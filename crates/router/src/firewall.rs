//! The firewall in the router: the install's configuration and each project's (see
//! `forkrail_common::firewall`), kept compiled in memory, and the addresses persistent actions
//! hold off.
//!
//! The routing table names each configuration by its SHA-256 (`firewall`, and each project's
//! `projects.<name>.firewall`); the configuration itself is `static/firewall/<id>.json` in this
//! router's region, read from where static files are (`statics.rs`). Whenever the table names a new
//! one it's read and compiled off the request path, and requests switch to it once it's ready: a
//! request never waits on S3 for the firewall, and a configuration that can't be read leaves the
//! previous one in force (retried every few seconds). A project the table no longer names has no
//! firewall.
//!
//! Each request (see `proxy::handle`) is judged in this order:
//! 1. persistent blocks: the install's, then the project's, for the client's address;
//! 2. the install's configuration: its IP blocks, then its custom rules;
//! 3. the project's: its IP blocks, then its custom rules.
//!
//! A `deny`, `redirect` or a rate limit gone over answers there; a bypass skips only the remaining
//! rules of its own configuration, so a project's bypass never skips the install's rules. A
//! `deny` (or a rate limit gone over) with an `actionDuration` holds the client's address to that
//! answer for that long: from the install, every project; from a project, that project. Holds and
//! rate-limit counts (`ratelimit.rs`) are this router's own, like Vercel's are its region's
//! (Global Accelerator keeps a client on the same router), and both are capped in number.
//!
//! An app's own rate-limit question (`/.well-known/forkrail/rate-limit-api/<id>`) comes from its
//! function, not a visitor: holds don't apply to it and it makes none.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use forkrail_common::firewall::{Config, Decision, Firewall, Limiter, NoLimits, Request, Verdict};
use forkrail_common::statics::firewall_key;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::ratelimit::{Counters, Scoped};
use crate::statics::Statics;

/// While a configuration can't be read, it's tried again this often.
const RETRY_EVERY: Duration = Duration::from_secs(5);
/// Addresses held off at once, over every project. Past it, new holds aren't kept (the rules
/// still deny each request) until old ones expire.
const MAX_HOLDS: usize = 200_000;
/// Compiled configurations kept by id beyond those in use, so switching back is instant.
const MAX_SPARE: usize = 64;

/// The configurations the routing table names: the install's, and each project's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refs {
    pub install: Option<String>,
    pub projects: BTreeMap<String, String>,
}

#[derive(Default)]
struct Loaded {
    install: Option<Arc<Firewall>>,
    projects: HashMap<String, Arc<Firewall>>,
}

/// Who a hold is for: the whole install, or one project; and the address, as `holder` has it.
type HoldKey = (Option<String>, IpAddr);

/// The address a hold is on: an IPv4 address, or the /64 an IPv6 one is in, as the shield and rate
/// limits count it (what one subscriber usually gets). Another address of the same /64 is no way out.
fn holder(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(std::net::Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
        v4 => v4,
    }
}

struct Hold {
    until: Instant,
    rule: String,
    answer: Held,
}

/// What a held address gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Deny,
    /// 429: a rate limit went over with its default answer.
    RateLimited,
    /// The challenge page, unless the request carries a pass.
    Challenge,
}

impl Held {
    /// How hard it holds: a solved challenge must never hide a deny or a rate limit.
    fn rank(self) -> u8 {
        match self {
            Held::Deny => 2,
            Held::RateLimited => 1,
            Held::Challenge => 0,
        }
    }
}

pub struct Firewalls {
    loaded: RwLock<Arc<Loaded>>,
    /// Compiled configurations by id.
    compiled: Mutex<HashMap<String, Arc<Firewall>>>,
    holds: Mutex<HashMap<HoldKey, Hold>>,
    /// Whether any hold exists, so requests skip the lock when none does.
    any_holds: std::sync::atomic::AtomicBool,
    pub counters: Arc<Counters>,
    /// The ids in force, for the first load to wait on.
    in_force: watch::Sender<Refs>,
}

/// What the firewall did with a request, for the access log and the answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Judged {
    pub decision: Decision,
    /// The rule, IP block or held rule that decided, by id.
    pub rule: Option<String>,
    /// Whether a persistent block answered.
    pub held: bool,
}

impl Judged {
    fn allow() -> Self {
        Self { decision: Decision::Allow, rule: None, held: false }
    }

    /// The action's name; empty when nothing matched.
    #[cfg(test)]
    pub fn action(&self) -> &'static str {
        self.decision.name()
    }
}

impl Firewalls {
    pub fn new() -> Arc<Self> {
        Self::with_buckets(crate::ratelimit::DEFAULT_BUCKETS)
    }

    /// With room for `buckets` rate-limit counts at once.
    pub fn with_buckets(buckets: usize) -> Arc<Self> {
        Arc::new(Self {
            loaded: RwLock::new(Arc::default()),
            compiled: Mutex::default(),
            holds: Mutex::default(),
            any_holds: Default::default(),
            counters: Arc::new(Counters::new(buckets)),
            in_force: watch::channel(Refs::default()).0,
        })
    }

    /// Whether `project`'s firewall (or the install's) needs the request's target path.
    pub fn uses_target_path(&self, project: &str) -> bool {
        let loaded = self.loaded.read().unwrap().clone();
        loaded.install.as_ref().is_some_and(|f| f.uses_target_path()) || loaded.projects.get(project).is_some_and(|f| f.uses_target_path())
    }

    /// Whether anything at all could act on a request to `project`.
    pub fn active(&self, project: &str) -> bool {
        if self.any_holds.load(std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
        let loaded = self.loaded.read().unwrap().clone();
        loaded.install.as_ref().is_some_and(|f| !f.is_empty()) || loaded.projects.get(project).is_some_and(|f| !f.is_empty())
    }

    /// Judge a request to `project` (empty for a host no project has). `count`: whether rate
    /// limits count it (port 80's requests aren't counted: they come back over HTTPS).
    pub fn judge(&self, project: &str, req: &Request, count: bool) -> Judged {
        let scope = Some(project.to_owned()).filter(|p| !p.is_empty());
        let api = req.rate_limit_api.is_some();
        if !api && let Some((rule, answer, left)) = self.held(&scope, req.ip) {
            let decision = match answer {
                Held::RateLimited => Some(Decision::RateLimited { persist: None, retry_after: left }),
                Held::Deny => Some(Decision::Deny { persist: None }),
                // Solved: judged as any request.
                Held::Challenge if req.challenge_passed => None,
                Held::Challenge => Some(Decision::Challenge { persist: None }),
            };
            if let Some(decision) = decision {
                return Judged { decision, rule: Some(rule), held: true };
            }
        }
        let loaded = self.loaded.read().unwrap().clone();
        let mut reported = Judged::allow();
        let firewalls = [(None, loaded.install.as_ref()), (scope.clone(), scope.as_ref().and_then(|p| loaded.projects.get(p)))];
        for (who, firewall) in firewalls {
            let Some(firewall) = firewall else { continue };
            let scoped = Scoped { counters: &self.counters, scope: who.as_deref() };
            let limiter: &dyn Limiter = if count { &scoped } else { &NoLimits };
            let Verdict { decision, rule } = firewall.check(req, limiter);
            if !api && let (Some(d), Some(rule)) = (decision.persist(), &rule) {
                let answer = match decision {
                    Decision::RateLimited { .. } => Held::RateLimited,
                    Decision::Challenge { .. } => Held::Challenge,
                    _ => Held::Deny,
                };
                self.hold(who.clone(), req.ip, d, rule, answer);
            }
            if decision.ends() {
                return Judged { decision, rule, held: false };
            }
            // The project's log or bypass is reported over the install's.
            if decision != Decision::Allow {
                reported = Judged { decision, rule, held: false };
            }
        }
        reported
    }

    /// Whether every request from `ip` to `host` of `project` is answered before the deployment
    /// is asked, whatever it carries: a hold on the address, the install's or the project's (a
    /// challenge's too: a pass can't be seen yet), or an IP block that denies or challenges it.
    /// The router starts no worker at such an address's TLS handshake (`early.rs`).
    pub fn stops_address(&self, project: &str, host: &str, ip: IpAddr) -> bool {
        let scope = Some(project.to_owned()).filter(|p| !p.is_empty());
        if self.held(&scope, ip).is_some() {
            return true;
        }
        let loaded = self.loaded.read().unwrap().clone();
        loaded.install.as_ref().is_some_and(|f| f.stops_address(ip, host)) || scope.as_ref().and_then(|p| loaded.projects.get(p)).is_some_and(|f| f.stops_address(ip, host))
    }

    /// Whether `project` (or the install) knows rate-limit API id `id`.
    pub fn knows_api(&self, project: &str, id: &str) -> bool {
        let loaded = self.loaded.read().unwrap().clone();
        loaded.install.as_ref().is_some_and(|f| f.knows_api(id)) || loaded.projects.get(project).is_some_and(|f| f.knows_api(id))
    }

    /// The rule holding `ip` off for `project` (or the install), if one is: its id, what it
    /// answers, and for how much longer.
    fn held(&self, project: &Option<String>, ip: IpAddr) -> Option<(String, Held, Duration)> {
        if !self.any_holds.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let ip = holder(ip);
        let now = Instant::now();
        let mut holds = self.holds.lock().unwrap();
        let keys: &[HoldKey] = if project.is_some() { &[(None, ip), (project.clone(), ip)] } else { &[(None, ip)] };
        // The install's and the project's: the one that holds hardest.
        let mut found: Option<(String, Held, Duration)> = None;
        for key in keys {
            match holds.get(key) {
                Some(h) if h.until > now => {
                    if found.as_ref().is_none_or(|(_, answer, _)| h.answer.rank() > answer.rank()) {
                        found = Some((h.rule.clone(), h.answer, h.until - now));
                    }
                }
                Some(_) => {
                    holds.remove(key);
                }
                None => {}
            }
        }
        if found.is_none() && holds.is_empty() {
            self.any_holds.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        found
    }

    fn hold(&self, project: Option<String>, ip: IpAddr, duration: Duration, rule: &str, answer: Held) {
        let now = Instant::now();
        let mut holds = self.holds.lock().unwrap();
        if holds.len() >= MAX_HOLDS {
            holds.retain(|_, h| h.until > now);
            if holds.len() >= MAX_HOLDS {
                warn!(holds = holds.len(), "firewall: too many addresses held off; this one isn't held");
                return;
            }
        }
        let key = (project, holder(ip));
        let until = now + duration;
        // A hold already in place stays if it holds harder, or as hard and longer.
        if holds.get(&key).is_some_and(|h| h.until > now && (h.answer.rank() > answer.rank() || (h.answer == answer && h.until >= until))) {
            return;
        }
        info!(project = key.0.as_deref(), ip = %key.1, rule, hold_s = duration.as_secs(), answer = ?answer, "firewall: holding an address off");
        holds.insert(key, Hold { until, rule: rule.to_owned(), answer });
        self.any_holds.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Follow the routing table's configurations until the router exits: read and compile each
    /// new one, and keep trying those that fail.
    pub async fn follow(self: Arc<Self>, mut refs: watch::Receiver<Refs>, statics: Arc<Statics>) {
        loop {
            let wanted = refs.borrow_and_update().clone();
            let complete = self.sync(&wanted, &statics).await;
            if complete {
                if refs.changed().await.is_err() {
                    return;
                }
            } else {
                tokio::select! {
                    changed = refs.changed() => if changed.is_err() { return },
                    _ = tokio::time::sleep(RETRY_EVERY) => {}
                }
            }
        }
    }

    /// Wait until the configurations the table names now are in force, or `wait` passes.
    pub async fn loaded(&self, refs: &watch::Receiver<Refs>, wait: Duration) -> bool {
        let wanted = refs.borrow().clone();
        let mut in_force = self.in_force.subscribe();
        tokio::time::timeout(wait, in_force.wait_for(|now| *now == wanted)).await.is_ok_and(|r| r.is_ok())
    }

    /// Bring the configurations in force to `wanted`; whether every one of them could be read.
    async fn sync(&self, wanted: &Refs, statics: &Statics) -> bool {
        let mut complete = true;
        let current = self.in_force.borrow().clone();
        let mut in_force = Refs::default();
        let mut next = Loaded::default();
        let previous = self.loaded.read().unwrap().clone();
        if let Some(id) = &wanted.install {
            match self.get(id, statics).await {
                Some(f) => {
                    next.install = Some(f);
                    in_force.install = Some(id.clone());
                }
                None => {
                    complete = false;
                    next.install = previous.install.clone();
                    in_force.install = current.install.clone();
                }
            }
        }
        for (project, id) in &wanted.projects {
            match self.get(id, statics).await {
                Some(f) => {
                    next.projects.insert(project.clone(), f);
                    in_force.projects.insert(project.clone(), id.clone());
                }
                None => {
                    complete = false;
                    if let (Some(f), Some(old)) = (previous.projects.get(project), current.projects.get(project)) {
                        next.projects.insert(project.clone(), f.clone());
                        in_force.projects.insert(project.clone(), old.clone());
                    }
                }
            }
        }
        if in_force != current {
            info!(
                install = in_force.install.as_deref().map(|id| &id[..12]),
                projects = in_force.projects.len(),
                "firewall configurations in force"
            );
        }
        *self.loaded.write().unwrap() = Arc::new(next);
        // Holds of projects that no longer have a firewall stay until they expire: the client was
        // refused for a reason, and the hold says how long.
        {
            let used: Vec<String> = in_force.install.iter().chain(in_force.projects.values()).cloned().collect();
            let mut compiled = self.compiled.lock().unwrap();
            if compiled.len() > used.len() + MAX_SPARE {
                compiled.retain(|id, _| used.contains(id));
            }
        }
        self.in_force.send_replace(in_force);
        complete
    }

    /// Configuration `id`, compiled; read once.
    async fn get(&self, id: &str, statics: &Statics) -> Option<Arc<Firewall>> {
        if let Some(f) = self.compiled.lock().unwrap().get(id) {
            return Some(f.clone());
        }
        let key = firewall_key(id);
        let bytes = match statics.object(&key).await {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(key, error = %e, "reading a firewall configuration failed; the one before stays in force");
                return None;
            }
        };
        let compiled = tokio::task::spawn_blocking(move || -> Result<Firewall, String> {
            let config: Config = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            Firewall::new(&config)
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        match compiled {
            Ok(f) => {
                let f = Arc::new(f);
                self.compiled.lock().unwrap().insert(id.to_owned(), f.clone());
                Some(f)
            }
            Err(e) => {
                warn!(key, error = %e, "a firewall configuration doesn't compile; the one before stays in force");
                None
            }
        }
    }

    #[cfg(test)]
    pub fn set(&self, install: Option<Firewall>, projects: Vec<(&str, Firewall)>) {
        let loaded = Loaded { install: install.map(Arc::new), projects: projects.into_iter().map(|(p, f)| (p.to_owned(), Arc::new(f))).collect() };
        *self.loaded.write().unwrap() = Arc::new(loaded);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn firewall(value: serde_json::Value) -> Firewall {
        let mut c: Config = serde_json::from_value(value).unwrap();
        c.assign_ids();
        Firewall::new(&c).unwrap()
    }

    fn judge(f: &Firewalls, project: &str, ip: &str, path: &str) -> Judged {
        judged(f, project, ip, path, false)
    }

    fn judged(f: &Firewalls, project: &str, ip: &str, path: &str, challenge_passed: bool) -> Judged {
        let headers = hyper::HeaderMap::new();
        f.judge(
            project,
            &Request {
                method: "GET",
                host: "app.forkrail.com",
                raw_path: path,
                target_path: None,
                query: None,
                headers: &headers,
                ip: ip.parse().unwrap(),
                protocol: "HTTP/2.0",
                scheme: "https",
                environment: "production",
                region: "us-east-1",
                continent: None,
                country: None,
                country_region: None,
                city: None,
                asn: None,
                ja4: None,
                rate_limit_api: None,
                challenge_passed,
            },
            true,
        )
    }

    fn rule(id: &str, path: &str, mitigate: serde_json::Value) -> serde_json::Value {
        json!({"id": id, "name": id, "conditionGroup": [{"conditions": [{"type": "path", "op": "eq", "value": path}]}], "action": {"mitigate": mitigate}})
    }

    #[test]
    fn install_rules_come_first_and_a_project_bypass_never_skips_them() {
        let f = Firewalls::new();
        f.set(
            Some(firewall(json!({"rules": [rule("install-deny", "/wp-login.php", json!({"action": "deny"})), rule("install-log", "/x", json!({"action": "log"}))]}))),
            vec![("app", firewall(json!({"rules": [rule("app-bypass", "/wp-login.php", json!({"action": "bypass"})), rule("app-log", "/x", json!({"action": "log"}))]})))],
        );
        let j = judge(&f, "app", "192.0.2.1", "/wp-login.php");
        assert_eq!((j.action(), j.rule.as_deref()), ("deny", Some("install-deny")));
        // Both log: the project's is reported.
        let j = judge(&f, "app", "192.0.2.1", "/x");
        assert_eq!((j.action(), j.rule.as_deref()), ("log", Some("app-log")));
        // Another project has the install's only.
        let j = judge(&f, "other", "192.0.2.1", "/x");
        assert_eq!((j.action(), j.rule.as_deref()), ("log", Some("install-log")));
        assert_eq!(judge(&f, "other", "192.0.2.1", "/").action(), "");
    }

    #[test]
    fn a_persistent_deny_holds_the_address_off_for_its_project() {
        let f = Firewalls::new();
        f.set(None, vec![("app", firewall(json!({"rules": [rule("probe", "/.env", json!({"action": "deny", "actionDuration": "10m"}))]})))]);
        assert_eq!(judge(&f, "app", "192.0.2.9", "/").action(), "");
        let j = judge(&f, "app", "192.0.2.9", "/.env");
        assert_eq!((j.action(), j.held), ("deny", false));
        // Every later request from it, on any path, until the hold ends.
        let j = judge(&f, "app", "192.0.2.9", "/");
        assert_eq!((j.action(), j.rule.as_deref(), j.held), ("deny", Some("probe"), true));
        assert!(f.active("other"));
        // Not another project, not another address.
        assert_eq!(judge(&f, "other", "192.0.2.9", "/").action(), "");
        assert_eq!(judge(&f, "app", "192.0.2.10", "/").action(), "");
        // An install hold covers every project.
        f.set(Some(firewall(json!({"rules": [rule("scan", "/admin", json!({"action": "deny", "actionDuration": "1h"}))]}))), vec![]);
        judge(&f, "x", "198.51.100.1", "/admin");
        assert!(judge(&f, "y", "198.51.100.1", "/").held);
        // Expired holds go.
        f.holds.lock().unwrap().values_mut().for_each(|h| h.until = Instant::now() - Duration::from_secs(1));
        assert!(!judge(&f, "app", "192.0.2.9", "/").held);
        assert!(!judge(&f, "y", "198.51.100.1", "/").held);
    }

    #[test]
    fn a_hold_covers_the_whole_ipv6_64_and_the_hardest_one_answers() {
        let f = Firewalls::new();
        f.set(
            Some(firewall(json!({"rules": [rule("puzzle", "/x", json!({"action": "challenge", "actionDuration": "1h"}))]}))),
            vec![("app", firewall(json!({"rules": [rule("probe", "/.env", json!({"action": "deny", "actionDuration": "10m"}))]})))],
        );
        judge(&f, "app", "2001:db8:1:2::1", "/.env");
        // Another address of the same /64 is held too; another /64 isn't.
        assert_eq!(judge(&f, "app", "2001:db8:1:2:ffff::9", "/").action(), "deny");
        assert_eq!(judge(&f, "app", "2001:db8:1:3::1", "/").action(), "");
        // An install challenge earned later, and solved, doesn't lift the project's deny.
        judge(&f, "other", "2001:db8:1:2::1", "/x");
        assert_eq!(judged(&f, "app", "2001:db8:1:2::1", "/", true).action(), "deny");
    }

    #[test]
    fn a_rate_limit_gone_over_holds_the_address_to_its_answer() {
        let rate = |over: &str, hold: Option<&str>| {
            let mut m = json!({"action": "rate_limit", "rateLimit": {"algo": "fixed_window", "window": 60, "limit": 2, "keys": ["ip"], "action": over}});
            if let Some(h) = hold {
                m["actionDuration"] = json!(h);
            }
            rule("login", "/login", m)
        };
        let f = Firewalls::new();
        f.set(None, vec![("app", firewall(json!({"rules": [rate("rate_limit", Some("10m"))]})))]);
        let actions: Vec<&str> = (0..3).map(|_| judge(&f, "app", "192.0.2.1", "/login").action()).collect();
        assert_eq!(actions, vec!["", "", "rate_limit"]);
        // Held to 429 on every path, with retry-after the hold's.
        let j = judge(&f, "app", "192.0.2.1", "/");
        assert!(j.held);
        assert!(matches!(j.decision, Decision::RateLimited { retry_after, .. } if retry_after > Duration::from_secs(590)));
        // Another address is counted apart.
        assert_eq!(judge(&f, "app", "192.0.2.2", "/login").action(), "");
        // Over with deny and a hold: 403 from then on.
        let f = Firewalls::new();
        f.set(None, vec![("app", firewall(json!({"rules": [rate("deny", Some("5m"))]})))]);
        for _ in 0..3 {
            judge(&f, "app", "192.0.2.1", "/login");
        }
        let j = judge(&f, "app", "192.0.2.1", "/");
        assert_eq!((j.action(), j.held), ("deny", true));
        // Without a hold, only the rule's own path is answered.
        let f = Firewalls::new();
        f.set(None, vec![("app", firewall(json!({"rules": [rate("rate_limit", None)]})))]);
        for _ in 0..3 {
            judge(&f, "app", "192.0.2.1", "/login");
        }
        assert_eq!(judge(&f, "app", "192.0.2.1", "/").action(), "");
        assert_eq!(judge(&f, "app", "192.0.2.1", "/login").action(), "rate_limit");
    }

    #[test]
    fn a_held_challenge_lets_a_solved_client_by() {
        let f = Firewalls::new();
        f.set(None, vec![("app", firewall(json!({"rules": [rule("probe", "/wp-login.php", json!({"action": "challenge", "actionDuration": "10m"}))]})))]);
        assert_eq!(judge(&f, "app", "192.0.2.3", "/wp-login.php").action(), "challenge");
        let j = judge(&f, "app", "192.0.2.3", "/");
        assert_eq!((j.action(), j.held), ("challenge", true));
        let headers = hyper::HeaderMap::new();
        let mut req = Request {
            method: "GET",
            host: "app.forkrail.com",
            raw_path: "/",
            target_path: None,
            query: None,
            headers: &headers,
            ip: "192.0.2.3".parse().unwrap(),
            protocol: "HTTP/2.0",
            scheme: "https",
            environment: "production",
            region: "us-east-1",
            continent: None,
            country: None,
            country_region: None,
            city: None,
            asn: None,
            ja4: None,
            rate_limit_api: None,
            challenge_passed: true,
        };
        assert_eq!(f.judge("app", &req, true).action(), "");
        req.raw_path = "/wp-login.php";
        assert_eq!(f.judge("app", &req, true).action(), "");
    }

    #[test]
    fn requests_not_counted_never_go_over() {
        let f = Firewalls::new();
        let m = json!({"action": "rate_limit", "rateLimit": {"algo": "fixed_window", "window": 60, "limit": 1, "keys": ["ip"]}});
        f.set(Some(firewall(json!({"rules": [rule("one", "/", m)]}))), vec![]);
        let headers = hyper::HeaderMap::new();
        let req = Request {
            method: "GET",
            host: "app.forkrail.com",
            raw_path: "/",
            target_path: None,
            query: None,
            headers: &headers,
            ip: "192.0.2.1".parse().unwrap(),
            protocol: "HTTP/1.1",
            scheme: "http",
            environment: "production",
            region: "us-east-1",
            continent: None,
            country: None,
            country_region: None,
            city: None,
            asn: None,
            ja4: None,
            rate_limit_api: None,
            challenge_passed: false,
        };
        for _ in 0..5 {
            assert_eq!(f.judge("app", &req, false).action(), "");
        }
        assert_eq!(f.judge("app", &req, true).action(), "");
        assert_eq!(f.judge("app", &req, true).action(), "rate_limit");
    }

    #[tokio::test]
    async fn configurations_load_from_the_store_and_a_bad_one_leaves_the_last_in_force() {
        let dir = std::env::temp_dir().join(format!("forkrail-fw-{}", forkrail_common::random_hex(4)));
        std::fs::create_dir_all(dir.join("static/firewall")).unwrap();
        let statics = Statics::new(crate::statics::Source::Dir(dir.clone()), 1 << 20);
        let good = json!({"rules": [rule("r1", "/blocked", json!({"action": "deny"}))]}).to_string();
        let good_id = "a".repeat(64);
        std::fs::write(dir.join(format!("static/firewall/{good_id}.json")), &good).unwrap();
        let f = Firewalls::new();
        let refs = Refs { install: None, projects: [("app".to_owned(), good_id.clone())].into() };
        assert!(f.sync(&refs, &statics).await);
        assert_eq!(judge(&f, "app", "192.0.2.1", "/blocked").action(), "deny");
        // A version that isn't there yet: the one before stays until it can be read.
        let next_id = "b".repeat(64);
        let refs2 = Refs { install: None, projects: [("app".to_owned(), next_id.clone())].into() };
        assert!(!f.sync(&refs2, &statics).await);
        assert_eq!(judge(&f, "app", "192.0.2.1", "/blocked").action(), "deny");
        assert_eq!(f.in_force.borrow().projects.get("app"), Some(&good_id));
        let open = json!({"rules": []}).to_string();
        std::fs::write(dir.join(format!("static/firewall/{next_id}.json")), open).unwrap();
        assert!(f.sync(&refs2, &statics).await);
        assert_eq!(judge(&f, "app", "192.0.2.1", "/blocked").action(), "");
        // Gone from the table: no firewall.
        assert!(f.sync(&Refs::default(), &statics).await);
        assert!(!f.active("app"));
        std::fs::remove_dir_all(dir).ok();
    }
}
