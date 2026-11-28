//! System mitigations: limits on each client address across every project, always on, in front of
//! the firewall, so one address can't take the router's connections or CPU from everyone else.
//!
//! - **Connections:** at most `FORKRAIL_SHIELD_CONNECTIONS` (default 256) open at once from one
//!   address. Checked when the connection is accepted, before the TLS handshake: a refused one is
//!   closed at once and costs a hash lookup.
//! - **Requests:** an address sending more than `FORKRAIL_SHIELD_REQUESTS` (default 5,000) in 10
//!   seconds, over every project and port, is blocked for `FORKRAIL_SHIELD_BLOCK_S` (default 600):
//!   its new connections are closed when accepted, and requests on connections it already has get
//!   429 with `connection: close`. A block is renewed while the address keeps sending. Measured
//!   2026-11-14 over 7 days: the busiest legitimate client sent 391 in 10 s, scanners up to 2,708.
//! - An IPv6 client counts by its /64, which is what one subscriber usually holds.
//!
//! What it refuses isn't written to the access log one request at a time (at flood rates that would
//! be most of the router's work): once a minute, a `mitigation` line per address says what was
//! refused (`requests`, `connections`), and a block's start has its own line. They reach
//! ClickHouse's `events` table like the router's other lines. A minute in which more than
//! `ATTACK_ALERT` requests or connections were refused, or an address was blocked, logs an
//! `attack detected` warning (at most every 10 minutes).
//!
//! The routing table's `mitigations` (see `routes::Mitigations`) apply at once: the install's
//! `limits` take the place of the numbers above (each one it sets; the environment's are the
//! router's own, for the rest), its `paused_until` turns all of this off; `system_bypass` entries (the install's, for any host; a
//! project's, for its hosts) leave an address alone. At accept the host isn't known yet, so an
//! address any bypass names isn't refused there; its requests are judged by host.
//!
//! The state is this router's own, like the firewall's holds. At most `MAX_TRACKED` addresses are
//! tracked at once; past it, new addresses go untracked until old ones go.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forkrail_common::firewall::{Cidr, HostScope, IpSet};
use forkrail_common::routes::{Routes, SHIELD_DEFAULTS, ShieldLimits};
use tokio::sync::watch;
use tracing::{info, warn};

const SHARDS: usize = 32;
/// Addresses tracked at once (about 50 bytes each).
const MAX_TRACKED: usize = 500_000;
/// The request window.
const WINDOW_S: u64 = 10;
/// How often finished state is dropped; summaries go out every sixth time.
pub const TICK: Duration = Duration::from_secs(10);
/// Refused requests and connections in a minute that make an "attack detected" warning.
const ATTACK_ALERT: u64 = 10_000;
const ALERT_EVERY_S: u64 = 600;

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub connections: u32,
    pub requests: u32,
    pub block_for: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            connections: SHIELD_DEFAULTS.connections.unwrap_or(256),
            requests: SHIELD_DEFAULTS.requests.unwrap_or(5_000),
            block_for: Duration::from_secs(SHIELD_DEFAULTS.block_s.unwrap_or(600).into()),
        }
    }
}

impl Settings {
    /// These, with each limit the routing table sets in their place.
    fn with(self, limits: &ShieldLimits) -> Self {
        Self {
            connections: limits.connections.unwrap_or(self.connections),
            requests: limits.requests.unwrap_or(self.requests),
            block_for: limits.block_s.map_or(self.block_for, |s| Duration::from_secs(s.into())),
        }
    }
}

/// One address's state.
#[derive(Debug, Default, Clone, Copy)]
struct Client {
    conns: u32,
    /// The request window's number, and requests in it.
    window: u64,
    count: u32,
    /// Unix seconds; 0: not blocked.
    blocked_until: u64,
    /// Refused since the last summary.
    dropped: u32,
    refused: u32,
    /// Why the last refusal: requests over the limit (block), or too many connections.
    over_connections: bool,
}

impl Client {
    fn idle(&self, now: u64) -> bool {
        self.conns == 0 && self.blocked_until <= now && self.window + 1 < now / WINDOW_S && self.dropped == 0 && self.refused == 0
    }
}

/// Who the routing table says to leave alone, and whether everything is paused.
#[derive(Default)]
pub struct Policy {
    paused_until: Option<u64>,
    /// Every bypassed range, whatever its host: a quick no for requests.
    any: IpSet<()>,
    /// The install's own bypasses (set by an owner), for accepting connections: a project's
    /// (which its members set) covers its hosts' requests only, since a connection names no host
    /// yet and the cap and blocks at accept guard every project.
    install: IpSet<()>,
    entries: Vec<(Cidr, Option<String>, HostScope)>,
    /// Projects in attack mode, until when (Unix seconds).
    pub attack_mode: HashMap<String, u64>,
    /// The install's limits, over the router's own.
    limits: ShieldLimits,
}

impl Policy {
    pub fn from_routes(routes: &Routes) -> Self {
        let mut policy = Policy { paused_until: routes.mitigations.paused_until, limits: routes.mitigations.limits.unwrap_or_default(), ..Policy::default() };
        let sections = std::iter::once((None, &routes.mitigations)).chain(routes.projects.iter().map(|(p, r)| (Some(p.clone()), &r.mitigations)));
        for (project, m) in sections {
            if let (Some(p), Some(until)) = (&project, m.attack_mode_until) {
                policy.attack_mode.insert(p.clone(), until);
            }
            for b in &m.system_bypass {
                // The table was validated; an entry that still doesn't parse is skipped.
                let (Ok(cidr), Ok(scope)) = (Cidr::parse(&b.source_ip), HostScope::parse(&b.domain)) else { continue };
                policy.any.insert(cidr, ());
                if project.is_none() {
                    policy.install.insert(cidr, ());
                }
                policy.entries.push((cidr, project.clone(), scope));
            }
        }
        policy
    }

    fn paused(&self, now: u64) -> bool {
        self.paused_until.is_some_and(|until| until > now)
    }

    /// Whether `ip` is left alone on `host` (of `project`, worked out only if a project's bypass
    /// could apply).
    fn bypassed(&self, ip: IpAddr, host: &str, project: impl FnOnce() -> Option<String>) -> bool {
        if !self.any.contains(ip) {
            return false;
        }
        let mut project = Some(project);
        let mut owner_of_host: Option<Option<String>> = None;
        for (cidr, owner, scope) in &self.entries {
            if !cidr.contains(ip) || !scope.covers(host) {
                continue;
            }
            let Some(p) = owner else { return true };
            let found = owner_of_host.get_or_insert_with(|| project.take().and_then(|f| f()));
            if found.as_deref() == Some(p.as_str()) {
                return true;
            }
        }
        false
    }
}

/// Why a connection was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Blocked,
    TooManyConnections,
}

pub struct Shield {
    settings: RwLock<Settings>,
    shards: Box<[Mutex<HashMap<u128, Client>>]>,
    per_shard: usize,
    policy: RwLock<Arc<Policy>>,
    /// Refused this minute, over every address, for the alert.
    refused_now: AtomicU64,
    blocks_now: AtomicU64,
    alerted_at: AtomicU64,
}

/// An accepted connection, counted until it's dropped.
pub struct Conn {
    shield: Arc<Shield>,
    key: Option<u128>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            let mut map = self.shield.shard(key).lock().unwrap();
            if let Some(c) = map.get_mut(&key) {
                c.conns = c.conns.saturating_sub(1);
            }
        }
    }
}

impl Default for Shield {
    fn default() -> Self {
        Self::new(Settings::default())
    }
}

impl Shield {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings: RwLock::new(settings),
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            per_shard: MAX_TRACKED.div_ceil(SHARDS),
            policy: RwLock::new(Arc::default()),
            refused_now: AtomicU64::new(0),
            blocks_now: AtomicU64::new(0),
            alerted_at: AtomicU64::new(0),
        }
    }

    pub fn set_settings(&self, settings: Settings) {
        *self.settings.write().unwrap() = settings;
    }

    pub fn policy(&self) -> Arc<Policy> {
        self.policy.read().unwrap().clone()
    }

    /// Whether `project` is in attack mode now.
    pub fn attack_mode(&self, project: &str) -> bool {
        let policy = self.policy.read().unwrap();
        !policy.attack_mode.is_empty() && policy.attack_mode.get(project).is_some_and(|until| *until > now_s())
    }

    /// Whether a system bypass leaves `ip` alone on `host` of `project` (attack mode too).
    pub fn exempt(&self, ip: IpAddr, host: &str, project: &str) -> bool {
        self.policy().bypassed(ip, host, || Some(project.to_owned()))
    }

    fn shard(&self, key: u128) -> &Mutex<HashMap<u128, Client>> {
        let mixed = (key as u64) ^ ((key >> 64) as u64).rotate_left(17);
        &self.shards[(mixed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 59) as usize % SHARDS]
    }

    /// Admit a connection from `ip`, or say why not.
    pub fn admit(self: &Arc<Self>, ip: IpAddr) -> Result<Conn, Refused> {
        self.admit_at(ip, now_s())
    }

    fn admit_at(self: &Arc<Self>, ip: IpAddr, now: u64) -> Result<Conn, Refused> {
        let policy = self.policy();
        let exempt = policy.paused(now) || policy.install.contains(ip);
        let limit = self.settings.read().unwrap().with(&policy.limits).connections;
        let key = key(ip);
        let mut map = self.shard(key).lock().unwrap();
        if !map.contains_key(&key) && map.len() >= self.per_shard {
            map.retain(|_, c| !c.idle(now));
            if map.len() >= self.per_shard {
                return Ok(Conn { shield: self.clone(), key: None });
            }
        }
        let client = map.entry(key).or_default();
        if !exempt {
            let refused = if client.blocked_until > now {
                Some(Refused::Blocked)
            } else if client.conns >= limit {
                client.over_connections = true;
                Some(Refused::TooManyConnections)
            } else {
                None
            };
            if let Some(why) = refused {
                client.refused = client.refused.saturating_add(1);
                self.refused_now.fetch_add(1, Ordering::Relaxed);
                return Err(why);
            }
        }
        client.conns += 1;
        Ok(Conn { shield: self.clone(), key: Some(key) })
    }

    /// Count a request from `ip` to `host`; how long it's blocked for, if it is.
    pub fn request(&self, ip: IpAddr, host: &str, project: impl FnOnce() -> Option<String>) -> Option<Duration> {
        self.request_at(ip, host, project, now_s())
    }

    fn request_at(&self, ip: IpAddr, host: &str, project: impl FnOnce() -> Option<String>, now: u64) -> Option<Duration> {
        let policy = self.policy();
        if policy.paused(now) || policy.bypassed(ip, host, project) {
            return None;
        }
        let settings = self.settings.read().unwrap().with(&policy.limits);
        let key = key(ip);
        let mut map = self.shard(key).lock().unwrap();
        if !map.contains_key(&key) && map.len() >= self.per_shard {
            return None;
        }
        let client = map.entry(key).or_default();
        if client.blocked_until > now {
            client.dropped = client.dropped.saturating_add(1);
            self.refused_now.fetch_add(1, Ordering::Relaxed);
            // Renewed while it keeps sending at the rate that blocked it.
            let window = now / WINDOW_S;
            if client.window != window {
                client.window = window;
                client.count = 0;
            }
            client.count = client.count.saturating_add(1);
            if client.count == settings.requests {
                client.blocked_until = now + settings.block_for.as_secs();
            }
            return Some(Duration::from_secs(client.blocked_until - now));
        }
        let window = now / WINDOW_S;
        if client.window != window {
            client.window = window;
            client.count = 0;
        }
        client.count = client.count.saturating_add(1);
        if client.count <= settings.requests {
            return None;
        }
        client.blocked_until = now + settings.block_for.as_secs();
        client.over_connections = false;
        client.dropped = client.dropped.saturating_add(1);
        self.refused_now.fetch_add(1, Ordering::Relaxed);
        self.blocks_now.fetch_add(1, Ordering::Relaxed);
        let shown = shown(ip);
        info!(
            kind = "block",
            ip = %shown,
            requests = client.count,
            window_s = WINDOW_S,
            block_s = settings.block_for.as_secs(),
            "mitigation"
        );
        Some(settings.block_for)
    }

    /// Follow the routing table's mitigations until the router exits.
    pub async fn follow(self: Arc<Self>, mut policies: watch::Receiver<Arc<Policy>>) {
        loop {
            let policy = policies.borrow_and_update().clone();
            let before = self.policy().limits;
            if policy.limits != before {
                let now = self.settings.read().unwrap().with(&policy.limits);
                info!(connections = now.connections, requests = now.requests, window_s = WINDOW_S, block_s = now.block_for.as_secs(), "system mitigations' limits");
            }
            *self.policy.write().unwrap() = policy;
            if policies.changed().await.is_err() {
                return;
            }
        }
    }

    /// Drop finished state every `TICK`, and once a minute say what was refused.
    pub async fn maintain(self: Arc<Self>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            n += 1;
            self.sweep(now_s(), n.is_multiple_of(6));
        }
    }

    /// Drop finished state; with `report`, log what each address had refused and reset it.
    fn sweep(&self, now: u64, report: bool) -> Vec<(u128, u32, u32)> {
        let mut reported = Vec::new();
        for shard in self.shards.iter() {
            let mut map = shard.lock().unwrap();
            if report {
                for (key, c) in map.iter_mut().filter(|(_, c)| c.dropped > 0 || c.refused > 0) {
                    reported.push((*key, c.dropped, c.refused));
                    info!(
                        kind = if c.blocked_until > now { "blocked" } else if c.over_connections { "connections" } else { "ended" },
                        ip = %from_key(*key),
                        requests = c.dropped,
                        connections = c.refused,
                        blocked_s = c.blocked_until.saturating_sub(now),
                        "mitigation"
                    );
                    c.dropped = 0;
                    c.refused = 0;
                }
            }
            map.retain(|_, c| !c.idle(now));
        }
        if report {
            let refused = self.refused_now.swap(0, Ordering::Relaxed);
            let blocks = self.blocks_now.swap(0, Ordering::Relaxed);
            let last = self.alerted_at.load(Ordering::Relaxed);
            if (refused > ATTACK_ALERT || blocks > 0) && now >= last + ALERT_EVERY_S {
                self.alerted_at.store(now, Ordering::Relaxed);
                warn!(refused, blocks, addresses = reported.len(), "attack detected: the system mitigations refused traffic in the last minute");
                let mut top: Vec<&(u128, u32, u32)> = reported.iter().collect();
                top.sort_by_key(|(_, requests, connections)| std::cmp::Reverse(u64::from(*requests) + u64::from(*connections)));
                let top: Vec<serde_json::Value> = top
                    .iter()
                    .take(5)
                    .map(|(key, requests, connections)| serde_json::json!({"ip": from_key(*key).to_string(), "requests": requests, "connections": connections}))
                    .collect();
                crate::events::send(
                    crate::events::ATTACK_DETECTED,
                    serde_json::json!({"refused": refused, "blocks": blocks, "addresses": reported.len(), "top": top, "window_s": 60}),
                );
            }
        }
        reported
    }

    /// Addresses blocked now, and until when (Unix seconds).
    #[cfg(test)]
    fn blocked(&self, now: u64) -> Vec<(IpAddr, u64)> {
        self.shards.iter().flat_map(|s| s.lock().unwrap().iter().filter(|(_, c)| c.blocked_until > now).map(|(k, c)| (from_key(*k), c.blocked_until)).collect::<Vec<_>>()).collect()
    }
}

/// An address as counted: IPv4 as itself (mapped), IPv6 by its /64.
fn key(ip: IpAddr) -> u128 {
    match ip.to_canonical() {
        IpAddr::V4(a) => 0xffff_0000_0000 | u128::from(u32::from(a)),
        IpAddr::V6(a) => u128::from(a) & (u128::MAX << 64),
    }
}

fn from_key(key: u128) -> IpAddr {
    IpAddr::from(std::net::Ipv6Addr::from(key)).to_canonical()
}

/// How a counted address is shown: IPv6 as its /64.
fn shown(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(a) => a.to_string(),
        v6 => format!("{}/64", from_key(key(v6))),
    }
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_800_000_000;

    fn shield(settings: Settings) -> Arc<Shield> {
        Arc::new(Shield::new(settings))
    }

    fn routes(value: serde_json::Value) -> Routes {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn connections_from_one_address_are_capped_and_given_back() {
        let s = shield(Settings { connections: 3, ..Settings::default() });
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let held: Vec<Conn> = (0..3).map(|_| s.admit_at(ip, T).unwrap()).collect();
        assert_eq!(s.admit_at(ip, T).err(), Some(Refused::TooManyConnections));
        // Another address isn't affected; one closed connection makes room.
        assert!(s.admit_at("192.0.2.2".parse().unwrap(), T).is_ok());
        drop(held);
        assert!(s.admit_at(ip, T).is_ok());
    }

    #[test]
    fn an_address_over_the_request_limit_is_blocked_and_refused_at_connect() {
        let s = shield(Settings { requests: 100, block_for: Duration::from_secs(600), ..Settings::default() });
        let ip: IpAddr = "198.51.100.7".parse().unwrap();
        for _ in 0..100 {
            assert_eq!(s.request_at(ip, "a.example.com", || None, T), None);
        }
        assert_eq!(s.request_at(ip, "b.example.com", || None, T), Some(Duration::from_secs(600)));
        assert_eq!(s.blocked(T).len(), 1);
        assert_eq!(s.admit_at(ip, T + 5).err(), Some(Refused::Blocked));
        assert_eq!(s.request_at(ip, "a.example.com", || None, T + 100), Some(Duration::from_secs(500)));
        // Once it ends, a new window starts clean.
        assert_eq!(s.request_at(ip, "a.example.com", || None, T + 601), None);
        assert!(s.admit_at(ip, T + 601).is_ok());
        // The summary says what was refused, then starts over.
        let reported = s.sweep(T + 602, true);
        assert_eq!(reported, vec![(key(ip), 2, 1)]);
        assert!(s.sweep(T + 603, true).is_empty());
    }

    #[test]
    fn a_block_is_renewed_while_the_flood_goes_on() {
        let s = shield(Settings { requests: 10, block_for: Duration::from_secs(60), ..Settings::default() });
        let ip: IpAddr = "203.0.113.9".parse().unwrap();
        for _ in 0..11 {
            s.request_at(ip, "h", || None, T);
        }
        // 50 s later, still flooding: the block now runs a minute from there.
        for _ in 0..10 {
            s.request_at(ip, "h", || None, T + 50);
        }
        assert_eq!(s.blocked(T + 50), vec![(ip, T + 110)]);
    }

    #[test]
    fn ipv6_counts_by_its_slash_64() {
        let s = shield(Settings { requests: 2, ..Settings::default() });
        for (i, ip) in ["2001:db8:1:2::1", "2001:db8:1:2::2", "2001:db8:1:2:ffff::"].into_iter().enumerate() {
            let blocked = s.request_at(ip.parse().unwrap(), "h", || None, T).is_some();
            assert_eq!(blocked, i == 2, "{ip}");
        }
        assert!(s.request_at("2001:db8:1:3::1".parse().unwrap(), "h", || None, T).is_none());
        assert_eq!(shown("2001:db8:1:2::1".parse().unwrap()), "2001:db8:1:2::/64");
    }

    #[test]
    fn the_installs_limits_take_the_place_of_the_routers_own() {
        let s = shield(Settings { requests: 5, connections: 2, block_for: Duration::from_secs(600) });
        let ip: IpAddr = "192.0.2.9".parse().unwrap();
        let raised = routes(serde_json::json!({"mitigations": {"limits": {"connections": 4, "requests": 20, "block_s": 60}}}));
        *s.policy.write().unwrap() = Arc::new(Policy::from_routes(&raised));
        let held: Vec<Conn> = (0..4).map(|_| s.admit_at(ip, T).unwrap()).collect();
        assert_eq!(s.admit_at(ip, T).err(), Some(Refused::TooManyConnections));
        drop(held);
        assert!((0..20).all(|_| s.request_at(ip, "a.example.com", || None, T).is_none()));
        assert_eq!(s.request_at(ip, "a.example.com", || None, T), Some(Duration::from_secs(60)));
        // A field the table leaves out keeps the router's own; no limits at all, all of them.
        let partly = routes(serde_json::json!({"mitigations": {"limits": {"requests": 50}}}));
        assert_eq!(s.settings.read().unwrap().with(&Policy::from_routes(&partly).limits).connections, 2);
        assert_eq!(s.settings.read().unwrap().with(&Policy::from_routes(&Routes::default()).limits).requests, 5);
    }

    #[test]
    fn bypasses_and_the_pause_come_from_the_routing_table() {
        let s = shield(Settings { requests: 1, connections: 1, ..Settings::default() });
        let table = routes(serde_json::json!({
            "mitigations": {"system_bypass": [{"id": "office", "source_ip": "192.0.2.0/24", "domain": "*.example.com"}]},
            "projects": {"api": {"mitigations": {"attack_mode_until": T + 3600, "system_bypass": [{"id": "partner", "source_ip": "198.51.100.5"}]}}}
        }));
        *s.policy.write().unwrap() = Arc::new(Policy::from_routes(&table));
        let office: IpAddr = "192.0.2.44".parse().unwrap();
        let partner: IpAddr = "198.51.100.5".parse().unwrap();
        // The install's bypass covers its domains only.
        for _ in 0..5 {
            assert!(s.request_at(office, "www.example.com", || None, T).is_none());
        }
        assert!(s.request_at(office, "other.test", || None, T).is_none());
        assert!(s.request_at(office, "other.test", || None, T).is_some());
        // A project's covers that project's hosts only.
        for _ in 0..5 {
            assert!(s.request_at(partner, "api.forkrail.com", || Some("api".to_owned()), T).is_none());
        }
        assert!(s.request_at(partner, "web.forkrail.com", || Some("web".to_owned()), T).is_none());
        assert!(s.request_at(partner, "web.forkrail.com", || Some("web".to_owned()), T).is_some());
        // The install's bypass isn't refused at connect, whatever it's blocked for elsewhere; a
        // project's is, once blocked elsewhere (its members set it, and the connection names no
        // host yet), and it counts toward the connection cap.
        let _a = s.admit_at(office, T).unwrap();
        let _b = s.admit_at(office, T).unwrap();
        assert_eq!(s.admit_at(partner, T).err(), Some(Refused::Blocked));
        let everyone = routes(serde_json::json!({"projects": {"api": {"mitigations": {"system_bypass": [{"id": "all", "source_ip": "0.0.0.0/0"}]}}}}));
        *s.policy.write().unwrap() = Arc::new(Policy::from_routes(&everyone));
        let fresh: IpAddr = "203.0.113.77".parse().unwrap();
        let _d = s.admit_at(fresh, T).unwrap();
        assert_eq!(s.admit_at(fresh, T).err(), Some(Refused::TooManyConnections), "a project's bypass of everyone leaves the cap");
        *s.policy.write().unwrap() = Arc::new(Policy::from_routes(&table));
        assert_eq!(s.policy().attack_mode.get("api"), Some(&(T + 3600)));
        // Paused: nothing is refused.
        let paused = routes(serde_json::json!({"mitigations": {"paused_until": T + 60}}));
        *s.policy.write().unwrap() = Arc::new(Policy::from_routes(&paused));
        let other: IpAddr = "203.0.113.1".parse().unwrap();
        let _c = s.admit_at(other, T).unwrap();
        assert!(s.admit_at(other, T).is_ok());
        assert!((0..10).all(|_| s.request_at(other, "x", || None, T).is_none()));
    }

    #[test]
    fn checks_are_quick() {
        let s = shield(Settings::default());
        let n = 200_000u32;
        let start = std::time::Instant::now();
        for i in 0..n {
            let ip = IpAddr::from(std::net::Ipv4Addr::from(0x0a00_0000 + (i % 50_000)));
            s.request_at(ip, "api.example.com", || None, T);
        }
        let each = start.elapsed() / n;
        eprintln!("shield: {each:?} a request");
        if !cfg!(debug_assertions) {
            assert!(each < Duration::from_micros(2), "{each:?}");
        }
    }
}
