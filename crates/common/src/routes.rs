//! The routing table, `state/routes.json`: which deployment serves each hostname, and how to run
//! it. The CLI writes it and the router reads it, so both sides share these types and the same
//! hostname normalisation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::protection::{Protection, ProtectionConfig};

/// A request's maximum duration when the deployment doesn't set one, in seconds. Vercel's default
/// on its paid plans is 300 s.
pub const DEFAULT_MAX_DURATION_S: u64 = 300;
/// A function's memory size when its `vercel.json` doesn't set one, in MB.
pub const DEFAULT_MEMORY_MB: u32 = 640;
/// How long to keep a worker with nothing in flight, in ms, for a function of `memory_mb`: as long as
/// holding it costs less than starting another. Lambda bills $0.20 per million invocations, which buys
/// 15,360 / MB ms of an arm64 worker at $0.0000133334 per GB-second: 24 ms at 640 MB, 20 at 768, 60
/// at 256. A request that arrives within it also skips a thaw (about 20 ms), and the router a new
/// invocation's API call and handshake. Retiring at once instead made a busy API start a worker for
/// every 5 requests (2026-11-06: 5,863 a minute at 1,000 requests a second).
pub fn economic_idle_ms(memory_mb: u32) -> u64 {
    (15_360 / u64::from(memory_mb.max(128))).max(5)
}

/// Longest deployment id, so `<project>-<id>` fits one DNS label.
const MAX_ID_LEN: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routes {
    /// Deployment id → how to run it.
    #[serde(default)]
    pub deployments: BTreeMap<String, Deployment>,
    /// Normalised hostname → deployment id.
    #[serde(default)]
    pub hosts: BTreeMap<String, String>,
    /// Custom domains (`atlas.example.net`, `*.example.com`): each follows its project's
    /// production deployment in `hosts` (or a branch's newest preview, or redirects elsewhere), and
    /// the router keeps a certificate for it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub domains: BTreeMap<String, CustomDomain>,
    /// Project name → what applies to every deployment of it and changes without a redeploy:
    /// kept apart from deployment entries, whose change replaces their pools.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, ProjectRouting>,
    /// Deployment protection's keys and defaults (see `crate::protection`). None protects nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protection: Option<ProtectionConfig>,
    /// The install's own firewall, run before every project's: the id of its configuration,
    /// `static/firewall/<id>.json` (see `crate::firewall`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firewall: Option<String>,
    /// The install's system mitigations: paused or not, and the addresses they leave alone.
    #[serde(default, skip_serializing_if = "Mitigations::is_empty")]
    pub mitigations: Mitigations,
    /// Spend protection's install budget (C15). None: no install budget is enforced (a router
    /// says so), so a router never pauses what an older table didn't ask it to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<InstallSpend>,
}

/// The install's monthly spend budget: usage-based spend in the calendar month (UTC), at list
/// price, fixed costs left out. At it, every project not `exempt` is paused (`docs/deploying.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallSpend {
    /// This month's budget, in nano-dollars.
    pub budget_n: i64,
    /// The default nobody has set: a month that ends at 90% or more of it raises the next
    /// month's (`docs/spend-protection.md`). Setting a budget turns it off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    /// The last month (`YYYYMM`) whose end was looked at for the default's step: each is once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stepped: Option<u32>,
}

/// A project's spend settings (C15): its own monthly budget, and whether it's paused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSpend {
    /// This month's budget in nano-dollars; None: only the install's applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_n: Option<i64>,
    /// Paused, until a person resumes it: no function runs and no file is served for any of its
    /// hosts. Set by control when a budget is reached (or by hand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<SpendPause>,
    /// Not paused when the install's budget is reached (the dashboard, so the install can still
    /// be managed); its own budget still applies.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exempt: bool,
    /// What its builds cost this month: control adds each as it ends (the routers don't see
    /// builds), and every router counts it toward the project's budgets and the install's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builds: Option<BuildSpend>,
}

/// A month's builds of a project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSpend {
    /// `202611`: another month's count is last month's, and counts for nothing now.
    pub month: u32,
    /// Nano-dollars, at list price (the builder's memory for as long as it ran).
    pub cost_n: i64,
    /// The last builds counted (deployment id and builder), so a notice delivered twice counts once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counted: Vec<String>,
}

impl ProjectSpend {
    /// What its builds cost in `month`.
    pub fn builds_n(&self, month: u32) -> i64 {
        self.builds.as_ref().filter(|b| b.month == month).map_or(0, |b| b.cost_n)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpendPause {
    /// When (RFC 3339).
    pub at: String,
    /// `project_budget`, `install_budget` or `manual`.
    pub reason: String,
    /// The function's reserved concurrency before the pause set it to 0 (the dashboard's reserve,
    /// `deploy/aws/reserve.sh`), put back on resume; none, the function goes back to the shared
    /// pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved: Option<u32>,
}

/// Bypasses of the system mitigations an install or project may have, as Vercel allows 25.
pub const MAX_SYSTEM_BYPASS: usize = 100;

/// What changes at once, without publishing, in the firewall: the routers' system mitigations
/// (per-IP connection and request limits, automatic blocks) and attack mode. For the install, a
/// pause and bypasses for every host; for a project, attack mode and bypasses for its hosts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mitigations {
    /// The install's system mitigations do nothing until then (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_until: Option<u64>,
    /// A project's attack mode: every visitor must pass a challenge until then (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attack_mode_until: Option<u64>,
    /// Who turned attack mode on or the mitigations off, and when (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_bypass: Vec<SystemBypass>,
    /// The install's limits on each address, in place of the routers' own (`SHIELD_DEFAULTS`
    /// unless a router's environment says otherwise). An install's only; each unset field keeps
    /// the router's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ShieldLimits>,
}

/// The system mitigations' limits on one address, over every project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShieldLimits {
    /// Connections open at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<u32>,
    /// Requests in 10 seconds; past it, the address is blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<u32>,
    /// How long a block lasts, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_s: Option<u32>,
}

/// The routers' own limits, where neither the table nor their environment sets one: connections,
/// requests in 10 s, block seconds.
pub const SHIELD_DEFAULTS: ShieldLimits = ShieldLimits { connections: Some(256), requests: Some(5_000), block_s: Some(600) };
/// What each limit may be set to.
pub const SHIELD_CONNECTIONS: std::ops::RangeInclusive<u32> = 16..=65_536;
pub const SHIELD_REQUESTS: std::ops::RangeInclusive<u32> = 100..=1_000_000;
pub const SHIELD_BLOCK_S: std::ops::RangeInclusive<u32> = 60..=86_400;

impl ShieldLimits {
    pub fn is_empty(&self) -> bool {
        self.connections.is_none() && self.requests.is_none() && self.block_s.is_none()
    }

    pub fn validate(&self) -> Result<(), String> {
        let check = |v: Option<u32>, range: &std::ops::RangeInclusive<u32>, what: &str| match v {
            Some(n) if !range.contains(&n) => Err(format!("{what}: from {} to {}, not {n}", range.start(), range.end())),
            _ => Ok(()),
        };
        check(self.connections, &SHIELD_CONNECTIONS, "connections per address")?;
        check(self.requests, &SHIELD_REQUESTS, "requests per address in 10 seconds")?;
        check(self.block_s, &SHIELD_BLOCK_S, "block seconds")
    }
}

impl Mitigations {
    pub fn is_empty(&self) -> bool {
        self.paused_until.is_none() && self.attack_mode_until.is_none() && self.system_bypass.is_empty() && self.limits.is_none()
    }

    /// Whether attack mode is on at `now` (Unix seconds).
    pub fn attack_mode(&self, now: u64) -> bool {
        self.attack_mode_until.is_some_and(|until| until > now)
    }

    /// Whether the mitigations are paused at `now`.
    pub fn paused(&self, now: u64) -> bool {
        self.paused_until.is_some_and(|until| until > now)
    }

    fn validate(&self) -> Result<(), String> {
        if self.system_bypass.len() > MAX_SYSTEM_BYPASS {
            return Err(format!("at most {MAX_SYSTEM_BYPASS} system bypasses"));
        }
        if let Some(limits) = &self.limits {
            limits.validate()?;
        }
        let mut ids = std::collections::HashSet::new();
        for b in &self.system_bypass {
            if b.id.is_empty() || b.id.len() > 64 || !b.id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') || !ids.insert(&b.id) {
                return Err(format!("system bypass {:?}: a unique id of letters, digits, _ and -", b.id));
            }
            if !valid_cidr(&b.source_ip) {
                return Err(format!("system bypass {}: {:?} isn't an IP address or CIDR range", b.id, b.source_ip));
            }
            if b.domain != "*" && !valid_domain_entry(&b.domain) {
                return Err(format!("system bypass {}: domain {:?}: `*`, a domain or `*.` and one", b.id, b.domain));
            }
            if b.note.len() > 500 {
                return Err(format!("system bypass {}: a note of at most 500 characters", b.id));
            }
        }
        Ok(())
    }
}

/// An address or range the system mitigations leave alone (never the custom rules).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemBypass {
    pub id: String,
    /// An address or a CIDR range.
    pub source_ip: String,
    /// `*`: every host (of the project, or of the install); else one domain, or `*.` and one.
    #[serde(default = "every_host")]
    pub domain: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
}

fn every_host() -> String {
    "*".to_owned()
}

/// `1.2.3.4`, `1.2.3.0/24`, `2001:db8::/32`.
fn valid_cidr(text: &str) -> bool {
    let (addr, len) = match text.split_once('/') {
        Some((a, l)) => (a, Some(l)),
        None => (text, None),
    };
    let Ok(addr) = addr.parse::<std::net::IpAddr>() else { return false };
    let max = if addr.is_ipv4() { 32 } else { 128 };
    len.is_none_or(|l| l.parse::<u8>().is_ok_and(|l| l <= max))
}

/// A project's own routing, published from the dashboard, CLI or API: run on every request to
/// any of its deployments, before the deployment's own rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRouting {
    /// Its bulk redirects: the id of their table, `static/redirects/<id>` (see `rules::bulk`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirects: Option<String>,
    /// Its routing rules: the id of their program, `static/rules/<id>.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<String>,
    /// Who may open its deployments; the install's default (`ProtectionConfig::default`) if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protection: Option<Protection>,
    /// Its firewall: the id of its configuration, `static/firewall/<id>.json`, run on every
    /// request to any of its deployments and domains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firewall: Option<String>,
    /// Its attack mode and system bypasses.
    #[serde(default, skip_serializing_if = "Mitigations::is_empty")]
    pub mitigations: Mitigations,
    /// Its skew protection. None: never set, so off (a new project's first build may turn it on,
    /// as Vercel does for supported frameworks); `enabled: false` is an explicit off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skew: Option<SkewProtection>,
    /// Its latest purges of the response cache by tag (`crate::purge`), newest last, at most
    /// `purge::MAX_TABLE_PURGES`: every router applies each as it sees it, and remembers it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub purges: Vec<crate::purge::Purge>,
    /// Environments the routers keep alive for its production deployment in each region it runs
    /// in, so a request after a quiet spell finds one ready instead of waiting for a cold start:
    /// a number (0 keeps none) or `auto`. None: the router's own default (`FORKRAIL_KEEP_WARM`).
    /// Routers from before it ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_warm: Option<KeepWarm>,
    /// Its spend budget and pause (C15). Routers from before it ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<ProjectSpend>,
    /// Its Web Analytics, which the routers collect (the router's `insights.rs`). None: never
    /// set, so off, as on Vercel: the script is an empty one and beacons are dropped. Routers
    /// from before it ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analytics: Option<WebAnalytics>,
}

/// A project's Web Analytics setting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebAnalytics {
    pub enabled: bool,
}

/// The most environments a project may have kept alive per region: each round sends that many
/// invocations at once, out of the account's Lambda concurrency.
pub const KEEP_WARM_MAX: u32 = 20;

/// How many environments are kept alive: a number, or `auto`, as many as the deployment had in
/// use at once at its busiest in the last day (at least 1, at most `KEEP_WARM_MAX`), so a burst
/// that comes back finds them ready. In JSON a number or `"auto"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepWarm {
    Count(u32),
    Auto,
}

impl KeepWarm {
    /// Whether it keeps any.
    pub fn on(self) -> bool {
        self != KeepWarm::Count(0)
    }
}

impl std::fmt::Display for KeepWarm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeepWarm::Count(n) => write!(f, "{n}"),
            KeepWarm::Auto => f.write_str("auto"),
        }
    }
}

impl std::str::FromStr for KeepWarm {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "auto" => Ok(KeepWarm::Auto),
            n => n.parse().map(KeepWarm::Count).map_err(|_| format!("{s:?}: a number of environments or `auto`")),
        }
    }
}

impl Serialize for KeepWarm {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            KeepWarm::Count(n) => s.serialize_u32(*n),
            KeepWarm::Auto => s.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for KeepWarm {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Count(u32),
            Word(String),
        }
        match Either::deserialize(d)? {
            Either::Count(n) => Ok(KeepWarm::Count(n)),
            Either::Word(w) if w == "auto" => Ok(KeepWarm::Auto),
            Either::Word(w) => Err(serde::de::Error::custom(format!("keep_warm {w:?}: a number or \"auto\""))),
        }
    }
}

impl ProjectRouting {
    pub fn is_empty(&self) -> bool {
        !self.has_rules() && self.protection.is_none() && self.firewall.is_none() && self.mitigations.is_empty() && self.skew.is_none() && self.purges.is_empty() && self.keep_warm.is_none() && self.spend.is_none() && self.analytics.is_none()
    }

    /// Whether it has redirects or rules to run on its requests.
    pub fn has_rules(&self) -> bool {
        self.redirects.is_some() || self.routing.is_some()
    }

    /// Its skew protection, when it's on.
    pub fn skew_on(&self) -> Option<&SkewProtection> {
        self.skew.as_ref().filter(|s| s.enabled)
    }

    /// Whether the routers collect its page views and events.
    pub fn analytics_on(&self) -> bool {
        self.analytics.as_ref().is_some_and(|a| a.enabled)
    }
}

/// How long a deployment can be pinned to by default, from its creation: Vercel's on Pro, 12 hours.
pub const SKEW_DEFAULT_MAX_AGE_S: u64 = 12 * 3_600;
/// Googlebot and Bingbot may render a page long after they fetched it, so Vercel lets them reach a
/// deployment for 60 days whatever the project's maximum age.
pub const SKEW_CRAWLER_MAX_AGE_S: u64 = 60 * 86_400;
/// What the maximum age may be set to: a minute, up to the longest a retention policy keeps a
/// deployment (ten years). A deployment its policy deleted can't be reached however long it is.
pub const SKEW_MAX_AGE_S: std::ops::RangeInclusive<u64> = 60..=3_650 * 86_400;
/// Domains allowed to send skew-protected requests from another site, as Vercel allows 12.
pub const SKEW_MAX_ALLOWED_DOMAINS: usize = 12;

/// Skew protection (Vercel's, `/docs/skew-protection`): a request naming one of the project's
/// deployments (`?dpl=`, `x-deployment-id` or the `__vdpl` cookie) is served by it while it's
/// younger than the maximum age, so a page keeps talking to the deployment that served it after a
/// newer one goes live. Changes apply at once; the build-time variable
/// `VERCEL_SKEW_PROTECTION_ENABLED` waits for the next deployment, as on Vercel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkewProtection {
    pub enabled: bool,
    /// From a deployment's creation, in seconds; `SKEW_DEFAULT_MAX_AGE_S` if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_s: Option<u64>,
    /// No deployment created at or before this one's creation is pinned to (a bad deployment and
    /// those older than it, once a fix is out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<SkewThreshold>,
    /// Sites whose pages may send skew-protected requests here (`app.example.com`, `*.example.com`
    /// one label deep, or `*`): other sites' requests are served by the current deployment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
}

/// A skew protection threshold: the deployment, and when it was created (Unix seconds), kept so
/// the threshold holds after that deployment is deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkewThreshold {
    pub deployment: String,
    pub created: u64,
}

impl SkewProtection {
    /// The maximum age in force, in seconds.
    pub fn max_age(&self) -> u64 {
        self.max_age_s.unwrap_or(SKEW_DEFAULT_MAX_AGE_S)
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(s) = self.max_age_s
            && !SKEW_MAX_AGE_S.contains(&s)
        {
            return Err(format!("skew protection's maximum age: from {} to {} seconds, not {s}", SKEW_MAX_AGE_S.start(), SKEW_MAX_AGE_S.end()));
        }
        if let Some(t) = &self.threshold
            && !valid_id(&t.deployment)
        {
            return Err(format!("skew protection threshold {:?} isn't a deployment id", t.deployment));
        }
        if self.allowed_domains.len() > SKEW_MAX_ALLOWED_DOMAINS {
            return Err(format!("skew protection allows at most {SKEW_MAX_ALLOWED_DOMAINS} domains for cross-site requests"));
        }
        if let Some(d) = self.allowed_domains.iter().find(|d| !valid_skew_domain(d)) {
            return Err(format!("skew protection's allowed domain {d:?}: `*`, a lowercase domain name, or `*.` and one"));
        }
        Ok(())
    }
}

/// An allowed domain for cross-site skew-protected requests: `*`, `localhost`, a domain name, or
/// a wildcard one label above one.
pub fn valid_skew_domain(name: &str) -> bool {
    name == "*" || name == "localhost" || valid_domain_entry(name)
}

/// A custom domain of a project. Entries from before C2 have only `project`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomDomain {
    pub project: String,
    /// When it was added (RFC 3339), and who added it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
    /// It serves nothing itself: every request is redirected to another of the project's domains,
    /// path and query kept. It has no entry in `hosts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<DomainRedirect>,
    /// It follows this git branch's newest ready preview, as the branch's
    /// `<project>-git-<branch>` host does, instead of production.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// When someone last asked for its DNS and certificate to be checked again (RFC 3339): a new
    /// value has the issuing router look again at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
}

impl CustomDomain {
    pub fn new(project: &str) -> Self {
        CustomDomain { project: project.to_owned(), ..Default::default() }
    }

    /// Whether it follows production: neither a redirect nor a branch's.
    pub fn production(&self) -> bool {
        self.redirect.is_none() && self.branch.is_none()
    }
}

/// Where a redirecting domain sends its requests: `https://<to><path>?<query>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainRedirect {
    pub to: String,
    /// 301, 302, 307 or 308.
    #[serde(default = "default_redirect_status")]
    pub status: u16,
}

fn default_redirect_status() -> u16 {
    308
}

/// The statuses a domain may redirect with, as Vercel offers them.
pub const DOMAIN_REDIRECT_STATUSES: [u16; 4] = [301, 302, 307, 308];

/// What the router needs to run one deployment. Deployments are immutable: a new one gets a new id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub project: String,
    /// The Lambda function, and the version that freezes this deployment's code and configuration.
    /// Both empty for a deployment of static files alone, which has no function.
    #[serde(default)]
    pub function: String,
    #[serde(default)]
    pub qualifier: String,
    /// The region `qualifier` was published in. Entries from before regions were recorded have
    /// none: a router takes its home region for them (`FORKRAIL_HOME_REGION`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The same deployment in other regions: region → version of `function` there, published from
    /// the same package with the same configuration. A router runs the version in its own region
    /// when there is one (see `placement`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub replicas: BTreeMap<String, String>,
    /// Concurrent requests per worker; the router's default if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inflight: Option<u32>,
    /// CPU budget per worker (see `FORKRAIL_CPU_TARGET_PCT`); the router's default if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_target_pct: Option<u32>,
    /// Workers the router may run for this deployment at once; the router's default if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_workers: Option<u32>,
    /// Retire a worker this long after its last request, in ms (see `FORKRAIL_IDLE_MS`); the
    /// router's default if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u64>,
    /// Free slots to keep ahead of demand (see `FORKRAIL_HEADROOM_PCT`); the router's default if
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headroom_pct: Option<u32>,
    /// A request taking longer gets 504 `FUNCTION_INVOCATION_TIMEOUT`.
    #[serde(default = "default_max_duration")]
    pub max_duration_s: u64,
    /// The function's memory size, for display. The router learns the real limit from the worker.
    #[serde(default = "default_memory")]
    pub memory_mb: u32,
    /// `production` or `preview`, as deployed (`deploy --prod` or not); for the logs. Entries from
    /// before it have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The Lambda runtime (`python3.13`, `nodejs24.x`) and the app's entry file (`api/main.py`), for
    /// the logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// Local spawner only: environment for the worker processes, such as `FORKRAIL_APP_DIR` and
    /// `FORKRAIL_APP_ENTRY`. A Lambda version carries its own environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The deployment's static files, served by the router before the function: the id of their
    /// manifest (see `crate::statics`), in each region's bucket. Routers from before it ignore it,
    /// and the function serves its own copies.
    #[serde(default, rename = "static", skip_serializing_if = "Option::is_none")]
    pub static_files: Option<String>,
    /// Private addresses the app may reach through its router, by name (`analytics`). Each router
    /// maps names to addresses its own network reaches (`FORKRAIL_EGRESS_TARGETS`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<String>,
    /// The deployment's routing rules (`vercel.json`'s redirects, rewrites, headers and the
    /// like): the SHA-256 of their program, `static/rules/<id>.json` in each region's bucket (see
    /// `crate::rules`). None routes as deployments always have: static files, then the function.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<String>,
    /// Its Image Optimization settings (`vercel.json`'s or its Build Output's `images`): the
    /// SHA-256 of their JSON, `static/images/<id>.json` in each region's bucket (see
    /// `crate::images`). With them, `/_vercel/image` serves optimized images; without, the path is
    /// the function's like any other. Routers from before it ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<String>,
    /// Its schedules (`vercel.json`'s `crons` and `schedules`): a router runs one when a message
    /// for its key arrives and this deployment is its project's production (see
    /// `crate::schedules`). Routers from before them ignore them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<crate::schedules::Schedule>,
    /// When it was created (Unix seconds), for skew protection's maximum age. Entries from before
    /// it have none and are never pinned to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<u64>,
    /// The name its framework gave it (a Build Output's `deploymentId`, Next.js's `deploymentId`
    /// setting), when that isn't its id: what its pages put in `?dpl=` and `x-deployment-id`, so
    /// skew protection takes it as this deployment's (within its project). Routers from before it
    /// ignore it, and such requests are served by the current deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skew_id: Option<String>,
    /// A Next.js deployment: how it runs and what the router needs to know of its build (N3).
    /// Routers from before it ignore it and would serve the deployment as any function's,
    /// with their response cache in front of its middleware: they must be released before
    /// anything publishes a Next.js deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<NextRef>,
}

/// How a Next.js deployment runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NextShape {
    /// Next's own standalone server (`server.js`): routing, middleware and rendering are Next's.
    Server,
    /// Next's entrypoints called directly (N8, Next 16.2+).
    Handlers,
}

/// A Next.js deployment's entry in the table: kept small, since a site may have thousands of
/// prerenders; the facts are in the static store (`crate::next::NextConfig`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextRef {
    pub shape: NextShape,
    /// The SHA-256 of its config, `static/next/<id>.json` in each region's bucket.
    pub config: String,
}

/// Whether `name` may be a framework's own deployment id (`Deployment::skew_id`): 1-64 of ASCII
/// letters, digits, `-` and `_`, as such ids travel in URLs, headers and cookies unescaped.
pub fn valid_skew_id(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Deployment {
    /// Run one request per worker, with no spares: concurrent compute off, classic Lambda, what
    /// `"fluid": false` asks for on Vercel. Concurrent requests get environments of their own, and
    /// so outbound IPs of their own; one request's failure takes no other with it. A worker is kept only as long as that's
    /// cheaper than a new invocation (`economic_idle_ms`).
    pub fn set_single(&mut self) {
        self.max_inflight = Some(1);
        self.idle_ms = Some(economic_idle_ms(self.memory_mb));
        self.headroom_pct = Some(0);
        self.cpu_target_pct = None;
    }

    /// Its project's cap on requests per worker (`None`: the router's default), on a deployment
    /// that runs concurrent compute; one that runs one request per worker, or has no function, is
    /// left as it is. Whether it changed.
    pub fn follow_max_inflight(&mut self, max_inflight: Option<u32>) -> bool {
        if self.single() || !self.has_function() || self.max_inflight == max_inflight {
            return false;
        }
        self.max_inflight = max_inflight;
        true
    }

    /// Whether it has a function: a deployment of static files alone doesn't.
    pub fn has_function(&self) -> bool {
        !self.function.is_empty()
    }

    /// Whether it runs one request per worker.
    pub fn single(&self) -> bool {
        self.max_inflight == Some(1)
    }

    /// The region `qualifier` lives in, with `home` for an entry that names none.
    pub fn home<'a>(&'a self, home: &'a str) -> &'a str {
        self.region.as_deref().unwrap_or(home)
    }

    /// Where a router in region `here` runs it, as (region, version): its version in `here` when it
    /// has one, otherwise the one in the region nearest `here`, reached from afar, where a region
    /// with no router of its own (`routed` says which have one) counts as farther
    /// (`crate::regions::nearest_reached`; its own region when none is nearer). In a region with a
    /// router, that router runs it; in one without, its workers dial back across regions.
    /// `home` stands in for an entry without a region.
    pub fn placement<'a>(&'a self, here: &'a str, home: &'a str, routed: impl Fn(&str) -> bool) -> (&'a str, &'a str) {
        let own = self.home(home);
        if own == here {
            return (here, &self.qualifier);
        }
        if let Some(version) = self.replicas.get(here) {
            return (here, version);
        }
        let region = crate::regions::nearest_reached(here, self.versions(home).map(|(r, _)| r), routed).unwrap_or(own);
        match self.replicas.get_key_value(region) {
            Some((region, version)) => (region, version),
            None => (own, &self.qualifier),
        }
    }

    /// Every region it's published in, with its version there: its own first.
    pub fn versions<'a>(&'a self, home: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        std::iter::once((self.home(home), self.qualifier.as_str()))
            .chain(self.replicas.iter().map(|(r, v)| (r.as_str(), v.as_str())))
    }
}

fn default_max_duration() -> u64 {
    DEFAULT_MAX_DURATION_S
}

fn default_memory() -> u32 {
    DEFAULT_MEMORY_MB
}

impl Routes {
    /// The deployment serving `host`, which must already be normalised: its own entry, else a
    /// wildcard domain's one label up (`*.example.com` serves `a.example.com`, not `a.b.example.com`).
    pub fn lookup(&self, host: &str) -> Option<(&str, &Deployment)> {
        let id = match self.hosts.get(host) {
            Some(id) => id,
            None => self.hosts.get(&wildcard_above(host)?)?,
        };
        self.deployments.get(id).map(|d| (id.as_str(), d))
    }

    /// The custom domain `host` is, by name or by a wildcard one label up, and its entry's name.
    pub fn domain(&self, host: &str) -> Option<(&str, &CustomDomain)> {
        if let Some((name, d)) = self.domains.get_key_value(host) {
            return Some((name, d));
        }
        let (name, d) = self.domains.get_key_value(&wildcard_above(host)?)?;
        Some((name, d))
    }

    /// Check what a hand edit or another tool could get wrong: every host points at a known
    /// deployment, and ids and hostnames have the shapes the router relies on.
    pub fn validate(&self) -> Result<(), String> {
        for (id, deployment) in &self.deployments {
            if !valid_id(id) {
                return Err(format!("deployment id {id:?}: use 1-{MAX_ID_LEN} of a-z, 0-9 and -"));
            }
            if deployment.function.is_empty() != deployment.qualifier.is_empty() {
                return Err(format!("deployment {id}: function and qualifier go together"));
            }
            if !deployment.has_function() && (deployment.static_files.is_none() || !deployment.replicas.is_empty()) {
                return Err(format!("deployment {id}: one without a function needs static files, and has no replicas"));
            }
            if let Some(rules) = &deployment.rules
                && !crate::statics::valid_manifest_id(rules)
            {
                return Err(format!("deployment {id}: rules {rules:?} isn't a SHA-256"));
            }
            let mut keys = std::collections::BTreeSet::new();
            for s in &deployment.schedules {
                if s.key.is_empty() || s.key.len() > 64 || !keys.insert(s.key.as_str()) || !s.path.starts_with('/') {
                    return Err(format!("deployment {id}: bad or repeated schedule {:?}", s.key));
                }
            }
            if deployment.region.as_deref() == Some("") {
                return Err(format!("deployment {id}: region is empty"));
            }
            if let Some(name) = &deployment.skew_id
                && !valid_skew_id(name)
            {
                return Err(format!("deployment {id}: skew_id {name:?}: use 1-64 of A-Z, a-z, 0-9, - and _"));
            }
            for (region, version) in &deployment.replicas {
                if region.is_empty() || version.is_empty() || deployment.region.as_deref() == Some(region.as_str()) {
                    return Err(format!("deployment {id}: bad replica {region:?} → {version:?}"));
                }
            }
            if deployment.max_inflight == Some(0) || deployment.max_workers == Some(0) {
                return Err(format!("deployment {id}: max_inflight and max_workers must be at least 1"));
            }
            if deployment.max_duration_s == 0 {
                return Err(format!("deployment {id}: max_duration_s must be at least 1"));
            }
            if let Some(manifest) = &deployment.static_files
                && !crate::statics::valid_manifest_id(manifest)
            {
                return Err(format!("deployment {id}: static manifest {manifest:?} isn't a SHA-256"));
            }
            if let Some(next) = &deployment.next {
                if !crate::statics::valid_manifest_id(&next.config) {
                    return Err(format!("deployment {id}: Next.js config {:?} isn't a SHA-256", next.config));
                }
                if !deployment.has_function() {
                    return Err(format!("deployment {id}: a Next.js deployment needs its function"));
                }
            }
            if let Some(name) = deployment.egress.iter().find(|n| !valid_egress_name(n)) {
                return Err(format!("deployment {id}: egress {name:?}: use 1-32 of a-z, 0-9 and -"));
            }
        }
        for (host, id) in &self.hosts {
            if normalize_host(host) != *host || host.is_empty() {
                return Err(format!("host {host:?} isn't normalised (lowercase, no port)"));
            }
            if !self.deployments.contains_key(id) {
                return Err(format!("host {host} points at unknown deployment {id}"));
            }
        }
        for (project, routing) in &self.projects {
            for id in [&routing.redirects, &routing.routing, &routing.firewall].into_iter().flatten() {
                if !crate::statics::valid_manifest_id(id) {
                    return Err(format!("project {project}: {id:?} isn't a SHA-256"));
                }
            }
            if let Some(protection) = &routing.protection {
                protection.validate().map_err(|e| format!("project {project}: {e}"))?;
            }
            routing.mitigations.validate().map_err(|e| format!("project {project}: {e}"))?;
            if let Some(skew) = &routing.skew {
                skew.validate().map_err(|e| format!("project {project}: {e}"))?;
            }
            if let Some(KeepWarm::Count(n)) = routing.keep_warm
                && n > KEEP_WARM_MAX
            {
                return Err(format!("project {project}: keep_warm is at most {KEEP_WARM_MAX}"));
            }
            if routing.mitigations.limits.is_some() {
                return Err(format!("project {project}: the system mitigations' limits are the install's, not a project's"));
            }
        }
        self.mitigations.validate()?;
        if let Some(protection) = &self.protection {
            protection.validate()?;
        }
        if let Some(id) = &self.firewall
            && !crate::statics::valid_manifest_id(id)
        {
            return Err(format!("firewall {id:?} isn't a SHA-256"));
        }
        for (domain, custom) in &self.domains {
            if !valid_domain_entry(domain) {
                return Err(format!("custom domain {domain:?}: use a lowercase DNS name with at least two labels, or *. and one"));
            }
            if let Some(id) = self.hosts.get(domain)
                && let Some(d) = self.deployments.get(id)
                && d.project != custom.project
            {
                return Err(format!("custom domain {domain} belongs to {}, but points at a deployment of {}", custom.project, d.project));
            }
            if let Some(redirect) = &custom.redirect {
                if !DOMAIN_REDIRECT_STATUSES.contains(&redirect.status) {
                    return Err(format!("custom domain {domain}: redirect status {} isn't 301, 302, 307 or 308", redirect.status));
                }
                if !valid_domain(&redirect.to) || redirect.to == *domain {
                    return Err(format!("custom domain {domain}: redirect to {:?} isn't another domain name", redirect.to));
                }
                if custom.branch.is_some() {
                    return Err(format!("custom domain {domain}: a redirect has no branch"));
                }
            }
            if custom.branch.as_deref().is_some_and(|b| b.is_empty() || b.len() > 255 || b.chars().any(char::is_control)) {
                return Err(format!("custom domain {domain}: branch {:?} isn't a branch name", custom.branch));
            }
        }
        Ok(())
    }

    /// Hostnames pointing at deployment `id`.
    pub fn hosts_of<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.hosts.iter().filter(move |(_, d)| *d == id).map(|(h, _)| h.as_str())
    }
}

/// Whether `id` can be a deployment id: lowercase letters, digits and dashes. No dots, because
/// worker ids are `<deployment>.<hex>`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `name` can name an egress target: 1-32 of a-z, 0-9 and -.
pub fn valid_egress_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `name` can be a custom domain: a normalised DNS name of two or more labels, each 1-63 of
/// a-z, 0-9 and -, not starting or ending with -. No wildcards, no IP addresses.
pub fn valid_domain(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    name.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
        && !labels.last().is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `name` can be a custom domain's entry: a domain name (`valid_domain`), or a wildcard
/// one label above at least two (`*.example.com`, not `*.com`).
pub fn valid_domain_entry(name: &str) -> bool {
    match name.strip_prefix("*.") {
        Some(rest) => valid_domain(rest),
        None => valid_domain(name),
    }
}

/// The wildcard that would cover `host` one label up: `*.example.com` for `a.example.com`. None
/// for a name of fewer than three labels, which no wildcard entry covers.
pub fn wildcard_above(host: &str) -> Option<String> {
    let (_, parent) = host.split_once('.')?;
    parent.contains('.').then(|| format!("*.{parent}"))
}

/// The name a custom domain's ACME challenge is delegated to, in the install's own zone:
/// `_acme-challenge.<domain>` CNAME `<hash>._acme.<base>`. The same for `example.com` and
/// `*.example.com`, whose challenges have the same name. `_acme` is no project's host, so the
/// records under it never hide one from the base domain's wildcard.
#[cfg(feature = "domains")]
pub fn acme_delegation(domain: &str, base: &str) -> String {
    use sha2::Digest;
    let name = domain.strip_prefix("*.").unwrap_or(domain);
    let digest = sha2::Sha256::digest(format!("forkrail-acme:{name}").as_bytes());
    format!("{}._acme.{base}", crate::hex(&digest[..8]))
}

/// Whether `host` is exactly one label under `base`, as `<project>.forkrail.com` and
/// `<project>-<id>.forkrail.com` are: what the base domain's wildcard certificate covers. Both
/// must already be normalised.
pub fn one_label_under(host: &str, base: &str) -> bool {
    host.strip_suffix(base)
        .and_then(|rest| rest.strip_suffix('.'))
        .is_some_and(|label| !label.is_empty() && !label.contains('.'))
}

/// A `Host` header as the routing table keys it: lowercase, without the port or a trailing dot.
/// IPv6 literals keep their brackets.
pub fn normalize_host(host: &str) -> String {
    let host = host.trim();
    let name = if host.starts_with('[') {
        // [v6]:port
        host.find(']').map_or(host, |end| &host[..=end])
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
            _ => host,
        }
    };
    name.trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(project: &str, qualifier: &str) -> Deployment {
        Deployment {
            project: project.into(),
            function: format!("forkrail-app-{project}"),
            qualifier: qualifier.into(),
            region: None,
            replicas: BTreeMap::new(),
            max_inflight: None,
            cpu_target_pct: None,
            max_workers: Some(5),
            idle_ms: None,
            headroom_pct: None,
            max_duration_s: 60,
            memory_mb: 640,
            target: None,
            runtime: None,
            entry: None,
            env: BTreeMap::new(),
            static_files: None,
            egress: Vec::new(),
            rules: None,
            images: None,
            schedules: Vec::new(),
            created: None,
            skew_id: None,
            next: None,
        }
    }

    #[test]
    fn keep_warm_is_a_number_or_auto() {
        let parse = |json: &str| serde_json::from_str::<ProjectRouting>(json).map(|p| p.keep_warm);
        assert_eq!(parse(r#"{"keep_warm": 3}"#).unwrap(), Some(KeepWarm::Count(3)));
        assert_eq!(parse(r#"{"keep_warm": "auto"}"#).unwrap(), Some(KeepWarm::Auto));
        assert_eq!(parse(r#"{}"#).unwrap(), None);
        assert!(parse(r#"{"keep_warm": "lots"}"#).is_err());
        assert!(parse(r#"{"keep_warm": -1}"#).is_err());
        let auto = ProjectRouting { keep_warm: Some(KeepWarm::Auto), ..Default::default() };
        assert_eq!(serde_json::to_value(&auto).unwrap(), serde_json::json!({"keep_warm": "auto"}));
        assert_eq!(serde_json::to_value(ProjectRouting { keep_warm: Some(KeepWarm::Count(0)), ..Default::default() }).unwrap(), serde_json::json!({"keep_warm": 0}));
        assert_eq!("auto".parse::<KeepWarm>(), Ok(KeepWarm::Auto));
        assert_eq!("12".parse::<KeepWarm>(), Ok(KeepWarm::Count(12)));
        assert!("x".parse::<KeepWarm>().is_err());
        assert!(!KeepWarm::Count(0).on() && KeepWarm::Count(1).on() && KeepWarm::Auto.on());
        // At most KEEP_WARM_MAX; auto is within it by its nature.
        let mut routes = Routes::default();
        routes.projects.insert("p".into(), ProjectRouting { keep_warm: Some(KeepWarm::Count(KEEP_WARM_MAX + 1)), ..Default::default() });
        assert!(routes.validate().is_err());
        routes.projects.insert("p".into(), auto);
        routes.validate().unwrap();
    }

    #[test]
    fn skew_protection_settings() {
        let mut routes = Routes::default();
        let mut d = deployment("shop", "1");
        d.created = Some(1_794_800_000);
        routes.deployments.insert("a1".into(), d);
        let skew = SkewProtection {
            enabled: true,
            max_age_s: Some(86_400),
            threshold: Some(SkewThreshold { deployment: "a0".into(), created: 1_794_700_000 }),
            allowed_domains: vec!["app.example.com".into(), "*.example.org".into(), "*".into(), "localhost".into()],
        };
        routes.projects.entry("shop".into()).or_default().skew = Some(skew.clone());
        assert!(routes.validate().is_ok());
        let json = serde_json::to_string(&routes).unwrap();
        assert!(json.contains(r#""created":1794800000"#) && json.contains(r#""skew":{"enabled":true,"max_age_s":86400"#), "{json}");
        assert_eq!(serde_json::from_str::<Routes>(&json).unwrap(), routes);
        assert_eq!(skew.max_age(), 86_400);
        assert_eq!(SkewProtection::default().max_age(), SKEW_DEFAULT_MAX_AGE_S);
        // Off but set is a project's entry; never set leaves the project without one.
        let off = ProjectRouting { skew: Some(SkewProtection::default()), ..Default::default() };
        assert!(!off.is_empty() && off.skew_on().is_none());
        assert!(ProjectRouting::default().is_empty());
        for (bad, error) in [
            (SkewProtection { max_age_s: Some(10), ..skew.clone() }, "maximum age"),
            (SkewProtection { max_age_s: Some(11 * 365 * 86_400), ..skew.clone() }, "maximum age"),
            (SkewProtection { threshold: Some(SkewThreshold { deployment: "A.B".into(), created: 1 }), ..skew.clone() }, "threshold"),
            (SkewProtection { allowed_domains: vec!["https://app.example.com".into()], ..skew.clone() }, "allowed domain"),
            (SkewProtection { allowed_domains: vec!["*.com".into()], ..skew.clone() }, "allowed domain"),
            (SkewProtection { allowed_domains: (0..13).map(|i| format!("a{i}.example.com")).collect(), ..skew.clone() }, "at most 12"),
        ] {
            routes.projects.get_mut("shop").unwrap().skew = Some(bad);
            assert!(routes.validate().unwrap_err().contains(error), "{error}");
        }
        // Tables and entries from before it read as they did.
        let old: Deployment = serde_json::from_str(r#"{"project": "p", "function": "f", "qualifier": "1"}"#).unwrap();
        assert_eq!(old.created, None);
        let old: Routes = serde_json::from_str(r#"{"projects": {"p": {"routing": null}}}"#).unwrap();
        assert!(old.projects["p"].skew.is_none());
    }

    #[test]
    fn static_files_and_egress() {
        let mut d = deployment("web", "3");
        d.static_files = Some("ab".repeat(32));
        d.egress = vec!["analytics".into()];
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains(&format!(r#""static":"{}""#, "ab".repeat(32))) && json.contains(r#""egress":["analytics"]"#), "{json}");
        assert_eq!(serde_json::from_str::<Deployment>(&json).unwrap(), d);
        let mut routes = Routes::default();
        routes.deployments.insert("a".into(), d.clone());
        assert!(routes.validate().is_ok());
        routes.deployments.get_mut("a").unwrap().static_files = Some("../x".into());
        assert!(routes.validate().unwrap_err().contains("SHA-256"));
        routes.deployments.get_mut("a").unwrap().static_files = None;
        routes.deployments.get_mut("a").unwrap().egress = vec!["Analytics".into()];
        assert!(routes.validate().unwrap_err().contains("egress"));
    }

    #[test]
    fn economic_idle() {
        assert_eq!([640, 768, 1769, 256, 128, 64, 10240].map(economic_idle_ms), [24, 20, 8, 60, 120, 120, 5]);
    }

    #[test]
    fn single_mode() {
        let mut d = deployment("harbor", "1");
        assert!(!d.single());
        d.cpu_target_pct = Some(150);
        d.set_single();
        assert!(d.single());
        assert_eq!((d.max_inflight, d.idle_ms, d.headroom_pct, d.cpu_target_pct), (Some(1), Some(24), Some(0), None));
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"idle_ms\":24") && json.contains("\"headroom_pct\":0"), "{json}");
        assert_eq!(serde_json::from_str::<Deployment>(&json).unwrap(), d);
        // Tables written before these fields existed read as before.
        let old = r#"{"project": "p", "function": "f", "qualifier": "1", "max_inflight": 10}"#;
        let d: Deployment = serde_json::from_str(old).unwrap();
        assert_eq!((d.idle_ms, d.headroom_pct), (None, None));
    }

    #[test]
    fn follows_the_projects_max_inflight() {
        let mut d = deployment("harbor", "1");
        assert!(d.follow_max_inflight(Some(3)));
        assert_eq!(d.max_inflight, Some(3));
        assert!(!d.follow_max_inflight(Some(3)), "unchanged");
        assert!(d.follow_max_inflight(None));
        assert_eq!(d.max_inflight, None);
        // One request per worker stays one, and static files alone have no workers.
        d.set_single();
        assert!(!d.follow_max_inflight(Some(5)));
        assert!(d.single());
        let mut files = deployment("harbor", "");
        files.function.clear();
        assert!(!files.follow_max_inflight(Some(5)));
        assert_eq!(files.max_inflight, None);
    }

    #[test]
    fn placement_by_region() {
        let mut d = deployment("atlas", "13");
        // No region recorded: the home region's version, from anywhere.
        assert_eq!(d.placement("us-east-1", "us-east-1", |_| true), ("us-east-1", "13"));
        assert_eq!(d.placement("us-west-1", "us-east-1", |_| true), ("us-east-1", "13"));
        d.region = Some("us-east-1".into());
        d.replicas.insert("us-west-1".into(), "2".into());
        assert_eq!(d.placement("us-east-1", "eu-central-1", |_| true), ("us-east-1", "13"));
        assert_eq!(d.placement("us-west-1", "us-east-1", |_| true), ("us-west-1", "2"));
        assert_eq!(d.placement("eu-central-1", "us-east-1", |_| true), ("us-east-1", "13"));
        assert_eq!(d.versions("x").collect::<Vec<_>>(), [("us-east-1", "13"), ("us-west-1", "2")]);
        // Elsewhere, the nearest region it runs in: Singapore's router takes Mumbai's version,
        // San Francisco's Frankfurt's over Sydney's.
        let mut far = d.clone();
        far.replicas = BTreeMap::from([("ap-south-1".into(), "5".into())]);
        assert_eq!(far.placement("ap-southeast-1", "us-east-1", |_| true), ("ap-south-1", "5"));
        assert_eq!(far.placement("us-west-1", "us-east-1", |_| true), ("us-east-1", "13"));
        far.region = Some("ap-southeast-2".into());
        far.replicas = BTreeMap::from([("eu-central-1".into(), "7".into())]);
        assert_eq!(far.placement("us-west-1", "us-east-1", |_| true), ("eu-central-1", "7"));
        assert_eq!(far.placement("ap-southeast-1", "us-east-1", |_| true), ("ap-southeast-2", "13"));
        // A router whose region isn't known runs its own region's.
        assert_eq!(far.placement("", "us-east-1", |_| true), ("ap-southeast-2", "13"));
        // A region with a router of its own wins over a somewhat nearer one with none: from San
        // Francisco, Virginia's (through its router) over Ohio's (dialed back to from afar).
        let mut ohio = deployment("atlas", "13");
        ohio.region = Some("us-east-1".into());
        ohio.replicas = BTreeMap::from([("us-east-2".into(), "4".into())]);
        let routed = |r: &str| r == "us-east-1" || r == "us-west-1";
        assert_eq!(ohio.placement("us-west-1", "us-east-1", routed), ("us-east-1", "13"));
        assert_eq!(ohio.placement("us-west-1", "us-east-1", |_| true), ("us-east-2", "4"));
        // Its own version where the router is, whatever else.
        assert_eq!(ohio.placement("us-east-2", "us-east-1", routed), ("us-east-2", "4"));
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains(r#""replicas":{"us-west-1":"2"}"#), "{json}");
        assert_eq!(serde_json::from_str::<Deployment>(&json).unwrap(), d);
        // A replica in the deployment's own region, or an empty one, is a mistake.
        let mut routes = Routes::default();
        routes.deployments.insert("a".into(), d.clone());
        assert!(routes.validate().is_ok());
        routes.deployments.get_mut("a").unwrap().replicas.insert("us-east-1".into(), "4".into());
        assert!(routes.validate().unwrap_err().contains("bad replica"));
    }

    #[test]
    fn round_trip() {
        let mut routes = Routes::default();
        routes.deployments.insert("a1b2c3d4".into(), deployment("pyapp", "3"));
        routes.hosts.insert("pyapp.forkrail.test".into(), "a1b2c3d4".into());
        routes.hosts.insert("pyapp-a1b2c3d4.forkrail.test".into(), "a1b2c3d4".into());
        let json = serde_json::to_string_pretty(&routes).unwrap();
        let back: Routes = serde_json::from_str(&json).unwrap();
        assert_eq!(back, routes);
        assert!(back.validate().is_ok());
        let (id, d) = back.lookup("pyapp.forkrail.test").unwrap();
        assert_eq!((id, d.qualifier.as_str()), ("a1b2c3d4", "3"));
        assert_eq!(back.hosts_of("a1b2c3d4").count(), 2);
        // Unset options stay out of the file.
        assert!(!json.contains("max_inflight") && !json.contains("idle_ms") && !json.contains("\"env\""));
        assert!(!json.contains("region") && !json.contains("replicas"));
    }

    #[test]
    fn defaults_fill_in() {
        let json = r#"{"deployments": {"x": {"project": "p", "function": "f", "qualifier": "1"}}, "hosts": {"p.test": "x"}}"#;
        let routes: Routes = serde_json::from_str(json).unwrap();
        let d = &routes.deployments["x"];
        assert_eq!((d.max_duration_s, d.memory_mb, d.max_inflight), (DEFAULT_MAX_DURATION_S, DEFAULT_MEMORY_MB, None));
        assert_eq!(serde_json::from_str::<Routes>("{}").unwrap(), Routes::default());
    }

    #[test]
    fn validate_catches_mistakes() {
        let mut routes = Routes::default();
        routes.deployments.insert("ok".into(), deployment("p", "1"));
        routes.hosts.insert("p.test".into(), "missing".into());
        assert!(routes.validate().unwrap_err().contains("unknown deployment"));

        routes.hosts.clear();
        routes.hosts.insert("P.test:8080".into(), "ok".into());
        assert!(routes.validate().unwrap_err().contains("normalised"));

        routes.hosts.clear();
        routes.deployments.insert("has.dot".into(), deployment("p", "1"));
        assert!(routes.validate().is_err());
    }

    #[test]
    fn host_normalisation() {
        assert_eq!(normalize_host("PyApp.Forkrail.Test"), "pyapp.forkrail.test");
        assert_eq!(normalize_host("pyapp.forkrail.test:8080"), "pyapp.forkrail.test");
        assert_eq!(normalize_host("pyapp.forkrail.test."), "pyapp.forkrail.test");
        assert_eq!(normalize_host(" 127.0.0.1:18080 "), "127.0.0.1");
        assert_eq!(normalize_host("[::1]:8080"), "[::1]");
        assert_eq!(normalize_host("[::1]"), "[::1]");
        assert_eq!(normalize_host("localhost"), "localhost");
    }

    #[test]
    fn labels_under_the_base() {
        assert!(one_label_under("pyapp.forkrail.com", "forkrail.com"));
        assert!(one_label_under("pyapp-a1b2c3d4.forkrail.com", "forkrail.com"));
        assert!(!one_label_under("forkrail.com", "forkrail.com"));
        assert!(!one_label_under(".forkrail.com", "forkrail.com"));
        assert!(!one_label_under("a.b.forkrail.com", "forkrail.com"));
        assert!(!one_label_under("evilforkrail.com", "forkrail.com"));
        assert!(!one_label_under("pyapp.forkrail.test", "forkrail.com"));
    }

    #[test]
    fn custom_domains() {
        assert!(valid_domain("atlas.example.net"));
        assert!(valid_domain("example.net"));
        assert!(valid_domain("a-1.b.example.co.uk"));
        for bad in ["localhost", "Atlas.example.net", "*.example.net", "a..b", "-a.b", "a.b-", "1.2.3.4", "a b.c", ""] {
            assert!(!valid_domain(bad), "{bad}");
        }
        let mut routes = Routes::default();
        routes.deployments.insert("a1b2c3d4".into(), deployment("atlas", "6"));
        routes.hosts.insert("atlas.example.net".into(), "a1b2c3d4".into());
        routes.domains.insert("atlas.example.net".into(), CustomDomain::new("atlas"));
        assert!(routes.validate().is_ok());
        let json = serde_json::to_string(&routes).unwrap();
        assert_eq!(serde_json::from_str::<Routes>(&json).unwrap(), routes);
        // Entries from before C2 read as they were, and new ones write only what they have.
        assert!(json.contains(r#""domains":{"atlas.example.net":{"project":"atlas"}}"#), "{json}");
        routes.domains.insert("atlas.example.net".into(), CustomDomain::new("orbit"));
        assert!(routes.validate().unwrap_err().contains("belongs to orbit"));
        // Tables without domains stay as they were.
        assert!(!serde_json::to_string(&Routes::default()).unwrap().contains("domains"));
    }

    #[test]
    fn wildcard_domains() {
        assert!(valid_domain_entry("*.example.com") && valid_domain_entry("example.com"));
        for bad in ["*.com", "*", "**.example.com", "a.*.example.com", "*example.com", "*.Example.com"] {
            assert!(!valid_domain_entry(bad), "{bad}");
        }
        assert_eq!(wildcard_above("a.example.com").as_deref(), Some("*.example.com"));
        assert_eq!(wildcard_above("example.com"), None);
        let mut routes = Routes::default();
        routes.deployments.insert("a1".into(), deployment("shop", "1"));
        routes.deployments.insert("b2".into(), deployment("shop", "2"));
        routes.hosts.insert("*.example.com".into(), "a1".into());
        routes.hosts.insert("www.example.com".into(), "b2".into());
        routes.domains.insert("*.example.com".into(), CustomDomain::new("shop"));
        routes.domains.insert("www.example.com".into(), CustomDomain::new("shop"));
        assert!(routes.validate().is_ok());
        // A name of its own wins; the wildcard covers one label only.
        assert_eq!(routes.lookup("www.example.com").map(|(id, _)| id), Some("b2"));
        assert_eq!(routes.lookup("eu.example.com").map(|(id, _)| id), Some("a1"));
        assert!(routes.lookup("a.eu.example.com").is_none() && routes.lookup("example.com").is_none());
        assert_eq!(routes.domain("eu.example.com").map(|(name, _)| name), Some("*.example.com"));
        assert_eq!(routes.domain("www.example.com").map(|(name, _)| name), Some("www.example.com"));
        assert!(routes.domain("example.com").is_none());
    }

    #[test]
    fn domain_redirects_and_branches() {
        let mut routes = Routes::default();
        routes.deployments.insert("a1".into(), deployment("shop", "1"));
        routes.hosts.insert("example.com".into(), "a1".into());
        routes.domains.insert("example.com".into(), CustomDomain::new("shop"));
        let www = CustomDomain { redirect: Some(DomainRedirect { to: "example.com".into(), status: 308 }), ..CustomDomain::new("shop") };
        routes.domains.insert("www.example.com".into(), www.clone());
        assert!(routes.validate().is_ok());
        assert!(!www.production());
        let json = serde_json::to_string(&www).unwrap();
        assert_eq!(json, r#"{"project":"shop","redirect":{"to":"example.com","status":308}}"#);
        // The status defaults to 308.
        let read: CustomDomain = serde_json::from_str(r#"{"project":"shop","redirect":{"to":"example.com"}}"#).unwrap();
        assert_eq!(read, www);
        for (redirect, error) in [
            (DomainRedirect { to: "example.com".into(), status: 303 }, "status"),
            (DomainRedirect { to: "www.example.com".into(), status: 301 }, "another domain"),
            (DomainRedirect { to: "*.example.com".into(), status: 301 }, "another domain"),
        ] {
            routes.domains.get_mut("www.example.com").unwrap().redirect = Some(redirect);
            assert!(routes.validate().unwrap_err().contains(error), "{error}");
        }
        routes.domains.insert("www.example.com".into(), www);
        routes.domains.get_mut("www.example.com").unwrap().branch = Some("main".into());
        assert!(routes.validate().unwrap_err().contains("no branch"));
        let staging = CustomDomain { branch: Some("staging".into()), ..CustomDomain::new("shop") };
        assert!(!staging.production() && CustomDomain::new("shop").production());
        routes.domains.insert("www.example.com".into(), staging);
        assert!(routes.validate().is_ok());
        routes.domains.get_mut("www.example.com").unwrap().branch = Some(String::new());
        assert!(routes.validate().unwrap_err().contains("branch"));
    }

    #[cfg(feature = "domains")]
    #[test]
    fn acme_delegations() {
        let name = acme_delegation("example.com", "forkrail.com");
        assert!(name.ends_with("._acme.forkrail.com") && name.len() == 16 + "._acme.forkrail.com".len(), "{name}");
        assert_eq!(acme_delegation("*.example.com", "forkrail.com"), name);
        assert_ne!(acme_delegation("www.example.com", "forkrail.com"), name);
        // Fixed: the dashboard computes the same (web/src/server/state.ts).
        assert_eq!(name, "c7b767492adfc8be._acme.forkrail.com");
    }

    #[test]
    fn ids() {
        assert!(valid_id("a1b2c3d4"));
        assert!(valid_id("my-app-1"));
        assert!(!valid_id(""));
        assert!(!valid_id("A1"));
        assert!(!valid_id("a.b"));
        assert!(!valid_id("a b"));
        assert!(!valid_id(&"a".repeat(33)));
    }

    #[test]
    fn shield_limits_are_the_installs_and_in_range() {
        let mut routes = Routes::default();
        routes.mitigations.limits = Some(ShieldLimits { connections: Some(1_024), requests: Some(20_000), block_s: None });
        assert!(routes.validate().is_ok());
        let back: Routes = serde_json::from_value(serde_json::to_value(&routes).unwrap()).unwrap();
        assert_eq!(back.mitigations.limits, routes.mitigations.limits);
        routes.mitigations.limits = Some(ShieldLimits { connections: Some(8), ..ShieldLimits::default() });
        assert!(routes.validate().unwrap_err().contains("connections per address"));
        routes.mitigations.limits = Some(ShieldLimits { requests: Some(2_000_000), ..ShieldLimits::default() });
        assert!(routes.validate().unwrap_err().contains("requests per address"));
        routes.mitigations.limits = None;
        routes.projects.entry("shop".into()).or_default().mitigations.limits = Some(ShieldLimits { connections: Some(512), ..ShieldLimits::default() });
        assert!(routes.validate().unwrap_err().contains("install's"));
        // A table from before limits reads as it did.
        let old: Routes = serde_json::from_str(r#"{"mitigations": {"paused_until": 5}}"#).unwrap();
        assert!(old.mitigations.limits.is_none());
    }
}
