//! The firewall's configuration and its evaluation: custom rules and IP blocks, per project and
//! for the whole install, in the shape of Vercel's firewall API (`conditionGroup`, `conditions`
//! with `type`/`op`/`neg`/`key`/`value`, `action.mitigate`, `ips`), so a configuration exported
//! from Vercel reads as it is.
//!
//! A configuration is checked and compiled once (`Firewall::new`): patterns compiled, values
//! lowercased, addresses parsed into tables by prefix length. Evaluating a request then costs a
//! few string comparisons per condition; nothing is allocated unless a rule reads the query, the
//! cookies or the decoded path.
//!
//! How a request is judged within one configuration (`Firewall::check`):
//! 1. IP blocks (`ips`), by the client's address and the request's host (`*` every host,
//!    `*.example.com` every subdomain of it, else the host exactly). The first entry in the list
//!    that matches decides: `deny` answers 403, `bypass` skips the custom rules, `log` records.
//! 2. Custom rules (`rules`), in order, unless `firewallEnabled` is false. A rule matches when
//!    any of its groups matches, and a group when all of its conditions do. `deny` and `redirect`
//!    end the evaluation; `bypass` skips the remaining rules; `log` records and goes on (the last
//!    log rule matched is the one reported, as on Vercel). A `rate_limit` rule counts the request
//!    for its client (see `rate`): under the limit the evaluation goes on; over it, the rule's
//!    `rateLimit.action` answers (429 by default, or 403, or a log).
//!
//! Matching follows Vercel's documented rules: every operator is case-insensitive; `neg` inverts
//! a condition; `ex` asks whether a header, query parameter or cookie is there at all; a header
//! sent more than once matches if any of its values does; `ip_address` with `eq` or `inc` takes
//! addresses or CIDR ranges. Regexes are the `regex` crate's syntax, with `fancy-regex` for
//! lookarounds and backreferences.
//!
//! A `challenge` (a rule's, an IP block's, or a rate limit's over the limit) answers with the
//! router's challenge page unless the request carries a pass from solving one
//! (`Request::challenge_passed`); with a pass, the rules after it run, as on Vercel.
//!
//! `ja4_digest` matches the TLS client's JA4 fingerprint (`t13d1516h2_8daaf6152771_e5627efa2ab1`),
//! which the router reads from the ClientHello; plain HTTP requests have none.

mod rate;

pub use rate::{Algo, ApiCheck, Bucket, Hit, Limiter, NoLimits, OverAction, RateLimit, RateSpec};
use rate::RateRule;

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::rules::convert::{parse_query, percent_decode};

/// Vercel's limits, from its API schema: condition groups in a rule, conditions in a group, values
/// in a condition, and a rule's name and description.
pub const MAX_GROUPS: usize = 25;
pub const MAX_CONDITIONS: usize = 65;
pub const MAX_VALUES: usize = 75;
pub const MAX_NAME: usize = 160;
pub const MAX_DESCRIPTION: usize = 256;
/// forkrail's own: rules, rate-limit rules and IP blocks in one configuration, named lists, and
/// addresses in a list. Vercel's Pro plan allows 40 rules, 40 rate limits and 100 IP blocks; a
/// self-hosted install has no plans, so these follow what a router can afford (measured in
/// `forty_rules_judge_quickly` and on the bench). System bypasses aren't published with the rules:
/// they apply at once (`routes::Mitigations`).
pub const MAX_RULES: usize = 1000;
pub const MAX_RATE_LIMITS: usize = 100;
pub const MAX_IPS: usize = 10_000;
pub const MAX_LISTS: usize = 100;
pub const MAX_LIST_ENTRIES: usize = 100_000;
const MAX_PATTERN: usize = 4096;
const MAX_VALUE_LEN: usize = 4096;
const MAX_ID: usize = 64;
/// How long a persistent action may hold a client off, as Vercel allows.
pub const MIN_PERSIST: Duration = Duration::from_secs(60);
pub const MAX_PERSIST: Duration = Duration::from_secs(3600);

/// A project's (or the install's) firewall, as written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// Whether the custom rules run. IP blocks apply either way.
    #[serde(default = "yes")]
    pub firewall_enabled: bool,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub ips: Vec<IpRule>,
    /// Named lists of addresses and ranges, for conditions with `op: "list"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lists: BTreeMap<String, Vec<String>>,
    /// The hosted plan's managed rulesets; empty in the open-source install, and kept as written.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub managed_rules: BTreeMap<String, serde_json::Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self { firewall_enabled: true, rules: Vec::new(), ips: Vec::new(), lists: BTreeMap::new(), managed_rules: BTreeMap::new() }
    }
}

fn yes() -> bool {
    true
}

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default = "yes")]
    pub active: bool,
    pub condition_group: Vec<Group>,
    pub action: RuleAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// What it looks at: `path`, `header`, `ip_address`, `geo_country`...
    #[serde(rename = "type")]
    pub kind: String,
    /// How: `eq`, `pre`, `re`, `inc`...
    pub op: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub neg: bool,
    /// The header, query parameter or cookie's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Text(String),
    List(Vec<String>),
    Number(f64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleAction {
    pub mitigate: Mitigate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mitigate {
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<Redirect>,
    /// A persistent action: `1m` to `1h` during which the client's address gets the same answer
    /// for every request to the project. For `deny`, and for `rate_limit` once a client goes over
    /// (it then gets the over-limit answer that long).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_duration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypass_system: Option<bool>,
    /// For the `rate_limit` action; ignored with any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Log,
    Deny,
    Challenge,
    Bypass,
    RateLimit,
    Redirect,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Log => "log",
            Action::Deny => "deny",
            Action::Challenge => "challenge",
            Action::Bypass => "bypass",
            Action::RateLimit => "rate_limit",
            Action::Redirect => "redirect",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Redirect {
    /// A path in the deployment (`/blocked`) or a URL elsewhere.
    pub location: String,
    /// 301 instead of 307.
    #[serde(default)]
    pub permanent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpRule {
    #[serde(default)]
    pub id: String,
    /// `*` for every host of the project, `*.example.com` for every subdomain, else one host.
    pub hostname: String,
    /// An address or a CIDR range.
    pub ip: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    pub action: IpAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpAction {
    Deny,
    Challenge,
    Log,
    Bypass,
}

impl IpAction {
    pub fn name(self) -> &'static str {
        match self {
            IpAction::Deny => "deny",
            IpAction::Challenge => "challenge",
            IpAction::Log => "log",
            IpAction::Bypass => "bypass",
        }
    }
}

// ---- What a request offers ----

/// A request as the firewall sees it. The router fills it from the request and its client's
/// location; the CLI's `test` from a URL.
pub struct Request<'a> {
    pub method: &'a str,
    /// Without its port.
    pub host: &'a str,
    /// The path as the client sent it, still percent-encoded, without the query.
    pub raw_path: &'a str,
    /// The path the deployment serves it from: the static file's, or the request's for a function.
    /// `None` means the request's path.
    pub target_path: Option<&'a str>,
    pub query: Option<&'a str>,
    pub headers: &'a http::HeaderMap,
    pub ip: IpAddr,
    /// `HTTP/1.1`, `HTTP/2.0`.
    pub protocol: &'a str,
    /// `https` or `http`.
    pub scheme: &'a str,
    /// `production` or `preview`.
    pub environment: &'a str,
    /// The router's region, `us-east-1`.
    pub region: &'a str,
    pub continent: Option<&'a str>,
    pub country: Option<&'a str>,
    /// The country region's name (DB-IP's free database has no codes).
    pub country_region: Option<&'a str>,
    pub city: Option<&'a str>,
    pub asn: Option<u32>,
    pub ja4: Option<&'a str>,
    /// Set when the request is an app asking whether a rate limit allows something
    /// (`/.well-known/forkrail/rate-limit-api/<id>`): only rules naming that id run, and their
    /// rate limits count the key the app sent.
    pub rate_limit_api: Option<ApiCheck<'a>>,
    /// Whether it carries a valid pass from solving the challenge (the router checks the cookie).
    pub challenge_passed: bool,
}

/// Values worked out from a request once, on first use.
#[derive(Default)]
pub struct Facts {
    path: OnceCell<String>,
    query: OnceCell<Vec<(String, Vec<String>)>>,
    cookies: OnceCell<Vec<(String, String)>>,
}

impl Facts {
    fn path(&self, req: &Request) -> &str {
        self.path.get_or_init(|| normalize_path(req.raw_path))
    }

    fn query(&self, req: &Request) -> &[(String, Vec<String>)] {
        self.query.get_or_init(|| req.query.map(parse_query).unwrap_or_default())
    }

    fn cookies(&self, req: &Request) -> &[(String, String)] {
        self.cookies.get_or_init(|| {
            let mut out = Vec::new();
            for value in req.headers.get_all(http::header::COOKIE) {
                let Ok(text) = value.to_str() else { continue };
                for pair in text.split(';') {
                    let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                    let k = k.trim();
                    if !k.is_empty() {
                        out.push((k.to_owned(), percent_decode(v.trim().trim_matches('"'))));
                    }
                }
            }
            out
        })
    }
}

/// The request path the firewall's `path` condition sees: decoded, repeated slashes as one, and
/// without a trailing slash (`/api/` is `/api`), as Vercel's examples show.
pub fn normalize_path(raw: &str) -> String {
    let decoded = percent_decode(raw);
    let mut out = String::with_capacity(decoded.len() + 1);
    for part in decoded.split('/').filter(|p| !p.is_empty()) {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

// ---- The verdict ----

/// What one configuration says about a request.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub decision: Decision,
    /// The rule or IP block that decided, by id; for `Allow`, none.
    pub rule: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Nothing matched.
    Allow,
    /// A log rule (or IP block) matched and nothing acted.
    Log,
    /// A bypass matched: the remaining custom rules were skipped.
    Bypass,
    /// 403. With `persist`, the client's address is refused that long for every request.
    Deny { persist: Option<Duration> },
    /// 429: a rate limit went over. With `persist`, the client's address gets 429 that long.
    RateLimited { persist: Option<Duration>, retry_after: Duration },
    /// The challenge page. With `persist`, every request from the client's address gets it that
    /// long, unless it carries a pass.
    Challenge { persist: Option<Duration> },
    Redirect { location: String, status: u16 },
}

impl Decision {
    /// Whether it answers the request instead of the deployment.
    pub fn ends(&self) -> bool {
        matches!(self, Decision::Deny { .. } | Decision::RateLimited { .. } | Decision::Challenge { .. } | Decision::Redirect { .. })
    }

    /// How long the client's address is held to this answer, if at all.
    pub fn persist(&self) -> Option<Duration> {
        match self {
            Decision::Deny { persist } | Decision::RateLimited { persist, .. } | Decision::Challenge { persist } => *persist,
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Decision::Allow => "",
            Decision::Log => "log",
            Decision::Bypass => "bypass",
            Decision::Deny { .. } => "deny",
            Decision::RateLimited { .. } => "rate_limit",
            Decision::Challenge { .. } => "challenge",
            Decision::Redirect { .. } => "redirect",
        }
    }
}

impl Verdict {
    fn allow() -> Self {
        Self { decision: Decision::Allow, rule: None }
    }
}

// ---- Compiled ----

/// A configuration ready to judge requests.
pub struct Firewall {
    enabled: bool,
    rules: Vec<CompiledRule>,
    ips: IpRules,
    target_path: bool,
    /// The rate-limit API ids active rules name (lowercased), for the API's 404.
    api_ids: HashSet<String>,
}

struct CompiledRule {
    id: String,
    groups: Vec<Vec<Test>>,
    then: Then,
    /// Whether a condition names a rate-limit API id: only these rules judge the API's requests.
    api: bool,
}

enum Then {
    Log,
    Deny(Option<Duration>),
    Challenge(Option<Duration>),
    Bypass,
    Redirect(String, u16),
    RateLimit(Box<RateRule>),
}

struct Test {
    field: Field,
    check: Check,
    neg: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Field {
    Host,
    Path,
    RawPath,
    TargetPath,
    Method,
    UserAgent,
    Header(http::HeaderName),
    Query(String),
    Cookie(String),
    Ip,
    Region,
    Protocol,
    Scheme,
    Environment,
    Continent,
    Country,
    CountryRegion,
    City,
    Asn,
    RateLimitApiId,
    Ja4,
}

enum Check {
    /// `ex`: the key (or a value) is there.
    Exists,
    Eq(String),
    Any(HashSet<String>),
    Prefix(String),
    Suffix(String),
    Contains(String),
    Regex(Pattern),
    Number(Cmp, f64),
    /// `ip_address` against addresses and ranges (`eq`, `inc`, `list`).
    Ips(std::sync::Arc<IpSet<()>>),
}

enum Pattern {
    Plain(regex::Regex),
    Fancy(fancy_regex::Regex),
}

impl Pattern {
    fn new(source: &str) -> Result<Self, String> {
        match regex::RegexBuilder::new(source).case_insensitive(true).size_limit(1 << 20).build() {
            Ok(re) => Ok(Pattern::Plain(re)),
            Err(_) => fancy_regex::RegexBuilder::new(source)
                .case_insensitive(true)
                .backtrack_limit(100_000)
                // The same 1 MiB for what it hands to the regex crate (its default is 10).
                .delegate_size_limit(1 << 20)
                .delegate_dfa_size_limit(1 << 20)
                .build()
                .map(Pattern::Fancy)
                .map_err(|e| e.to_string()),
        }
    }

    fn is_match(&self, text: &str) -> bool {
        match self {
            Pattern::Plain(re) => re.is_match(text),
            // A pattern that backtracks past its limit doesn't match.
            Pattern::Fancy(re) => re.is_match(text).unwrap_or(false),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Cmp {
    Gt,
    Gte,
    Lt,
    Lte,
}

/// Which hosts an entry covers: `*`, `*.example.com` or one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostScope {
    All,
    /// `*.example.com`: hosts ending in `.example.com`.
    Under(String),
    Exactly(String),
}

impl HostScope {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim().to_ascii_lowercase();
        if text == "*" {
            return Ok(HostScope::All);
        }
        let check = |name: &str| -> Result<(), String> {
            if name.is_empty() || name.len() > 253 || !name.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')) {
                return Err(format!("host {text:?} isn't a domain name, `*.` and one, or `*`"));
            }
            Ok(())
        };
        match text.strip_prefix("*.") {
            Some(rest) => {
                check(rest)?;
                Ok(HostScope::Under(format!(".{rest}")))
            }
            None => {
                check(&text)?;
                Ok(HostScope::Exactly(text))
            }
        }
    }

    pub fn covers(&self, host: &str) -> bool {
        match self {
            HostScope::All => true,
            HostScope::Under(suffix) => host.len() > suffix.len() && host.ends_with(suffix.as_str()),
            HostScope::Exactly(name) => host == name,
        }
    }
}

/// An address range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: IpAddr,
    pub len: u8,
}

impl Cidr {
    /// `1.2.3.4`, `1.2.3.0/24`, `2001:db8::/32`; host bits are cleared. IPv4-mapped IPv6
    /// addresses are taken as IPv4.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (addr, len) = match text.split_once('/') {
            Some((a, l)) => (a, Some(l)),
            None => (text, None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| format!("{text:?} isn't an IP address or CIDR range"))?;
        let addr = addr.to_canonical();
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let len = match len {
            Some(l) => l.parse::<u8>().ok().filter(|&l| l <= max).ok_or_else(|| format!("{text:?}: the prefix length must be 0-{max}"))?,
            None => max,
        };
        Ok(Cidr { addr: mask(addr, len), len })
    }

    /// Whether `ip` is in the range.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        ip.is_ipv4() == self.addr.is_ipv4() && mask(ip, self.len) == self.addr
    }
}

fn mask(addr: IpAddr, len: u8) -> IpAddr {
    match addr {
        IpAddr::V4(a) => {
            let bits = u32::from(a);
            let m = if len == 0 { 0 } else { u32::MAX << (32 - len as u32) };
            IpAddr::V4((bits & m).into())
        }
        IpAddr::V6(a) => {
            let bits = u128::from(a);
            let m = if len == 0 { 0 } else { u128::MAX << (128 - len as u32) };
            IpAddr::V6((bits & m).into())
        }
    }
}

/// Addresses and ranges with a value each, found by trying each prefix length present: a lookup
/// costs one hash probe per distinct length, however many entries there are.
pub struct IpSet<T> {
    v4: Vec<(u8, HashMap<u32, Vec<T>>)>,
    v6: Vec<(u8, HashMap<u128, Vec<T>>)>,
}

impl<T> Default for IpSet<T> {
    fn default() -> Self {
        Self { v4: Vec::new(), v6: Vec::new() }
    }
}

impl<T> IpSet<T> {
    pub fn insert(&mut self, cidr: Cidr, value: T) {
        match cidr.addr {
            IpAddr::V4(a) => {
                let slot = match self.v4.iter().position(|(l, _)| *l == cidr.len) {
                    Some(i) => i,
                    None => {
                        self.v4.push((cidr.len, HashMap::new()));
                        self.v4.sort_by_key(|e| std::cmp::Reverse(e.0));
                        self.v4.iter().position(|(l, _)| *l == cidr.len).unwrap()
                    }
                };
                self.v4[slot].1.entry(u32::from(a)).or_default().push(value);
            }
            IpAddr::V6(a) => {
                let slot = match self.v6.iter().position(|(l, _)| *l == cidr.len) {
                    Some(i) => i,
                    None => {
                        self.v6.push((cidr.len, HashMap::new()));
                        self.v6.sort_by_key(|e| std::cmp::Reverse(e.0));
                        self.v6.iter().position(|(l, _)| *l == cidr.len).unwrap()
                    }
                };
                self.v6[slot].1.entry(u128::from(a)).or_default().push(value);
            }
        }
    }

    /// Every value whose range holds `ip`, longest prefixes first.
    pub fn find(&self, ip: IpAddr) -> impl Iterator<Item = &T> {
        let ip = ip.to_canonical();
        let (v4, v6): (Vec<&Vec<T>>, Vec<&Vec<T>>) = match ip {
            IpAddr::V4(a) => {
                let bits = u32::from(a);
                (self.v4.iter().filter_map(|(len, map)| map.get(&(if *len == 0 { 0 } else { bits & (u32::MAX << (32 - *len as u32)) }))).collect(), Vec::new())
            }
            IpAddr::V6(a) => {
                let bits = u128::from(a);
                (Vec::new(), self.v6.iter().filter_map(|(len, map)| map.get(&(if *len == 0 { 0 } else { bits & (u128::MAX << (128 - *len as u32)) }))).collect())
            }
        };
        v4.into_iter().chain(v6).flatten()
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.find(ip).next().is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.v4.is_empty() && self.v6.is_empty()
    }
}

struct IpRules {
    /// Entry index in the list, for "the first entry that matches decides".
    set: IpSet<usize>,
    entries: Vec<(String, HostScope, IpAction)>,
}

// ---- Checking and compiling ----

/// What a condition type reads, and which operators make sense on it.
fn field(kind: &str, key: Option<&str>) -> Result<Field, String> {
    let key = || -> Result<String, String> {
        let k = key.map(str::trim).filter(|k| !k.is_empty()).ok_or_else(|| format!("a {kind} condition needs a key (its name)"))?;
        if k.len() > 256 {
            return Err(format!("{kind} key {k:?} is too long"));
        }
        Ok(k.to_lowercase())
    };
    Ok(match kind {
        "host" => Field::Host,
        "path" => Field::Path,
        "raw_path" => Field::RawPath,
        "target_path" => Field::TargetPath,
        "method" => Field::Method,
        "user_agent" => Field::UserAgent,
        "header" => {
            let k = key()?;
            Field::Header(http::HeaderName::from_bytes(k.as_bytes()).map_err(|_| format!("header name {k:?} isn't valid"))?)
        }
        "query" => Field::Query(key()?),
        "cookie" => Field::Cookie(key()?),
        "ip_address" => Field::Ip,
        "region" => Field::Region,
        "protocol" => Field::Protocol,
        "scheme" => Field::Scheme,
        "environment" => Field::Environment,
        "geo_continent" => Field::Continent,
        "geo_country" => Field::Country,
        "geo_country_region" => Field::CountryRegion,
        "geo_city" => Field::City,
        "geo_as_number" => Field::Asn,
        "rate_limit_api_id" => Field::RateLimitApiId,
        "ja4_digest" => Field::Ja4,
        "ja3_digest" | "route" | "server_action" | "bot_name" | "bot_category" | "bot_status" | "bot_protection" | "domain_environment"
        | "shared_condition" | "traffic_source" | "ruleset" | "response_code" | "response_header" => {
            return Err(format!("condition type {kind:?} isn't supported"));
        }
        other => return Err(format!("unknown condition type {other:?}")),
    })
}

fn fold(s: &str) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase() || !b.is_ascii()) { Cow::Owned(s.to_lowercase()) } else { Cow::Borrowed(s) }
}

fn compile_condition(c: &Condition, lists: &BTreeMap<String, IpSetSource>) -> Result<Test, String> {
    let field = field(&c.kind, c.key.as_deref())?;
    let mut neg = c.neg;
    let op = match c.op.as_str() {
        "neq" => {
            neg = !neg;
            "eq"
        }
        "ninc" => {
            neg = !neg;
            "inc"
        }
        "nex" => {
            neg = !neg;
            "ex"
        }
        other => other,
    };
    let text = || -> Result<String, String> {
        match &c.value {
            Some(Value::Text(s)) if s.len() <= MAX_VALUE_LEN => Ok(s.clone()),
            Some(Value::Text(_)) => Err(format!("a {} value may be at most {MAX_VALUE_LEN} characters", c.kind)),
            Some(Value::Number(n)) => Ok(format_number(*n)),
            Some(Value::List(_)) => Err(format!("{} {op} takes one value, not a list", c.kind)),
            None => Err(format!("{} {op} needs a value", c.kind)),
        }
    };
    let values = || -> Result<Vec<String>, String> {
        let list = match &c.value {
            Some(Value::List(l)) => l.clone(),
            Some(Value::Text(s)) => vec![s.clone()],
            Some(Value::Number(n)) => vec![format_number(*n)],
            None => return Err(format!("{} {op} needs values", c.kind)),
        };
        if list.is_empty() || list.len() > MAX_VALUES {
            return Err(format!("{} {op} takes 1-{MAX_VALUES} values", c.kind));
        }
        if list.iter().any(|v| v.len() > MAX_VALUE_LEN) {
            return Err(format!("a {} value may be at most {MAX_VALUE_LEN} characters", c.kind));
        }
        Ok(list)
    };
    let check = match (op, &field) {
        ("ex", Field::Header(_) | Field::Query(_) | Field::Cookie(_)) => Check::Exists,
        ("ex", _) => Check::Exists,
        ("eq", Field::Ip) => {
            let mut set = IpSet::default();
            set.insert(Cidr::parse(&text()?)?, ());
            Check::Ips(std::sync::Arc::new(set))
        }
        ("inc", Field::Ip) => {
            let mut set = IpSet::default();
            for v in values()? {
                set.insert(Cidr::parse(&v)?, ());
            }
            Check::Ips(std::sync::Arc::new(set))
        }
        ("list", Field::Ip) => {
            let name = text()?;
            let source = lists.get(&name).ok_or_else(|| format!("there's no list {name:?}"))?;
            Check::Ips(source.build())
        }
        ("list", _) => return Err("op list is for ip_address conditions".into()),
        ("eq", _) => Check::Eq(fold(&text()?).into_owned()),
        ("inc", _) => Check::Any(values()?.iter().map(|v| fold(v).into_owned()).collect()),
        ("pre", _) => Check::Prefix(fold(&text()?).into_owned()),
        ("suf", _) => Check::Suffix(fold(&text()?).into_owned()),
        ("sub", _) => Check::Contains(fold(&text()?).into_owned()),
        ("re", _) => {
            let source = text()?;
            if source.len() > MAX_PATTERN {
                return Err(format!("a pattern may be at most {MAX_PATTERN} characters"));
            }
            Check::Regex(Pattern::new(&source).map_err(|e| format!("pattern {source:?}: {e}"))?)
        }
        (cmp @ ("gt" | "gte" | "lt" | "lte"), _) => {
            let n = match &c.value {
                Some(Value::Number(n)) => *n,
                Some(Value::Text(s)) => s.trim().parse::<f64>().map_err(|_| format!("{} {cmp} needs a number, not {s:?}", c.kind))?,
                _ => return Err(format!("{} {cmp} needs a number", c.kind)),
            };
            let cmp = match cmp {
                "gt" => Cmp::Gt,
                "gte" => Cmp::Gte,
                "lt" => Cmp::Lt,
                _ => Cmp::Lte,
            };
            Check::Number(cmp, n)
        }
        (other, _) => return Err(format!("unknown operator {other:?}")),
    };
    Ok(Test { field, check, neg })
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { n.to_string() }
}

/// A named list, parsed once and built into a set the first time a condition names it, which
/// every condition naming it shares (a list may hold 100,000 entries, named by any number).
struct IpSetSource(Vec<Cidr>, std::sync::OnceLock<std::sync::Arc<IpSet<()>>>);

impl IpSetSource {
    fn build(&self) -> std::sync::Arc<IpSet<()>> {
        self.1
            .get_or_init(|| {
                let mut set = IpSet::default();
                for c in &self.0 {
                    set.insert(*c, ());
                }
                std::sync::Arc::new(set)
            })
            .clone()
    }
}

/// `1m`, `30m`, `1h`, `90s`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (n, unit) = t.split_at(split);
    let n: u64 = n.parse().map_err(|_| format!("duration {text:?}: use a number and s, m or h (5m, 1h)"))?;
    let secs = match unit {
        "s" => n,
        "m" => n.saturating_mul(60),
        "h" => n.saturating_mul(3600),
        _ => return Err(format!("duration {text:?}: use a number and s, m or h (5m, 1h)")),
    };
    let d = Duration::from_secs(secs);
    if d < MIN_PERSIST || d > MAX_PERSIST {
        return Err(format!("duration {text:?}: from 1 minute to 1 hour"));
    }
    Ok(d)
}

fn check_id(id: &str, what: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_ID || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err(format!("{what} id {id:?}: use 1-{MAX_ID} of letters, digits, _ and -"));
    }
    Ok(())
}

fn valid_list_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Config {
    /// Check it as the routers will run it.
    pub fn validate(&self) -> Result<(), String> {
        Firewall::new(self).map(|_| ())
    }

    /// Give every rule, IP block and system bypass without an id one.
    pub fn assign_ids(&mut self) {
        for r in &mut self.rules {
            if r.id.is_empty() {
                r.id = format!("rule_{}", crate::random_hex(6));
            }
        }
        for ip in &mut self.ips {
            if ip.id.is_empty() {
                ip.id = format!("ip_{}", crate::random_hex(6));
            }
        }
    }

    /// Whether it does nothing at all.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.ips.is_empty() && self.managed_rules.is_empty()
    }
}

impl Firewall {
    pub fn new(config: &Config) -> Result<Self, String> {
        if config.rules.len() > MAX_RULES {
            return Err(format!("at most {MAX_RULES} rules"));
        }
        if config.ips.len() > MAX_IPS {
            return Err(format!("at most {MAX_IPS} IP blocks"));
        }
        let rate_limits = config.rules.iter().filter(|r| r.action.mitigate.action == Action::RateLimit).count();
        if rate_limits > MAX_RATE_LIMITS {
            return Err(format!("at most {MAX_RATE_LIMITS} rate-limit rules ({rate_limits} here)"));
        }
        if config.lists.len() > MAX_LISTS {
            return Err(format!("at most {MAX_LISTS} lists"));
        }
        let mut lists = BTreeMap::new();
        for (name, entries) in &config.lists {
            if !valid_list_name(name) {
                return Err(format!("list name {name:?}: use 1-64 of letters, digits, _ and -"));
            }
            if entries.len() > MAX_LIST_ENTRIES {
                return Err(format!("list {name}: at most {MAX_LIST_ENTRIES} addresses"));
            }
            let parsed = entries.iter().map(|e| Cidr::parse(e)).collect::<Result<Vec<_>, _>>().map_err(|e| format!("list {name}: {e}"))?;
            lists.insert(name.clone(), IpSetSource(parsed, std::sync::OnceLock::new()));
        }
        let mut ids = HashSet::new();
        let mut rules = Vec::new();
        let mut target_path = false;
        let mut api_ids = HashSet::new();
        for (i, rule) in config.rules.iter().enumerate() {
            let at = || format!("rule {} ({:?})", i + 1, rule.name);
            check_id(&rule.id, "rule").map_err(|e| format!("{}: {e}", at()))?;
            if !ids.insert(rule.id.as_str()) {
                return Err(format!("{}: id {} is used twice", at(), rule.id));
            }
            if rule.name.trim().is_empty() || rule.name.len() > MAX_NAME {
                return Err(format!("{}: a name of 1-{MAX_NAME} characters", at()));
            }
            if rule.description.len() > MAX_DESCRIPTION {
                return Err(format!("{}: a description of at most {MAX_DESCRIPTION} characters", at()));
            }
            if rule.condition_group.is_empty() || rule.condition_group.len() > MAX_GROUPS {
                return Err(format!("{}: 1-{MAX_GROUPS} condition groups", at()));
            }
            let mut groups = Vec::new();
            for group in &rule.condition_group {
                if group.conditions.is_empty() || group.conditions.len() > MAX_CONDITIONS {
                    return Err(format!("{}: 1-{MAX_CONDITIONS} conditions in each group", at()));
                }
                let tests = group.conditions.iter().map(|c| compile_condition(c, &lists)).collect::<Result<Vec<_>, _>>().map_err(|e| format!("{}: {e}", at()))?;
                target_path |= tests.iter().any(|t| t.field == Field::TargetPath);
                groups.push(tests);
            }
            let api = groups.iter().flatten().any(|t| t.field == Field::RateLimitApiId);
            let m = &rule.action.mitigate;
            let persist = m.action_duration.as_deref().filter(|d| !d.is_empty()).map(parse_duration).transpose().map_err(|e| format!("{}: {e}", at()))?;
            let then = match m.action {
                Action::Log => Then::Log,
                Action::Deny => Then::Deny(persist),
                Action::Bypass => Then::Bypass,
                Action::Redirect => {
                    let r = m.redirect.as_ref().ok_or_else(|| format!("{}: a redirect needs redirect.location", at()))?;
                    let location = r.location.trim();
                    let ok = location.starts_with('/') || location.starts_with("https://") || location.starts_with("http://");
                    if !ok || location.len() > 2048 || http::HeaderValue::from_str(location).is_err() {
                        return Err(format!("{}: redirect location {location:?} must be a path (/blocked) or an http(s) URL", at()));
                    }
                    Then::Redirect(location.to_owned(), if r.permanent { 301 } else { 307 })
                }
                Action::Challenge => Then::Challenge(persist),
                Action::RateLimit => {
                    let limit = m.rate_limit.as_ref().ok_or_else(|| format!("{}: a rate_limit rule needs rateLimit (algo, window, limit, keys)", at()))?;
                    Then::RateLimit(Box::new(RateRule::new(limit, persist).map_err(|e| format!("{}: {e}", at()))?))
                }
            };
            if persist.is_some() && !matches!(m.action, Action::Deny | Action::Challenge | Action::RateLimit) {
                return Err(format!("{}: a persistent action (actionDuration) goes with deny, challenge or rate_limit", at()));
            }
            if rule.active {
                if api && config.firewall_enabled {
                    for t in groups.iter().flatten().filter(|t| t.field == Field::RateLimitApiId && !t.neg) {
                        match &t.check {
                            Check::Eq(id) => {
                                api_ids.insert(id.clone());
                            }
                            Check::Any(set) => api_ids.extend(set.iter().cloned()),
                            _ => {}
                        }
                    }
                }
                rules.push(CompiledRule { id: rule.id.clone(), groups, then, api });
            }
        }
        let mut ips = IpRules { set: IpSet::default(), entries: Vec::new() };
        for (i, entry) in config.ips.iter().enumerate() {
            let at = || format!("IP block {} ({})", i + 1, entry.ip);
            check_id(&entry.id, "IP block").map_err(|e| format!("{}: {e}", at()))?;
            if !ids.insert(entry.id.as_str()) {
                return Err(format!("{}: id {} is used twice", at(), entry.id));
            }
            if entry.notes.len() > 500 {
                return Err(format!("{}: notes of at most 500 characters", at()));
            }
            let cidr = Cidr::parse(&entry.ip).map_err(|e| format!("{}: {e}", at()))?;
            let scope = HostScope::parse(&entry.hostname).map_err(|e| format!("{}: {e}", at()))?;
            ips.set.insert(cidr, i);
            ips.entries.push((entry.id.clone(), scope, entry.action));
        }
        Ok(Self { enabled: config.firewall_enabled, rules, ips, target_path, api_ids })
    }

    /// Whether a rule reads `target_path`, which the router then works out before judging.
    pub fn uses_target_path(&self) -> bool {
        self.target_path
    }

    /// Whether it can do anything to a request.
    pub fn is_empty(&self) -> bool {
        self.ips.entries.is_empty() && (self.rules.is_empty() || !self.enabled)
    }

    /// Whether an active rule names rate-limit API id `id` (the API answers 404 otherwise).
    pub fn knows_api(&self, id: &str) -> bool {
        self.api_ids.contains(fold(id).as_ref())
    }

    /// Whether an IP block denies or challenges `ip` on `host` (without its port), whatever it
    /// asks: for the router, which then starts no worker at the address's TLS handshake.
    pub fn stops_address(&self, ip: IpAddr, host: &str) -> bool {
        let first = self.ips.set.find(ip).copied().filter(|&i| self.ips.entries[i].1.covers(host)).min();
        first.is_some_and(|i| matches!(self.ips.entries[i].2, IpAction::Deny | IpAction::Challenge))
    }

    /// Judge `req`, counting it for rate limits in `limiter`.
    pub fn check(&self, req: &Request, limiter: &dyn Limiter) -> Verdict {
        let facts = Facts::default();
        let mut logged: Option<&str> = None;
        let mut skip_rules = !self.enabled;
        // An app's rate-limit question comes from the app's own function, not a visitor: IP blocks
        // aren't about it.
        let api = req.rate_limit_api.is_some();
        if !api && !self.ips.entries.is_empty() {
            let first = self.ips.set.find(req.ip).copied().filter(|&i| self.ips.entries[i].1.covers(req.host)).min();
            if let Some(i) = first {
                let (id, _, action) = &self.ips.entries[i];
                match action {
                    IpAction::Deny => return Verdict { decision: Decision::Deny { persist: None }, rule: Some(id.clone()) },
                    // Solved: the custom rules still run.
                    IpAction::Challenge if req.challenge_passed => {}
                    IpAction::Challenge => return Verdict { decision: Decision::Challenge { persist: None }, rule: Some(id.clone()) },
                    // The custom rules are skipped; nothing else here acts.
                    IpAction::Bypass => return Verdict { decision: Decision::Bypass, rule: Some(id.clone()) },
                    IpAction::Log => logged = Some(id),
                }
            }
        }
        if self.rules.is_empty() {
            skip_rules = true;
        }
        if !skip_rules {
            for rule in &self.rules {
                if (api && !rule.api) || !rule.groups.iter().any(|group| group.iter().all(|t| t.matches(req, &facts))) {
                    continue;
                }
                let decision = match &rule.then {
                    Then::Log => {
                        logged = Some(&rule.id);
                        continue;
                    }
                    Then::Deny(persist) => Decision::Deny { persist: *persist },
                    Then::Challenge(_) if req.challenge_passed => continue,
                    Then::Challenge(persist) => Decision::Challenge { persist: *persist },
                    Then::Bypass => Decision::Bypass,
                    Then::Redirect(location, status) => Decision::Redirect { location: location.clone(), status: *status },
                    Then::RateLimit(limit) => {
                        let client = match &req.rate_limit_api {
                            Some(check) => Cow::Borrowed(check.key.as_bytes()),
                            None => Cow::Owned(limit.client(req, &facts)),
                        };
                        let hit = limiter.hit(&rule.id, limit.spec, &client);
                        if !hit.over {
                            continue;
                        }
                        match limit.over {
                            OverAction::Log => {
                                logged = Some(&rule.id);
                                continue;
                            }
                            OverAction::Deny => Decision::Deny { persist: limit.persist },
                            OverAction::Challenge if req.challenge_passed => continue,
                            OverAction::Challenge => Decision::Challenge { persist: limit.persist },
                            OverAction::RateLimit => {
                                Decision::RateLimited { persist: limit.persist, retry_after: hit.retry_after.max(limit.persist.unwrap_or_default()) }
                            }
                        }
                    }
                };
                return Verdict { decision, rule: Some(rule.id.clone()) };
            }
        }
        match logged {
            Some(id) => Verdict { decision: Decision::Log, rule: Some(id.to_owned()) },
            None => Verdict::allow(),
        }
    }
}

impl Test {
    fn matches(&self, req: &Request, facts: &Facts) -> bool {
        self.matched(req, facts) != self.neg
    }

    fn matched(&self, req: &Request, facts: &Facts) -> bool {
        match &self.field {
            Field::Header(name) => {
                let mut values = req.headers.get_all(name).iter().peekable();
                if matches!(self.check, Check::Exists) {
                    return values.peek().is_some();
                }
                values.any(|v| self.check.text(&String::from_utf8_lossy(v.as_bytes())))
            }
            Field::UserAgent => {
                let mut values = req.headers.get_all(http::header::USER_AGENT).iter().peekable();
                if matches!(self.check, Check::Exists) {
                    return values.peek().is_some();
                }
                values.any(|v| self.check.text(&String::from_utf8_lossy(v.as_bytes())))
            }
            Field::Query(key) => {
                let mut found = facts.query(req).iter().filter(|(k, _)| fold(k) == key.as_str()).flat_map(|(_, vs)| vs).peekable();
                if matches!(self.check, Check::Exists) {
                    return found.peek().is_some();
                }
                found.any(|v| self.check.text(v))
            }
            Field::Cookie(key) => {
                let mut found = facts.cookies(req).iter().filter(|(k, _)| fold(k) == key.as_str()).map(|(_, v)| v).peekable();
                if matches!(self.check, Check::Exists) {
                    return found.peek().is_some();
                }
                found.any(|v| self.check.text(v))
            }
            Field::Ip => match &self.check {
                Check::Ips(set) => set.contains(req.ip),
                Check::Exists => true,
                check => check.text(&req.ip.to_canonical().to_string()),
            },
            Field::Asn => match (req.asn, &self.check) {
                (None, _) => false,
                (Some(_), Check::Exists) => true,
                (Some(n), Check::Number(cmp, limit)) => {
                    let n = n as f64;
                    match cmp {
                        Cmp::Gt => n > *limit,
                        Cmp::Gte => n >= *limit,
                        Cmp::Lt => n < *limit,
                        Cmp::Lte => n <= *limit,
                    }
                }
                (Some(n), check) => check.text(&n.to_string()),
            },
            field => {
                let value: Option<&str> = match field {
                    Field::Host => Some(req.host),
                    Field::Path => Some(facts.path(req)),
                    Field::RawPath => Some(req.raw_path),
                    Field::TargetPath => Some(req.target_path.unwrap_or_else(|| facts.path(req))),
                    Field::Method => Some(req.method),
                    Field::Region => Some(req.region),
                    Field::Protocol => Some(req.protocol),
                    Field::Scheme => Some(req.scheme),
                    Field::Environment => Some(req.environment),
                    Field::Continent => req.continent,
                    Field::Country => req.country,
                    Field::CountryRegion => req.country_region,
                    Field::City => req.city,
                    Field::RateLimitApiId => req.rate_limit_api.as_ref().map(|c| c.id),
                    Field::Ja4 => req.ja4,
                    _ => None,
                };
                let Some(value) = value.filter(|v| !v.is_empty()) else { return false };
                match &self.check {
                    Check::Exists => true,
                    Check::Number(cmp, limit) => value.trim().parse::<f64>().is_ok_and(|n| match cmp {
                        Cmp::Gt => n > *limit,
                        Cmp::Gte => n >= *limit,
                        Cmp::Lt => n < *limit,
                        Cmp::Lte => n <= *limit,
                    }),
                    check => check.text(value),
                }
            }
        }
    }
}

impl Check {
    /// Whether one text value passes (case-insensitive).
    fn text(&self, value: &str) -> bool {
        match self {
            Check::Exists => true,
            Check::Regex(p) => p.is_match(value),
            Check::Number(cmp, limit) => value.trim().parse::<f64>().is_ok_and(|n| match cmp {
                Cmp::Gt => n > *limit,
                Cmp::Gte => n >= *limit,
                Cmp::Lt => n < *limit,
                Cmp::Lte => n <= *limit,
            }),
            Check::Ips(set) => value.trim().parse::<IpAddr>().is_ok_and(|ip| set.contains(ip)),
            check => {
                let v = fold(value);
                match check {
                    Check::Eq(want) => v == want.as_str(),
                    Check::Any(set) => set.contains(v.as_ref()),
                    Check::Prefix(p) => v.starts_with(p.as_str()),
                    Check::Suffix(s) => v.ends_with(s.as_str()),
                    Check::Contains(s) => v.contains(s.as_str()),
                    _ => false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
