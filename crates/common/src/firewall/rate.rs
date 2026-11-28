//! Rate limits: a rule with the `rate_limit` action counts each request it matches for the client
//! its `keys` name, and acts once the client goes over `limit` requests in `window` seconds.
//!
//! The shape is Vercel's (`action.mitigate.rateLimit {algo, window, limit, keys, action}`), with
//! Vercel's bounds: windows of 10 s to 1 h, limits of 1 to 10,000,000, `fixed_window` and
//! `token_bucket`. Keys are Vercel's `ip`, `ja4` and `header:<name>`, plus `user_agent`,
//! `asn`, `cookie:<name>`, `query:<name>`, `path`, `host` and `country`; several keys make one
//! bucket per combination of their values, and a missing value counts as empty (every request
//! without the header shares one bucket). An IPv6 client is counted by its /64, which is what one
//! subscriber usually gets.
//!
//! Counting is the router's (`Limiter`); this module says what to count and how a bucket moves
//! (`Bucket`), so the router stores 16 bytes a bucket and the arithmetic is tested here.
//! - Fixed window: windows start at multiples of `window` since the Unix epoch; the `limit + 1`th
//!   request in one is over, until the next window starts.
//! - Token bucket: a bucket holds up to `limit` tokens and refills at `limit` per `window`; a
//!   request takes one, and is over when none is left. A refused request takes nothing.

use std::net::IpAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{Facts, Request};

/// Vercel's bounds.
pub const MIN_WINDOW_S: u64 = 10;
pub const MAX_WINDOW_S: u64 = 3600;
pub const MAX_LIMIT: u64 = 10_000_000;
const MAX_KEYS: usize = 8;
/// Bytes of one key's value counted; longer values are cut (a header can be 64 KB).
const MAX_KEY_VALUE: usize = 512;

/// A rule's `rateLimit`, as written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    pub algo: Algo,
    /// Seconds.
    pub window: u64,
    /// Requests allowed per window.
    pub limit: u64,
    /// What makes requests one client's: `ip`, `header:x-api-key`...
    pub keys: Vec<String>,
    /// What a request over the limit gets; `rate_limit` (429) when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<OverAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Algo {
    FixedWindow,
    TokenBucket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverAction {
    /// 429.
    RateLimit,
    /// 403.
    Deny,
    /// Recorded; the request goes on.
    Log,
    /// The challenge page, unless the client has solved one.
    Challenge,
}

/// An app asking the firewall about a rate limit: the id its rules name, and the client's key.
#[derive(Debug, Clone, Copy)]
pub struct ApiCheck<'a> {
    pub id: &'a str,
    pub key: &'a str,
}

/// How a rule counts, for the `Limiter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSpec {
    pub algo: Algo,
    pub window_ns: u64,
    pub limit: u64,
}

/// What counting one request said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub over: bool,
    /// When the client may try again (for `retry-after`); zero when not over.
    pub retry_after: Duration,
}

impl Hit {
    pub const UNDER: Hit = Hit { over: false, retry_after: Duration::ZERO };
}

/// Where rate limits are counted.
pub trait Limiter {
    /// Count one request of `client` (its keys' values) for rule `rule`.
    fn hit(&self, rule: &str, spec: RateSpec, client: &[u8]) -> Hit;
}

/// Counts nothing: every request is under every limit (the CLI's `test`).
pub struct NoLimits;

impl Limiter for NoLimits {
    fn hit(&self, _: &str, _: RateSpec, _: &[u8]) -> Hit {
        Hit::UNDER
    }
}

/// One client's count for one rule: 16 bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bucket {
    /// Fixed window: the window's number since the epoch. Token bucket: when tokens were last
    /// added, in Unix nanoseconds.
    mark: u64,
    /// Fixed window: requests in the window. Token bucket: tokens left.
    level: u32,
    /// Unix seconds after which the bucket is as good as new and can be dropped.
    pub expires_s: u32,
}

impl Bucket {
    /// Count a request at `now_ns` (Unix nanoseconds). `fresh` for a bucket just made.
    pub fn hit(&mut self, fresh: bool, spec: RateSpec, now_ns: u64) -> Hit {
        let limit = spec.limit.min(u32::MAX as u64) as u32;
        let window = spec.window_ns.max(1);
        match spec.algo {
            Algo::FixedWindow => {
                let n = now_ns / window;
                if fresh || self.mark != n {
                    self.mark = n;
                    self.level = 0;
                }
                self.level = self.level.saturating_add(1);
                let end = (n + 1).saturating_mul(window);
                self.expires_s = seconds_up(end);
                let over = self.level > limit;
                Hit { over, retry_after: if over { Duration::from_nanos(end - now_ns) } else { Duration::ZERO } }
            }
            Algo::TokenBucket => {
                // Nanoseconds a token takes to come back: at least 1,000 (10 s / 10,000,000).
                let per = (window / limit.max(1) as u64).max(1);
                if fresh {
                    self.level = limit;
                    self.mark = now_ns;
                } else if now_ns > self.mark {
                    let add = (now_ns - self.mark) / per;
                    if add > 0 {
                        let level = (self.level as u64).saturating_add(add);
                        if level >= limit as u64 {
                            self.level = limit;
                            self.mark = now_ns;
                        } else {
                            self.level = level as u32;
                            self.mark += add * per;
                        }
                    }
                }
                let hit = if self.level >= 1 {
                    self.level -= 1;
                    Hit::UNDER
                } else {
                    let next = (self.mark + per).saturating_sub(now_ns).max(1);
                    Hit { over: true, retry_after: Duration::from_nanos(next) }
                };
                let missing = (limit - self.level) as u64;
                self.expires_s = seconds_up(self.mark.saturating_add(missing.saturating_mul(per)));
                hit
            }
        }
    }
}

fn seconds_up(ns: u64) -> u32 {
    ns.div_ceil(1_000_000_000).min(u32::MAX as u64) as u32
}

// ---- Compiled ----

#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    Ip,
    Ja4,
    Header(http::HeaderName),
    Asn,
    Cookie(String),
    Query(String),
    Path,
    Host,
    Country,
}

impl Key {
    fn parse(text: &str) -> Result<Key, String> {
        let t = text.trim().to_ascii_lowercase();
        // A cookie's or query parameter's name as written: apps read them case-sensitively.
        let named = |prefix: &str| -> Result<Option<String>, String> {
            if !t.starts_with(prefix) {
                return Ok(None);
            }
            let name = text.trim()[prefix.len()..].trim();
            if name.is_empty() || name.len() > 256 {
                return Err(format!("rate-limit key {text:?}: name what to count, as {prefix}<name>"));
            }
            Ok(Some(name.to_owned()))
        };
        if let Some(name) = named("header:")? {
            return http::HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()).map(Key::Header).map_err(|_| format!("rate-limit key {text:?}: not a header name"));
        }
        if let Some(name) = named("cookie:")? {
            return Ok(Key::Cookie(name));
        }
        if let Some(name) = named("query:")? {
            return Ok(Key::Query(name));
        }
        Ok(match t.as_str() {
            "ip" => Key::Ip,
            "user_agent" | "user-agent" => Key::Header(http::header::USER_AGENT),
            "asn" => Key::Asn,
            "path" => Key::Path,
            "host" => Key::Host,
            "country" => Key::Country,
            "ja4" => Key::Ja4,
            _ => {
                return Err(format!(
                    "rate-limit key {text:?}: use ip, ja4, header:<name>, user_agent, asn, cookie:<name>, query:<name>, path, host or country"
                ));
            }
        })
    }

    /// This key's value for `req`, appended to `out`.
    fn value(&self, req: &Request, facts: &Facts, out: &mut Vec<u8>) {
        let mut push = |bytes: &[u8]| out.extend_from_slice(&bytes[..bytes.len().min(MAX_KEY_VALUE)]);
        match self {
            Key::Ip => match req.ip.to_canonical() {
                IpAddr::V4(a) => push(&a.octets()),
                IpAddr::V6(a) => push(&a.octets()[..8]),
            },
            // One value for each, the first, as apps read it (`URLSearchParams.get`, a cookie
            // parser, Node's single headers): a second copy, or another case of the name, beside
            // the one the app reads isn't a new client to count.
            Key::Header(name) => {
                if let Some(v) = req.headers.get(name) {
                    push(v.as_bytes());
                }
            }
            Key::Ja4 => push(req.ja4.unwrap_or("").as_bytes()),
            Key::Asn => {
                if let Some(n) = req.asn {
                    push(&n.to_be_bytes());
                }
            }
            Key::Cookie(name) => {
                if let Some((_, v)) = facts.cookies(req).iter().find(|(k, _)| k == name) {
                    push(v.as_bytes());
                }
            }
            Key::Query(name) => {
                if let Some(v) = facts.query(req).iter().find(|(k, _)| k == name).and_then(|(_, vs)| vs.first()) {
                    push(v.as_bytes());
                }
            }
            Key::Path => push(facts.path(req).as_bytes()),
            Key::Host => push(req.host.as_bytes()),
            Key::Country => push(req.country.unwrap_or("").as_bytes()),
        }
    }
}

/// A rate limit ready to count.
#[derive(Debug)]
pub(super) struct RateRule {
    pub(super) spec: RateSpec,
    keys: Vec<Key>,
    pub(super) over: OverAction,
    pub(super) persist: Option<Duration>,
}

impl RateRule {
    pub(super) fn new(limit: &RateLimit, persist: Option<Duration>) -> Result<Self, String> {
        if !(MIN_WINDOW_S..=MAX_WINDOW_S).contains(&limit.window) {
            return Err(format!("a rate-limit window of {MIN_WINDOW_S}-{MAX_WINDOW_S} seconds"));
        }
        if !(1..=MAX_LIMIT).contains(&limit.limit) {
            return Err(format!("a rate limit of 1-{MAX_LIMIT} requests"));
        }
        if limit.keys.is_empty() || limit.keys.len() > MAX_KEYS {
            return Err(format!("1-{MAX_KEYS} rate-limit keys (ip counts each address)"));
        }
        let mut keys = Vec::new();
        for k in &limit.keys {
            let key = Key::parse(k)?;
            if keys.contains(&key) {
                return Err(format!("rate-limit key {k:?} is there twice"));
            }
            keys.push(key);
        }
        let over = limit.action.unwrap_or(OverAction::RateLimit);
        match over {
            OverAction::Log if persist.is_some() => return Err("a rate limit that only logs can't hold a client (actionDuration)".into()),
            _ => {}
        }
        Ok(Self { spec: RateSpec { algo: limit.algo, window_ns: limit.window * 1_000_000_000, limit: limit.limit }, keys, over, persist })
    }

    /// The client's identity under this rule's keys.
    pub(super) fn client(&self, req: &Request, facts: &Facts) -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        for key in &self.keys {
            key.value(req, facts, &mut out);
            // A separator no value can fake: values are cut to MAX_KEY_VALUE, and the length goes in.
            let len = out.len() as u32;
            out.extend_from_slice(&[0xff]);
            out.extend_from_slice(&len.to_be_bytes());
        }
        out
    }
}
