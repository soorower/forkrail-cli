//! The response cache: functions' answers kept by the router and served again without running the
//! function, as Vercel does in two ways (docs/c16-frameworks.md, F4 and F4b):
//!
//! - **Pages**: a Build Output's prerenders (Vercel's ISR, `forkrail_common::rules::prerender`).
//!   Kept until they expire, then served once more (`STALE`) while the function makes the page
//!   again in the background, one regeneration at a time; made again at once by a request with
//!   `x-prerender-revalidate` and the prerender's token (`REVALIDATED`); never served from or
//!   kept for a request with the Draft Mode cookie (`BYPASS`). Every router keeps the same pages,
//!   on its own disk (`disk.rs`) with the most used in memory, and the home region's router makes
//!   them for all: the others ask it for a page they haven't got, tell it of one gone stale, send
//!   it on-demand revalidations, and get every page it makes pushed to them (`peers.rs`). A
//!   router that can't reach home makes its own until it can.
//! - **CDN answers**: any function's answer that asks for it in `Vercel-CDN-Cache-Control`,
//!   `CDN-Cache-Control` (`max-age` or `s-maxage`) or `Cache-Control` (`s-maxage`), in that order
//!   of precedence. Kept in each router's memory only, as Vercel's CDN keeps them in each region
//!   ("minutes to hours"), for their time, then served stale for their `stale-while-revalidate`
//!   while one request refreshes them, else made again while the client waits.
//!
//! Vercel's rules, from its CDN cache, cache-control and ISR pages (read 2026-11-16) and what its
//! CDN did for forkrail-check-fw-sveltekit that day:
//! - Cacheable: GET and HEAD, without `Authorization` (`BYPASS`), answers 200, 301, 302, 307, 308,
//!   404, 410 of up to 10 MB without `set-cookie`, `private`, `no-cache`, `no-store` or a `Vary` of
//!   `*` or `Cookie`. Pages of up to 20 MB with those statuses, whatever their headers say.
//! - The key: method, host, deployment, path and query string as sent (order counts, cookies
//!   don't; seen), `Accept` and `Accept-Encoding`, and the headers the answer's `Vary` names.
//!   A page's: host, deployment, function and its `allowQuery` parameters (sorted).
//! - What the client gets: the stored answer with `age` (seconds since it was made) and
//!   `x-vercel-cache`; `Vercel-CDN-Cache-Control` never; with only `Cache-Control`, its
//!   `s-maxage`, `stale-while-revalidate` and `stale-if-error` taken off; and an answer that sets
//!   no caching gets `public, max-age=0, must-revalidate`, as does every function answer on Vercel,
//!   with `age: 0` and `x-vercel-cache: MISS` (`Defaults`).
//! - A page whose regeneration fails (another status, or no answer) stays as it was and is tried
//!   again after 30 s.
//!
//! Memory: CDN answers `FORKRAIL_RESPONSE_CACHE_MB` (default 64), pages `FORKRAIL_PAGES_MEMORY_MB`
//! (default 32), each least recently used out first; an answer larger than an eighth of its share
//! isn't kept in memory (a page still is on disk).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use hyper::header::{ACCEPT, ACCEPT_ENCODING, AGE, CACHE_CONTROL, COOKIE, HeaderMap, HeaderName, HeaderValue, SET_COOKIE, VARY};
use hyper::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tracing::warn;

mod disk;
pub mod next;
pub mod peers;
pub mod tags;

pub use disk::{Disk, SWEEP_EVERY};
pub use peers::Link;
pub use tags::{TagMark, TagMarks, Verdict};

/// Where pages are kept on a router (systemd's `StateDirectory=forkrail/pages`).
pub const DEFAULT_PAGES_DIR: &str = "/var/lib/forkrail/pages";

/// What the client is told about the cache, with Vercel's values (apps and Vercel's own examples
/// read it: on-demand revalidation succeeds only on `REVALIDATED`).
pub const STATUS_HEADER: HeaderName = HeaderName::from_static("x-vercel-cache");
const VERCEL_CDN_CACHE_CONTROL: HeaderName = HeaderName::from_static("vercel-cdn-cache-control");
const CDN_CACHE_CONTROL: HeaderName = HeaderName::from_static("cdn-cache-control");
/// An answer's cache tags (comma-separated), which Vercel reads and never passes on: kept with
/// the answer, which purges by tag read (`tags.rs`).
pub const CACHE_TAG: HeaderName = HeaderName::from_static(forkrail_common::purge::CACHE_TAG_HEADER);
/// What Vercel answers when a function sets no caching: neither the CDN nor the browser keeps it.
pub const DEFAULT_CACHE_CONTROL: &str = "public, max-age=0, must-revalidate";
/// The largest CDN answer kept, and the largest page (Vercel's `FALLBACK_BODY_TOO_LARGE`).
pub const MAX_CDN_BYTES: usize = 10 << 20;
pub const MAX_PAGE_BYTES: usize = 20 << 20;
/// The longest any answer is kept for, in seconds (a year, as Vercel's limit).
const MAX_TTL_S: u64 = 31_536_000;
/// After a failed regeneration, the stale page is served as it is for this long before trying again.
const RETRY_FAILED_MS: u64 = 30_000;
/// Base keys whose `Vary` is remembered; past it, all are forgotten.
const MAX_VARY: usize = 10_000;
/// Answers larger than this share of memory are served but not kept there.
const MAX_MEMORY_SHARE: usize = 8;

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The hex SHA-256 of a cache key: its name in memory, on disk and between routers.
pub fn hash(key: &str) -> String {
    forkrail_common::hex(&Sha256::digest(key.as_bytes()))
}

/// `x-vercel-cache`'s values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Hit,
    Miss,
    Stale,
    /// A prerender's fallback page, made at build.
    Prerender,
    /// Made again now, on demand.
    Revalidated,
    /// Not looked up: Draft Mode, or `Authorization`.
    Bypass,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Hit => "HIT",
            Status::Miss => "MISS",
            Status::Stale => "STALE",
            Status::Prerender => "PRERENDER",
            Status::Revalidated => "REVALIDATED",
            Status::Bypass => "BYPASS",
        }
    }
}

/// A kept answer: the function's own status, headers and body, before the deployment's routes add
/// theirs (those are added each time it's served, as they are to a function's answer).
#[derive(Debug, Clone)]
pub struct Entry {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    /// When it was made, in Unix milliseconds.
    pub stored_ms: u64,
    /// Fresh until then; `None` until it's revalidated (a page with `expiration: false`).
    pub fresh_until: Option<u64>,
    /// Served stale until then while it's made again; `None` for good (a page).
    pub stale_until: Option<u64>,
    pub deployment: String,
    /// A page's group (`None` for CDN answers and ungrouped pages), and whether it's a page.
    pub group: Option<u64>,
    pub page: bool,
    /// A page this router made itself while the home region couldn't be reached: home's copy
    /// replaces it once there's a newer one (`peers.rs`).
    pub local: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    /// Served, and made again in the background.
    Stale,
    /// Too old to serve.
    Expired,
    /// Deleted by a purge (`tags.rs`): made again while the client waits, told `REVALIDATED`.
    Deleted,
}

/// The project and environment a kept entry's deployment is of: what purges by tag are of.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub project: &'a str,
    pub environment: &'a str,
}

impl Entry {
    /// Its age in whole seconds, as the `age` header says it.
    pub fn age(&self, now: u64) -> u64 {
        now.saturating_sub(self.stored_ms) / 1000
    }

    fn freshness(&self, now: u64, group_since: Option<u64>) -> Freshness {
        let outdated = group_since.is_some_and(|since| self.stored_ms < since);
        if !outdated && self.fresh_until.is_none_or(|until| now < until) {
            return Freshness::Fresh;
        }
        if self.stale_until.is_none_or(|until| now < until) { Freshness::Stale } else { Freshness::Expired }
    }

    fn size(&self) -> usize {
        self.body.len() + self.headers.iter().map(|(k, v)| k.as_str().len() + v.len() + 32).sum::<usize>() + 256
    }

    /// The same answer, fresh for `ms` more: a stale page whose regeneration failed.
    fn fresh_for(&self, ms: u64) -> Entry {
        Entry { fresh_until: Some(now_ms() + ms), ..self.clone() }
    }
}

// ---- Vercel's rules, as pure functions ----

/// How long a CDN answer is kept, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ttl {
    pub fresh: u64,
    pub stale_while_revalidate: u64,
}

impl Ttl {
    pub fn entry_times(&self, now: u64) -> (Option<u64>, Option<u64>) {
        let fresh = now + self.fresh * 1000;
        (Some(fresh), Some(fresh + self.stale_while_revalidate * 1000))
    }
}

/// One header's directives, lowercase names, with their values.
fn directives(value: &str) -> Vec<(String, Option<String>)> {
    value
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| match d.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), Some(v.trim().trim_matches('"').to_owned())),
            None => (d.to_ascii_lowercase(), None),
        })
        .collect()
}

fn joined(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    let values: Vec<&str> = headers.get_all(name).iter().filter_map(|v| v.to_str().ok()).collect();
    (!values.is_empty()).then(|| values.join(", "))
}

fn seconds(found: &[(String, Option<String>)], name: &str) -> Option<u64> {
    found.iter().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref()?.parse::<u64>().ok()).map(|s| s.min(MAX_TTL_S))
}

/// Whether an answer may be kept by the CDN, and for how long: by the first of
/// `Vercel-CDN-Cache-Control`, `CDN-Cache-Control` and `Cache-Control` it has. The targeted two
/// keep it for `s-maxage` or `max-age`; `Cache-Control` only for `s-maxage` (`max-age` is the
/// browser's). `private`, `no-cache` and `no-store` keep nothing.
pub fn cdn_ttl(headers: &HeaderMap) -> Option<Ttl> {
    let (value, targeted) = [(VERCEL_CDN_CACHE_CONTROL, true), (CDN_CACHE_CONTROL, true), (CACHE_CONTROL, false)]
        .into_iter()
        .find_map(|(name, targeted)| joined(headers, &name).map(|v| (v, targeted)))?;
    let found = directives(&value);
    if found.iter().any(|(k, _)| matches!(k.as_str(), "private" | "no-cache" | "no-store")) {
        return None;
    }
    let fresh = seconds(&found, "s-maxage").or_else(|| targeted.then(|| seconds(&found, "max-age")).flatten()).filter(|&s| s > 0)?;
    Some(Ttl { fresh, stale_while_revalidate: seconds(&found, "stale-while-revalidate").unwrap_or(0) })
}

/// The statuses Vercel's CDN keeps.
pub fn cdn_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 200 | 301 | 302 | 307 | 308 | 404 | 410)
}

/// The statuses that make a page (others are a failed regeneration: Vercel's ISR page lists the
/// same seven).
pub fn page_status(status: StatusCode) -> bool {
    cdn_status(status)
}

/// The request headers an answer's `Vary` names, or `None` if it can't be kept: `*`, or a header
/// that differs for every visitor.
pub fn vary(headers: &HeaderMap) -> Option<Vec<HeaderName>> {
    let mut names = Vec::new();
    for value in headers.get_all(VARY).iter().filter_map(|v| v.to_str().ok()) {
        for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            let name = name.to_ascii_lowercase();
            if matches!(name.as_str(), "*" | "cookie" | "authorization") {
                return None;
            }
            // Already in every key.
            if matches!(name.as_str(), "accept" | "accept-encoding") {
                continue;
            }
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes())
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
    }
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Some(names)
}

/// The codings a client takes, as one word for the key: an answer compressed for one client is
/// only served to clients that take the same. Every coding it doesn't refuse counts, `*` and ones
/// the router doesn't know included (an app may answer `*` or `x-gzip` compressed): only the same
/// list shares a key with them, never a client that takes none.
pub fn encodings(headers: &HeaderMap) -> String {
    /// Enough for any real client; past it, the rest of the list is one word, so it still keys apart.
    const MAX_CODINGS: usize = 8;
    const MAX_CODING: usize = 32;
    let Some(value) = joined(headers, &ACCEPT_ENCODING) else { return String::new() };
    let mut out: Vec<String> = Vec::new();
    for part in value.split(',') {
        let mut bits = part.split(';');
        let coding = bits.next().unwrap_or("").trim().to_ascii_lowercase();
        let refused = bits.any(|b| b.trim().strip_prefix("q=").is_some_and(|q| q.trim().parse::<f32>().is_ok_and(|q| q == 0.0)));
        if refused || coding.is_empty() {
            continue;
        }
        let coding = if out.len() == MAX_CODINGS || coding.len() > MAX_CODING { format!("~{}", hash(&coding)[..16].to_owned()) } else { coding };
        if !out.contains(&coding) {
            out.push(coding);
        }
    }
    out.sort_unstable();
    out.join(",")
}

/// A CDN answer's key, before the headers its `Vary` names.
pub fn cdn_key(deployment: &str, method: &Method, host: &str, path_and_query: &str, headers: &HeaderMap) -> String {
    let accept = joined(headers, &ACCEPT).unwrap_or_default();
    format!("cdn\n{deployment}\n{method}\n{host}\n{path_and_query}\n{}\n{accept}", encodings(headers))
}

/// A CDN key with the values of the request headers named.
pub fn with_vary(base: &str, names: &[HeaderName], headers: &HeaderMap) -> String {
    let mut key = base.to_owned();
    for name in names {
        key.push('\n');
        key.push_str(name.as_str());
        key.push('=');
        key.push_str(&joined(headers, name).unwrap_or_default());
    }
    key
}

/// A page's key: host and deployment (Vercel keeps pages per deployment and domain), function, and
/// its `allowQuery` parameters (`Prerender::key_query`).
pub fn page_key(deployment: &str, host: &str, function: &str, key_query: &str) -> String {
    format!("page\n{deployment}\n{host}\n{function}\n{key_query}")
}

/// A cookie's value in the request.
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get_all(COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k.trim() == name).then(|| v.trim())
    })
}

/// Whether `token` is the one whose SHA-256 is `hash` (hex), compared in constant time.
pub fn token_matches(hash: &str, token: &str) -> bool {
    let given = forkrail_common::hex(&Sha256::digest(token.as_bytes()));
    given.len() == hash.len() && given.bytes().zip(hash.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// The headers a client gets on any function answer (Vercel's CDN does the same): no
/// `Vercel-CDN-Cache-Control`; `Cache-Control` without the CDN's directives when it's the only
/// caching header; and with `defaults`, `public, max-age=0, must-revalidate` when nothing's left.
pub fn client_headers(headers: &mut HeaderMap, defaults: bool) {
    let targeted = headers.contains_key(&VERCEL_CDN_CACHE_CONTROL) || headers.contains_key(&CDN_CACHE_CONTROL);
    platform_headers(headers);
    if !targeted && let Some(value) = joined(headers, &CACHE_CONTROL) {
        let kept: Vec<&str> = value
            .split(',')
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .filter(|d| {
                let name = d.split('=').next().unwrap_or("").trim().to_ascii_lowercase();
                !matches!(name.as_str(), "s-maxage" | "stale-while-revalidate" | "stale-if-error")
            })
            .collect();
        if kept.len() != directives(&value).len() || headers.get_all(CACHE_CONTROL).iter().count() > 1 {
            headers.remove(CACHE_CONTROL);
            if !kept.is_empty()
                && let Ok(v) = HeaderValue::from_str(&kept.join(", "))
            {
                headers.insert(CACHE_CONTROL, v);
            }
        }
    }
    if defaults && !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static(DEFAULT_CACHE_CONTROL));
    }
}

/// The headers a client gets on an answer to a request that couldn't be cached (not GET or HEAD,
/// or with `Authorization`): as Vercel sends them, nothing taken off and no `age`; with
/// `defaults`, the default `Cache-Control` when there's none, and `x-vercel-cache`.
pub fn uncacheable_headers(headers: &mut HeaderMap, status: Status, defaults: bool) {
    platform_headers(headers);
    if defaults && !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static(DEFAULT_CACHE_CONTROL));
    }
    if defaults || status != Status::Miss {
        headers.insert(STATUS_HEADER, HeaderValue::from_static(status.as_str()));
    }
}

/// Headers only the platform reads, taken off every answer a client gets (Vercel's CDN does, seen:
/// Astro's `vercel-cache-tag`): `Vercel-CDN-Cache-Control` and the cache tags.
pub fn platform_headers(headers: &mut HeaderMap) {
    headers.remove(&VERCEL_CDN_CACHE_CONTROL);
    headers.remove(&CACHE_TAG);
}

/// `age` and `x-vercel-cache`, on an answer from the cache or a function.
pub fn mark(headers: &mut HeaderMap, status: Status, age_s: u64) {
    headers.insert(AGE, HeaderValue::from(age_s));
    headers.insert(STATUS_HEADER, HeaderValue::from_static(status.as_str()));
}

/// Whether an answer can be kept at all by its own headers (pages are kept whatever their caching
/// headers say, but never with `set-cookie`: one visitor's session would go to everyone).
pub fn sets_cookie(headers: &HeaderMap) -> bool {
    headers.contains_key(SET_COOKIE)
}

// ---- Memory ----

/// Entries by hash, least recently used out first, within `max` bytes.
struct Memory {
    max: usize,
    used: usize,
    tick: u64,
    entries: HashMap<String, (Arc<Entry>, u64, usize)>,
    order: BTreeMap<u64, String>,
}

impl Memory {
    fn new(max: usize) -> Self {
        Self { max, used: 0, tick: 0, entries: HashMap::new(), order: BTreeMap::new() }
    }

    fn get(&mut self, hash: &str) -> Option<Arc<Entry>> {
        self.tick += 1;
        let tick = self.tick;
        let (entry, used, _) = self.entries.get_mut(hash)?;
        self.order.remove(used);
        *used = tick;
        self.order.insert(tick, hash.to_owned());
        Some(entry.clone())
    }

    /// Whether it was kept (too large is not).
    fn put(&mut self, hash: String, entry: Arc<Entry>) -> bool {
        let size = entry.size();
        if size > self.max / MAX_MEMORY_SHARE {
            self.remove(&hash);
            return false;
        }
        self.remove(&hash);
        self.tick += 1;
        self.used += size;
        self.order.insert(self.tick, hash.clone());
        self.entries.insert(hash, (entry, self.tick, size));
        while self.used > self.max {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            if let Some((_, _, size)) = self.entries.remove(&oldest) {
                self.used -= size;
            }
        }
        true
    }

    /// Without counting as a use.
    fn peek(&self, hash: &str) -> Option<Arc<Entry>> {
        self.entries.get(hash).map(|(entry, _, _)| entry.clone())
    }

    fn remove(&mut self, hash: &str) -> Option<Arc<Entry>> {
        let (entry, used, size) = self.entries.remove(hash)?;
        self.order.remove(&used);
        self.used -= size;
        Some(entry)
    }
}

// ---- Pages as bytes: on disk and between routers ----

const MAGIC: &[u8] = b"forkrail-response 1\n";

/// A page's line of JSON in its encoding.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Meta {
    pub status: u16,
    pub stored_ms: u64,
    #[serde(default)]
    pub fresh_until: Option<u64>,
    #[serde(default)]
    pub group: Option<u64>,
    #[serde(default)]
    pub deployment: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
}

/// Headers as `name\0value\n` (which header names and values can't contain), then a blank line.
pub fn put_headers(headers: &HeaderMap, out: &mut Vec<u8>) {
    for (name, value) in headers {
        out.extend_from_slice(name.as_str().as_bytes());
        out.push(0);
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// Headers as `put_headers` wrote them from `pos`, and where they end; `None` until they're all
/// there (a streamed page's head comes in pieces) or if they're not headers.
pub fn take_headers(bytes: &[u8], mut pos: usize) -> Option<(HeaderMap, usize)> {
    let mut headers = HeaderMap::new();
    loop {
        let line_end = bytes.get(pos..)?.iter().position(|&b| b == b'\n')? + pos;
        if line_end == pos {
            return Some((headers, pos + 1));
        }
        let line = &bytes[pos..line_end];
        let zero = line.iter().position(|&b| b == 0)?;
        headers.append(HeaderName::from_bytes(&line[..zero]).ok()?, HeaderValue::from_bytes(&line[zero + 1..]).ok()?);
        pos = line_end + 1;
    }
}

/// A page as stored and sent: a line naming the format, a line of JSON (`Meta`), the headers
/// (`put_headers`), then the body.
pub fn encode(entry: &Entry, hash: &str) -> Vec<u8> {
    let meta = Meta {
        status: entry.status.as_u16(),
        stored_ms: entry.stored_ms,
        fresh_until: entry.fresh_until,
        group: entry.group,
        deployment: entry.deployment.clone(),
        hash: hash.to_owned(),
        local: entry.local,
    };
    let mut out = Vec::with_capacity(entry.body.len() + 512);
    out.extend_from_slice(&encode_head(&meta, &entry.headers));
    out.extend_from_slice(&entry.body);
    out
}

/// Everything of an encoded page before its body.
pub fn encode_head(meta: &Meta, headers: &HeaderMap) -> Vec<u8> {
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&serde_json::to_vec(meta).expect("plain fields"));
    out.push(b'\n');
    put_headers(headers, &mut out);
    out
}

/// An encoded page's meta alone, from its first bytes (its headers may go on past them).
pub fn decode_meta(bytes: &[u8]) -> Option<Meta> {
    let rest = bytes.strip_prefix(MAGIC)?;
    let at = rest.iter().position(|&b| b == b'\n')?;
    serde_json::from_slice(&rest[..at]).ok()
}

/// An encoded page's meta and headers, and where its body starts; `None` until they're all there.
pub fn decode_head(bytes: &[u8]) -> Option<(Meta, HeaderMap, usize)> {
    let rest = bytes.strip_prefix(MAGIC)?;
    let at = rest.iter().position(|&b| b == b'\n')?;
    let meta: Meta = serde_json::from_slice(&rest[..at]).ok()?;
    let (headers, body) = take_headers(bytes, MAGIC.len() + at + 1)?;
    Some((meta, headers, body))
}

/// A page and its hash, from its encoding.
pub fn decode(bytes: Bytes) -> Option<(String, Entry)> {
    let (meta, headers, at) = decode_head(&bytes)?;
    let entry = Entry {
        status: StatusCode::from_u16(meta.status).ok()?,
        headers,
        body: bytes.slice(at..),
        stored_ms: meta.stored_ms,
        fresh_until: meta.fresh_until,
        stale_until: None,
        deployment: meta.deployment,
        group: meta.group,
        page: true,
        local: meta.local,
    };
    Some((meta.hash, entry))
}

// ---- One render at a time ----

/// A render or regeneration in progress, by hash: the first request makes it, others wait for it
/// (Vercel's request collapsing) or, for a stale page, don't start another.
type Flights = Arc<Mutex<HashMap<String, watch::Receiver<bool>>>>;

pub enum Flight {
    Leader(FlightGuard),
    /// Another request is making it; this one ends when it's done (or gone).
    Follower(watch::Receiver<bool>),
}

/// Held by the request making a page; others waiting on it go on when it's dropped.
pub struct FlightGuard {
    flights: Flights,
    hash: String,
    done: watch::Sender<bool>,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.flights.lock().unwrap().remove(&self.hash);
        let _ = self.done.send(true);
    }
}

// ---- The cache ----

/// Told of a page kept (its name, the page), and of an entry another router is making again
/// (its name, until when): `next.rs`'s.
type KeptWatcher = Box<dyn Fn(&str, &Arc<Entry>) + Send + Sync>;
type LeaseWatcher = Box<dyn Fn(&str, u64) + Send + Sync>;

pub struct ResponseCache {
    /// CDN answers, in this router's memory only (Vercel's CDN cache is each region's own).
    memory: Mutex<Memory>,
    /// The pages most recently served, decoded, in front of the disk.
    hot: Mutex<Memory>,
    disk: Option<Arc<Disk>>,
    /// Pages being written to disk, served from here meanwhile (one too large for `hot`, say).
    writing: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
    flights: Flights,
    /// When each (deployment, group) was last revalidated on demand: its pages made before then
    /// are stale. Kept with the store, and pushed from home with the pages.
    groups: Mutex<HashMap<(String, u64), u64>>,
    /// The `Vary` names last seen for each CDN base key.
    varies: Mutex<HashMap<String, Arc<[HeaderName]>>>,
    /// The other routers (`peers.rs`); `None` for a router alone.
    pub link: Option<Arc<Link>>,
    lost: Mutex<Option<disk::Lost>>,
    /// Vercel's headers on every function answer (`client_headers`, `mark`): `FORKRAIL_CACHE_DEFAULTS`.
    pub defaults: bool,
    /// What's been purged by tag (`tags.rs`).
    pub tags: Arc<TagMarks>,
    /// Told of every page kept here, wherever it came from (`next.rs` tells Next.js's workers).
    kept: std::sync::OnceLock<KeptWatcher>,
    /// Told when another router says one of its workers is making a stale Next.js entry again.
    leased: std::sync::OnceLock<LeaseWatcher>,
}

impl ResponseCache {
    pub fn new(memory_bytes: usize, page_memory_bytes: usize, disk: Option<Disk>, link: Option<Arc<Link>>, defaults: bool) -> Self {
        let state = disk.as_ref().map(Disk::state).unwrap_or_default();
        let tags = TagMarks::open(disk.as_ref().map(|d| d.root().to_path_buf()));
        Self {
            memory: Mutex::new(Memory::new(memory_bytes)),
            hot: Mutex::new(Memory::new(page_memory_bytes)),
            disk: disk.map(Arc::new),
            writing: Arc::default(),
            flights: Arc::default(),
            groups: Mutex::new(state.groups.into_iter().map(|(d, g, t)| ((d, g), t)).collect()),
            varies: Mutex::default(),
            link,
            lost: Mutex::new(state.lost),
            defaults,
            tags,
            kept: std::sync::OnceLock::new(),
            leased: std::sync::OnceLock::new(),
        }
    }

    /// Call `watcher` when another router says an entry (by name) is being made again until a
    /// time (Unix ms). One, set once.
    pub fn watch_leases(&self, watcher: impl Fn(&str, u64) + Send + Sync + 'static) {
        let _ = self.leased.set(Box::new(watcher));
    }

    /// Another router's worker is making a stale Next.js entry again (`next.rs`): this router's
    /// readers are served it meanwhile, and (home) the other routers are told.
    pub fn lease_heard(&self, hash: &str, until_ms: u64, from: Option<&str>) {
        if let Some(watcher) = self.leased.get() {
            watcher(hash, until_ms);
        }
        if let Some(link) = &self.link {
            link.push_lease(hash, until_ms, from);
        }
    }

    /// Call `watcher` with every page kept from now on (its name and the page). One, set once.
    pub fn watch_kept(&self, watcher: impl Fn(&str, &Arc<Entry>) + Send + Sync + 'static) {
        let _ = self.kept.set(Box::new(watcher));
    }

    pub fn describe(&self) -> String {
        match &self.disk {
            Some(disk) => disk.root().display().to_string(),
            None => "memory only".to_owned(),
        }
    }

    pub fn disk(&self) -> Option<&Arc<Disk>> {
        self.disk.as_ref()
    }

    /// How many answers are in memory, and the bytes they're counted as.
    #[cfg(test)]
    pub fn memory_use(&self) -> (usize, usize) {
        let memory = self.memory.lock().unwrap();
        (memory.entries.len(), memory.used)
    }

    /// This router's link to home, when it's an edge and the link is up: its pages come from there.
    pub fn home(&self) -> Option<&Arc<Link>> {
        self.link.as_ref().filter(|link| link.up())
    }

    /// Whether this router makes pages for others too (home), rather than asking home for them.
    pub fn is_edge(&self) -> bool {
        self.link.as_ref().is_some_and(|link| !link.is_home())
    }

    /// A kept answer: a CDN answer from memory, a page from memory or disk.
    pub async fn get(&self, deployment: &str, hash: &str, page: bool) -> Option<Arc<Entry>> {
        if !page {
            return self.memory.lock().unwrap().get(hash);
        }
        // A page's hash has its deployment in it; a name from elsewhere never finds another's.
        let hot = self.hot.lock().unwrap().get(hash).filter(|e| e.deployment == deployment);
        if let Some(entry) = hot {
            if let Some(disk) = &self.disk {
                disk.used(hash);
            }
            return Some(entry);
        }
        if let Some(entry) = self.writing.lock().unwrap().get(hash).cloned() {
            return Some(entry);
        }
        let bytes = self.disk.as_ref()?.read(deployment, hash).await?;
        let (found, entry) = decode(bytes)?;
        if found != hash || entry.deployment != deployment {
            return None;
        }
        let entry = Arc::new(entry);
        self.hot.lock().unwrap().put(hash.to_owned(), entry.clone());
        Some(entry)
    }

    /// Whether a kept entry of `scope` can be served: by its own times, its group's on-demand
    /// revalidations, and the purges of its tags.
    pub fn freshness(&self, entry: &Entry, now: u64, scope: Scope) -> Freshness {
        let since = entry.group.and_then(|g| self.groups.lock().unwrap().get(&(entry.deployment.clone(), g)).copied());
        let own = entry.freshness(now, since);
        match self.tags.judge(scope.project, scope.environment, &entry.headers, entry.stored_ms, now) {
            None => own,
            Some(Verdict::Deleted) => Freshness::Deleted,
            Some(Verdict::Stale) if own == Freshness::Fresh => Freshness::Stale,
            Some(Verdict::Stale) => own,
        }
    }

    /// Marks of a purge made here, or heard from another router: kept, and (home) pushed to every
    /// other router but `from`. The ones that changed anything.
    pub fn take_marks(&self, marks: &[TagMark], from: Option<&str>) -> Vec<TagMark> {
        let changed = self.tags.merge_in(marks, now_ms());
        if !changed.is_empty()
            && let Some(link) = &self.link
        {
            link.push_tags(&changed, from);
        }
        changed
    }

    /// A function's purge (`tunnel.rs`), of its worker's project and environment: kept here, and
    /// sent to home from an edge (which pushes it to every router) or pushed from home. It's done
    /// when this router has it and home has said it has (or can't be reached: then it's sent
    /// when the link is back).
    pub async fn purge_from_function(&self, project: &str, environment: &str, asked: forkrail_common::purge::RuntimePurge) -> Result<usize, String> {
        use forkrail_common::purge::{self, Purge};
        let mut purge = Purge {
            op: asked.op,
            tags: asked.tags,
            src_images: asked.src_images,
            target: Some(purge::environment(environment).to_owned()),
            deadline_s: asked.deadline_s,
            at_ms: now_ms(),
            by: None,
        };
        purge.check(purge::MAX_TAGS_PER_RUNTIME_CALL)?;
        let marks = TagMarks::marks_of(project, &purge);
        self.take_marks(&marks, None);
        if self.is_edge()
            && let Some(link) = &self.link
        {
            link.send_tags(marks.clone()).await;
        }
        Ok(marks.len())
    }

    /// The routing table's purges (from the CLI and the dashboard): every router reads the table,
    /// so each keeps them itself.
    pub fn table_purges(&self, purges: &BTreeMap<String, Vec<forkrail_common::purge::Purge>>) -> usize {
        let marks: Vec<TagMark> = purges.iter().flat_map(|(project, list)| list.iter().flat_map(move |p| TagMarks::marks_of(project, p))).collect();
        self.tags.merge_in(&marks, now_ms()).len()
    }

    /// Keep an answer: a CDN answer in memory; a page in memory and on disk, and from home, pushed
    /// to every other router but `from` (whose request it answers, with the page in the answer).
    pub fn put(&self, hash: &str, entry: Entry, from: Option<&str>) -> Arc<Entry> {
        let entry = Arc::new(entry);
        if entry.page {
            self.keep_page(hash, entry.clone(), from);
        } else {
            self.memory.lock().unwrap().put(hash.to_owned(), entry.clone());
        }
        entry
    }

    fn keep_page(&self, hash: &str, entry: Arc<Entry>, from: Option<&str>) {
        self.hot.lock().unwrap().put(hash.to_owned(), entry.clone());
        if let Some(watcher) = self.kept.get() {
            watcher(hash, &entry);
        }
        let encoded = Bytes::from(encode(&entry, hash));
        if let Some(link) = &self.link {
            link.push_page(encoded.clone(), from);
        }
        let Some(disk) = self.disk.clone() else { return };
        self.writing.lock().unwrap().insert(hash.to_owned(), entry.clone());
        let writing = self.writing.clone();
        let hash = hash.to_owned();
        tokio::spawn(async move {
            // Only while it's the newest of this page being kept: two writes may finish out of order.
            let newest = || writing.lock().unwrap().get(&hash).is_some_and(|w| Arc::ptr_eq(w, &entry));
            if let Err(e) = disk.write(&entry.deployment, &hash, entry.stored_ms, entry.local, encoded, newest).await {
                warn!(deployment = %entry.deployment, error = %e, "keeping a page on disk failed; it's in memory only");
            }
            let mut writing = writing.lock().unwrap();
            if writing.get(&hash).is_some_and(|w| Arc::ptr_eq(w, &entry)) {
                writing.remove(&hash);
            }
        });
    }

    /// When the page this router has was made, and whether it made it itself without home.
    pub(crate) fn current(&self, hash: &str) -> Option<(u64, bool)> {
        let seen = |e: &Entry| (e.stored_ms, e.local);
        if let Some(entry) = self.hot.lock().unwrap().peek(hash) {
            return Some(seen(&entry));
        }
        if let Some(entry) = self.writing.lock().unwrap().get(hash) {
            return Some(seen(entry));
        }
        self.disk.as_ref()?.info(hash).map(|i| (i.stored_ms, i.local))
    }

    /// A page from home, kept unless this router has it already or one made later (its own,
    /// while home couldn't be reached). Whether it was kept.
    pub fn receive(&self, hash: &str, entry: Entry) -> bool {
        self.receive_from(hash, entry, None)
    }

    /// As `receive`, of a page the router `from` sent (an edge's Next.js worker wrote it): home
    /// pushes it to every other router but that one.
    pub fn receive_from(&self, hash: &str, entry: Entry, from: Option<&str>) -> bool {
        if self.current(hash).is_some_and(|(stored, _)| stored >= entry.stored_ms) {
            return false;
        }
        self.keep_page(hash, Arc::new(entry), from);
        true
    }

    /// A page is gone from disk (its scope's size limit, `next.rs`): out of memory too.
    pub(crate) fn forget(&self, hash: &str) {
        self.hot.lock().unwrap().remove(hash);
    }

    /// A stale page whose regeneration failed: served as it is for another 30 s.
    pub fn retry_later(&self, hash: &str, entry: &Entry) {
        self.hot.lock().unwrap().put(hash.to_owned(), Arc::new(entry.fresh_for(RETRY_FAILED_MS)));
    }

    /// Make it, or wait for the request making it.
    pub fn flight(&self, hash: &str) -> Flight {
        let mut flights = self.flights.lock().unwrap();
        if let Some(rx) = flights.get(hash) {
            return Flight::Follower(rx.clone());
        }
        let (done, rx) = watch::channel(false);
        flights.insert(hash.to_owned(), rx);
        Flight::Leader(FlightGuard { flights: self.flights.clone(), hash: hash.to_owned(), done })
    }

    /// Lead a regeneration, unless one is already under way.
    pub fn lead(&self, hash: &str) -> Option<FlightGuard> {
        match self.flight(hash) {
            Flight::Leader(guard) => Some(guard),
            Flight::Follower(_) => None,
        }
    }

    pub fn vary_of(&self, base: &str) -> Option<Arc<[HeaderName]>> {
        self.varies.lock().unwrap().get(base).cloned()
    }

    pub fn set_vary(&self, base: &str, names: Vec<HeaderName>) {
        let mut varies = self.varies.lock().unwrap();
        if varies.get(base).is_some_and(|known| **known == *names) {
            return;
        }
        if varies.len() >= MAX_VARY {
            varies.clear();
        }
        varies.insert(base.to_owned(), names.into());
    }

    /// A page was made again on demand: its group's other pages are stale from now, here and (from
    /// home) on every other router.
    pub fn revalidated(&self, deployment: &str, group: Option<u64>, stored_ms: u64) {
        let Some(group) = group else { return };
        self.apply_group(deployment, group, stored_ms);
        if let Some(link) = &self.link {
            link.push_group(deployment, group, stored_ms);
        }
    }

    /// A group revalidated on demand (here, or at home): its pages made before `ms` are stale.
    pub fn apply_group(&self, deployment: &str, group: u64, ms: u64) {
        let changed = {
            let mut groups = self.groups.lock().unwrap();
            let since = groups.entry((deployment.to_owned(), group)).or_default();
            let changed = ms > *since;
            *since = (*since).max(ms);
            changed
        };
        if changed && let Some(disk) = &self.disk {
            let groups = self.groups();
            disk.update_state(|s| s.groups = groups);
        }
    }

    pub fn groups(&self) -> Vec<(String, u64, u64)> {
        self.groups.lock().unwrap().iter().map(|((d, g), t)| (d.clone(), *g, *t)).collect()
    }

    /// Pages made after `since_ms`, oldest first, for another router (`peers.rs`).
    pub fn pages_since(&self, since_ms: u64) -> Vec<(String, String)> {
        match &self.disk {
            Some(disk) => disk.since(since_ms),
            None => {
                let hot = self.hot.lock().unwrap();
                let mut found: Vec<(u64, String, String)> =
                    hot.entries.iter().filter(|(_, (e, _, _))| e.stored_ms > since_ms && !e.local).map(|(h, (e, _, _))| (e.stored_ms, e.deployment.clone(), h.clone())).collect();
                found.sort_unstable();
                found.into_iter().map(|(_, d, h)| (d, h)).collect()
            }
        }
    }

    /// A page's encoding, for another router.
    pub async fn page_bytes(&self, deployment: &str, hash: &str) -> Option<Bytes> {
        if let Some(entry) = self.hot.lock().unwrap().peek(hash) {
            return Some(Bytes::from(encode(&entry, hash)));
        }
        self.disk.as_ref()?.read(deployment, hash).await
    }

    /// Whether this router's store is new: nothing it had before, if anything, survives.
    pub fn store_is_new(&self) -> bool {
        self.disk.as_ref().is_none_or(|d| d.is_new())
    }

    /// Whether a page of `deployment` this router has no copy of may have been made before every
    /// copy was lost: then its build's fallback may be older than what visitors last saw, and is
    /// served as stale once while the page is made (F4b).
    pub fn lost(&self, deployment: &str) -> bool {
        self.lost.lock().unwrap().as_ref().is_some_and(|l| l.deployments.iter().any(|d| d == deployment))
    }

    /// No router could give this one a copy of the pages: the deployments there are now may have
    /// had pages.
    pub fn mark_lost(&self, deployments: Vec<String>) {
        warn!(deployments = deployments.len(), "no copy of the pages could be had: their fallbacks are served as stale once while each is made");
        let lost = disk::Lost { at_ms: now_ms(), deployments };
        if let Some(disk) = &self.disk {
            let lost = lost.clone();
            disk.update_state(|s| s.lost = Some(lost));
        }
        *self.lost.lock().unwrap() = Some(lost);
    }

    /// Every page has been copied from home.
    pub fn clear_lost(&self) {
        if self.lost.lock().unwrap().take().is_some()
            && let Some(disk) = &self.disk
        {
            disk.update_state(|s| s.lost = None);
        }
    }

    /// Where an edge is in home's pages (`peers.rs`).
    pub fn home_since(&self) -> u64 {
        self.disk.as_ref().map_or(0, |d| d.state().home_since_ms)
    }

    pub fn set_home_since(&self, ms: u64) {
        if let Some(disk) = &self.disk {
            disk.update_state(|s| s.home_since_ms = s.home_since_ms.max(ms));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn which_header_keeps_an_answer_and_for_how_long() {
        // `max-age` alone is the browser's.
        assert_eq!(cdn_ttl(&headers(&[("cache-control", "public, max-age=60")])), None);
        assert_eq!(cdn_ttl(&headers(&[("cache-control", "public, max-age=0, s-maxage=60")])), Some(Ttl { fresh: 60, stale_while_revalidate: 0 }));
        assert_eq!(cdn_ttl(&headers(&[("cache-control", "s-maxage=1, stale-while-revalidate=59")])), Some(Ttl { fresh: 1, stale_while_revalidate: 59 }));
        // The targeted headers come first, and there `max-age` counts.
        let h = headers(&[("cache-control", "s-maxage=5"), ("cdn-cache-control", "max-age=60"), ("vercel-cdn-cache-control", "max-age=300")]);
        assert_eq!(cdn_ttl(&h).map(|t| t.fresh), Some(300));
        let h = headers(&[("cache-control", "s-maxage=5"), ("cdn-cache-control", "max-age=60")]);
        assert_eq!(cdn_ttl(&h).map(|t| t.fresh), Some(60));
        // A targeted header beats `private` in Cache-Control, but not its own.
        assert!(cdn_ttl(&headers(&[("cache-control", "private"), ("vercel-cdn-cache-control", "max-age=60")])).is_some());
        for no in ["private, s-maxage=60", "no-store, s-maxage=60", "no-cache=\"set-cookie\", s-maxage=60", "s-maxage=0"] {
            assert_eq!(cdn_ttl(&headers(&[("cache-control", no)])), None, "{no}");
        }
        // At most a year.
        assert_eq!(cdn_ttl(&headers(&[("cache-control", "s-maxage=99999999999")])).map(|t| t.fresh), Some(MAX_TTL_S));
    }

    #[test]
    fn what_the_client_gets() {
        // Seen on Vercel: `public, max-age=0, s-maxage=60` reached the browser as `public, max-age=0`.
        let mut h = headers(&[("cache-control", "public, max-age=0, s-maxage=60")]);
        client_headers(&mut h, true);
        assert_eq!(h["cache-control"], "public, max-age=0");
        let mut h = headers(&[("cache-control", "s-maxage=60, stale-while-revalidate=30, stale-if-error=5")]);
        client_headers(&mut h, true);
        assert_eq!(h["cache-control"], DEFAULT_CACHE_CONTROL);
        // With a targeted header, Cache-Control goes as it is; Vercel-CDN-Cache-Control never.
        let mut h = headers(&[("cache-control", "max-age=10, s-maxage=60"), ("vercel-cdn-cache-control", "max-age=60"), ("cdn-cache-control", "max-age=5")]);
        client_headers(&mut h, true);
        assert_eq!((h["cache-control"].to_str().unwrap(), h.get("vercel-cdn-cache-control"), h["cdn-cache-control"].to_str().unwrap()), ("max-age=10, s-maxage=60", None, "max-age=5"));
        // Something else stays as written.
        let mut h = headers(&[("cache-control", "private, no-store")]);
        client_headers(&mut h, true);
        assert_eq!(h["cache-control"], "private, no-store");
        // Cache tags are the platform's, on any answer (seen: Vercel never passes Astro's on).
        for keep in [true, false] {
            let mut h = headers(&[("vercel-cache-tag", "posts,post-1"), ("x-app", "1")]);
            if keep {
                client_headers(&mut h, true);
            } else {
                uncacheable_headers(&mut h, Status::Miss, true);
            }
            assert!(!h.contains_key("vercel-cache-tag") && h.contains_key("x-app"));
        }
    }

    /// Everything Vercel's defaults change on a function answer that sets no caching headers
    /// (`FORKRAIL_CACHE_DEFAULTS`): exactly three headers added, nothing else touched.
    #[test]
    fn defaults_on_an_answer_without_caching_headers() {
        let before = headers(&[("content-type", "text/html"), ("x-app", "1"), ("etag", "\"a\"")]);
        let mut after = before.clone();
        client_headers(&mut after, true);
        mark(&mut after, Status::Miss, 0);
        let added: Vec<(String, String)> = after.iter().filter(|(k, _)| !before.contains_key(*k)).map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned())).collect();
        assert_eq!(added, vec![("cache-control".into(), DEFAULT_CACHE_CONTROL.into()), ("age".into(), "0".into()), ("x-vercel-cache".into(), "MISS".into())]);
        assert!(before.iter().all(|(k, v)| after.get(k) == Some(v)));
        // Off: nothing added by the defaults.
        let mut off = before.clone();
        client_headers(&mut off, false);
        assert_eq!(off, before);
        // A request that can't be cached (a POST, `Authorization`): two, and no `age` (seen on Vercel).
        let mut post = before.clone();
        uncacheable_headers(&mut post, Status::Miss, true);
        let added: Vec<&str> = post.keys().filter(|k| !before.contains_key(*k)).map(|k| k.as_str()).collect();
        assert_eq!(added, ["cache-control", "x-vercel-cache"]);
        let mut bypass = headers(&[("cache-control", "public, max-age=0, s-maxage=60")]);
        uncacheable_headers(&mut bypass, Status::Bypass, true);
        assert_eq!((bypass["cache-control"].to_str().unwrap(), bypass["x-vercel-cache"].to_str().unwrap()), ("public, max-age=0, s-maxage=60", "BYPASS"));
    }

    /// Isolation check (2026-11-16): `Accept-Encoding: *` takes any coding, so an app may answer it
    /// compressed (Node's `compression` does); kept under the same key as no `Accept-Encoding` at
    /// all, that answer then reaches clients that take no coding (tools/isolation-check.py,
    /// "Accept-Encoding: *"). Un-ignore with the fix.
    #[test]
    fn an_answer_for_any_coding_is_kept_apart_from_one_for_none() {
        let none = encodings(&HeaderMap::new());
        let key = |v: &str| encodings(&headers(&[("accept-encoding", v)]));
        for any in ["*", "*;q=1", "identity, *", "x-gzip", "identity"] {
            assert_ne!(key(any), none, "{any:?} keys as no coding");
            assert_ne!(key(any), key("gzip"), "{any:?} keys as gzip");
        }
        assert_eq!(key("GZIP, Br"), key("br, gzip"), "names without case, in any order");
        assert_eq!(key("*"), key("*;q=0.5"));
        // Only a coding the client refuses outright stays out of its key.
        assert_eq!(key("gzip;q=0"), none);
        assert_eq!(key("gzip, *;q=0"), key("gzip"));
        // A long list or a long name still keys apart, in bounded words.
        let long = (0..40).map(|i| format!("c{i}")).collect::<Vec<_>>().join(", ");
        assert!(key(&long).len() < 200 && key(&long) != key("c0"));
    }

    /// Answers are kept per deployment and host: one project's answer never serves another's.
    #[test]
    fn keys_are_per_deployment_and_host() {
        let h = HeaderMap::new();
        let key = cdn_key("dpl_a", &Method::GET, "a.example", "/me", &h);
        assert_ne!(key, cdn_key("dpl_b", &Method::GET, "a.example", "/me", &h));
        assert_ne!(key, cdn_key("dpl_a", &Method::GET, "b.example", "/me", &h));
        assert_ne!(page_key("dpl_a", "a.example", "/fn", ""), page_key("dpl_b", "a.example", "/fn", ""));
        assert_ne!(page_key("dpl_a", "a.example", "/fn", ""), page_key("dpl_a", "b.example", "/fn", ""));
        // (Fields are joined by newlines, which no host, path or header value can hold.)
    }

    #[test]
    fn keys_and_vary() {
        let h = headers(&[("accept-encoding", "gzip, deflate, br;q=1, zstd;q=0"), ("accept", "text/html")]);
        assert_eq!(encodings(&h), "br,deflate,gzip");
        assert_eq!(encodings(&HeaderMap::new()), "");
        let a = cdn_key("d", &Method::GET, "h", "/x?a=1&b=2", &h);
        assert_ne!(a, cdn_key("d", &Method::GET, "h", "/x?b=2&a=1", &h), "order counts (seen on Vercel)");
        assert_ne!(a, cdn_key("d", &Method::HEAD, "h", "/x?a=1&b=2", &h), "HEAD has its own (seen)");
        assert_eq!(a, cdn_key("d", &Method::GET, "h", "/x?a=1&b=2", &headers(&[("accept-encoding", "br, gzip, deflate"), ("accept", "text/html"), ("cookie", "a=b")])));
        assert_eq!(vary(&headers(&[("vary", "Accept-Encoding, Origin"), ("vary", "x-a")])).unwrap(), vec![HeaderName::from_static("origin"), HeaderName::from_static("x-a")]);
        assert!(vary(&headers(&[("vary", "*")])).is_none() && vary(&headers(&[("vary", "Cookie")])).is_none());
        let base = cdn_key("d", &Method::GET, "h", "/", &HeaderMap::new());
        let names = [HeaderName::from_static("origin")];
        assert_ne!(with_vary(&base, &names, &headers(&[("origin", "a")])), with_vary(&base, &names, &headers(&[("origin", "b")])));
        assert_eq!(cookie(&headers(&[("cookie", "a=1; __prerender_bypass=tok ; b=2")]), "__prerender_bypass"), Some("tok"));
        let h = forkrail_common::hex(&Sha256::digest(b"tok"));
        assert!(token_matches(&h, "tok") && !token_matches(&h, "tok2") && !token_matches(&h, ""));
    }

    #[test]
    fn freshness_of_pages_and_cdn_answers() {
        let entry = |fresh: Option<u64>, stale: Option<u64>, group: Option<u64>| Entry {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            stored_ms: 1_000,
            fresh_until: fresh,
            stale_until: stale,
            deployment: "d".into(),
            group,
            page: stale.is_none(),
            local: false,
        };
        // A page: fresh for its expiration, then stale for good.
        let page = entry(Some(31_000), None, Some(1));
        assert_eq!(page.freshness(30_999, None), Freshness::Fresh);
        assert_eq!(page.freshness(31_000, None), Freshness::Stale);
        assert_eq!(page.freshness(9_999_999_999, None), Freshness::Stale);
        // Its group revalidated on demand after it was made.
        assert_eq!(page.freshness(2_000, Some(1_500)), Freshness::Stale);
        assert_eq!(page.freshness(2_000, Some(500)), Freshness::Fresh);
        // `expiration: false`.
        assert_eq!(entry(None, None, None).freshness(u64::MAX, None), Freshness::Fresh);
        // A CDN answer: s-maxage, then its stale-while-revalidate, then gone.
        let cdn = entry(Some(61_000), Some(91_000), None);
        assert_eq!((cdn.freshness(60_000, None), cdn.freshness(61_000, None), cdn.freshness(91_000, None)), (Freshness::Fresh, Freshness::Stale, Freshness::Expired));
        assert_eq!(page.age(31_999), 30);
    }

    #[test]
    fn entries_round_trip() {
        let entry = Entry {
            status: StatusCode::NOT_FOUND,
            headers: headers(&[("content-type", "text/html"), ("set-thing", "a"), ("set-thing", "b"), ("x-bin", "caf\u{e9}")]),
            body: Bytes::from_static(b"<p>\n\n\0body</p>"),
            stored_ms: 42,
            fresh_until: Some(99),
            stale_until: None,
            deployment: "dep-1".into(),
            group: Some(3),
            page: true,
            local: true,
        };
        let encoded = encode(&entry, "h1");
        let (hash, back) = decode(Bytes::from(encoded.clone())).unwrap();
        assert_eq!(hash, "h1");
        assert_eq!((back.status, &back.headers, &back.body, back.stored_ms, back.fresh_until, back.group, back.deployment.as_str(), back.local), (entry.status, &entry.headers, &entry.body, 42, Some(99), Some(3), "dep-1", true));
        assert!(decode(Bytes::from_static(b"something else")).is_none());
        // A streamed page's head is read once it has all come.
        let at = decode_head(&encoded).unwrap().2;
        assert!(decode_head(&encoded[..at - 2]).is_none());
        assert_eq!(&encoded[at..], b"<p>\n\n\0body</p>");
    }

    #[test]
    fn memory_keeps_the_recently_used_within_its_budget() {
        let entry = |n: usize| {
            Arc::new(Entry { status: StatusCode::OK, headers: HeaderMap::new(), body: Bytes::from(vec![0; n]), stored_ms: 0, fresh_until: None, stale_until: None, deployment: "d".into(), group: None, page: false, local: false })
        };
        let mut m = Memory::new(8 * 1400);
        assert!(!m.put("big".into(), entry(2000)), "over an eighth: not kept");
        for k in ["a", "b", "c", "d", "e", "f", "g"] {
            assert!(m.put(k.into(), entry(1000)));
        }
        m.get("a");
        for k in ["h", "i", "j"] {
            m.put(k.into(), entry(1000));
        }
        assert!(m.used <= m.max);
        assert!(m.get("a").is_some() && m.get("j").is_some());
        assert!(m.get("b").is_none(), "least recently used went first");
        assert!(m.remove("a").is_some() && m.get("a").is_none());
        assert_eq!(m.used, m.entries.values().map(|(_, _, s)| s).sum::<usize>());
    }

    #[tokio::test]
    async fn one_render_at_a_time() {
        let cache = ResponseCache::new(1 << 20, 1 << 20, None, None, true);
        let Flight::Leader(guard) = cache.flight("k") else { panic!("first leads") };
        let Flight::Follower(mut rx) = cache.flight("k") else { panic!("second follows") };
        assert!(cache.lead("k").is_none(), "no second regeneration");
        drop(guard);
        rx.wait_for(|done| *done).await.unwrap();
        assert!(matches!(cache.flight("k"), Flight::Leader(_)));
    }

    #[tokio::test]
    async fn pages_from_home_and_groups() {
        let dir = std::env::temp_dir().join(format!("forkrail-cache-{}", forkrail_common::random_hex(6)));
        let cache = ResponseCache::new(1 << 20, 1 << 20, Some(Disk::open(dir.clone(), 1 << 20, 0).unwrap()), None, true);
        assert!(cache.store_is_new());
        let page = |stored_ms: u64, group: Option<u64>, local: bool| Entry {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(format!("made at {stored_ms}")),
            stored_ms,
            fresh_until: None,
            stale_until: None,
            deployment: "dep".into(),
            group,
            page: true,
            local,
        };
        // Made here while home was away; home's older copy doesn't replace it, a newer one does.
        cache.put("h1", page(100, Some(2), true), None);
        assert!(!cache.receive("h1", page(90, Some(2), false)));
        assert!(cache.receive("h1", page(110, Some(2), false)));
        assert!(!cache.receive("h1", page(110, Some(2), false)), "once");
        let kept = cache.get("dep", "h1", true).await.unwrap();
        assert_eq!((kept.stored_ms, kept.local), (110, false));
        // On disk too, once written (the newer of the two writes, whichever finished first): found
        // again by a cache on the same store.
        for _ in 0..400 {
            if cache.writing.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(cache.disk().unwrap().info("h1").map(|i| i.stored_ms), Some(110));
        assert_eq!(cache.pages_since(0), [("dep".to_owned(), "h1".to_owned())]);
        let bytes = cache.page_bytes("dep", "h1").await.unwrap();
        assert_eq!(decode(bytes).unwrap().1.stored_ms, 110);
        // A group revalidated on demand: older pages of it stale, here and after a restart.
        let scope = Scope { project: "shop", environment: "production" };
        cache.apply_group("dep", 2, 120);
        assert_eq!(cache.freshness(&kept, 130, scope), Freshness::Stale);
        // Purged by a tag it has: deleted, here and after a restart (the marks beside the pages).
        let now = now_ms();
        let mut tagged = (*kept).clone();
        tagged.stored_ms = now - 10_000;
        tagged.headers.insert(CACHE_TAG, HeaderValue::from_static("post-1, posts"));
        let purge = forkrail_common::purge::Purge { op: forkrail_common::purge::PurgeOp::Delete, tags: vec!["posts".into()], src_images: false, target: None, deadline_s: None, at_ms: now - 5_000, by: None };
        cache.table_purges(&[("shop".to_owned(), vec![purge])].into());
        assert_eq!(cache.freshness(&tagged, now, scope), Freshness::Deleted);
        assert_eq!(cache.freshness(&tagged, now, Scope { project: "blog", ..scope }), Freshness::Fresh, "another project's");
        assert_eq!(cache.freshness(&kept, 130, scope), Freshness::Stale, "untagged: only its group");
        let again = ResponseCache::new(1 << 20, 1 << 20, Some(Disk::open(dir.clone(), 1 << 20, 0).unwrap()), None, true);
        assert!(!again.store_is_new());
        assert_eq!(again.groups(), [("dep".to_owned(), 2, 120)]);
        // The marks are written within a second.
        let mut kept_on_disk = false;
        for _ in 0..300 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let reopened = ResponseCache::new(1 << 20, 1 << 20, Some(Disk::open(dir.clone(), 1 << 20, 0).unwrap()), None, true);
            if reopened.tags.len() > 0 {
                assert_eq!(reopened.freshness(&tagged, now, scope), Freshness::Deleted);
                kept_on_disk = true;
                break;
            }
        }
        assert!(kept_on_disk, "purge marks kept on disk");
        let from_disk = again.get("dep", "h1", true).await.unwrap();
        assert_eq!((from_disk.stored_ms, &from_disk.body[..]), (110, &b"made at 110"[..]));
        assert!(again.get("other", "h1", true).await.is_none(), "another deployment's name");
        // Lost: only the deployments there were then.
        again.mark_lost(vec!["dep".into()]);
        assert!(again.lost("dep") && !again.lost("new"));
        again.clear_lost();
        assert!(!again.lost("dep"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
