//! A project's firewall, and the install's: changed from the dashboard, the CLI or the API (all
//! through here), as Vercel's firewall configuration is. The configuration's shape and how the
//! routers judge requests by it are in `forkrail_common::firewall`.
//!
//! Every change makes a new version, staged as a draft: it does nothing until it's published.
//! `diff` shows what publishing would change; publishing (or restoring an older version) copies the
//! configuration to every region's bucket, then names it in `state/routes.json` (the install's as
//! `firewall`, a project's as `projects.<name>.firewall`), and every router has it in force within
//! about a second.
//!
//! In the home bucket:
//! - `state/firewall/<project>.json` (`_install.json` for the install's): the versions, newest
//!   first, and which is live and which staged;
//! - `static/firewall/<sha>.json`: a version's configuration, as the routers read it.
//!
//! Changes take Vercel's PATCH actions (`rules.insert`, `ip.remove`, `firewallEnabled`...), several
//! at once, or a whole configuration (Vercel's PUT).

use std::collections::BTreeMap;
use std::net::IpAddr;

use anyhow::{Context, Result, bail};
use forkrail_common::firewall::{Config, Decision, Firewall, Hit, IpRule, Limiter, RateSpec, Request, Rule, Verdict};
use forkrail_common::install::project_key;
use forkrail_common::firewall::Cidr;
use forkrail_common::routes::{MAX_SYSTEM_BYPASS, Routes, SHIELD_DEFAULTS, ShieldLimits, SystemBypass, normalize_host};
use forkrail_common::statics::firewall_key;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::deploy::Ctx;
use crate::project_routing::{KEEP_VERSIONS, Version, Versions, split_url};
use crate::state::{Project, ROUTES_KEY, now_rfc3339};
use crate::statics;

/// The install's versions are kept under this name, which no project can have.
const INSTALL: &str = "_install";

fn versions_key(scope: Option<&str>) -> String {
    format!("state/firewall/{}.json", scope.unwrap_or(INSTALL))
}

fn describe_scope(scope: Option<&str>) -> String {
    match scope {
        Some(p) => format!("project {p}"),
        None => "the install".to_owned(),
    }
}

/// What the dashboard, the CLI and the API ask of a firewall.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FirewallOp {
    /// A version's configuration (`version`: live, staging, or an id; default the staged one,
    /// else the live one), with the draft's changes against what's live.
    Get {
        #[serde(default)]
        version: Option<String>,
    },
    Versions,
    /// Changes to the working configuration (the draft, else what's live), staged as one version.
    Change { changes: Vec<Change> },
    /// A whole configuration, staged in place of the working one.
    Replace { config: Value },
    /// What publishing version `to` (default the draft) would change from `from` (default live).
    Diff {
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
    },
    Publish {
        #[serde(default)]
        version: Option<String>,
    },
    /// Make an older version live again.
    Restore { version: String },
    /// Drop the draft.
    Discard,
    /// What a request would get from a version (default the draft, else live).
    Test {
        url: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// The client's address; default 192.0.2.1 (a documentation address).
        #[serde(default)]
        ip: Option<String>,
        #[serde(default)]
        country: Option<String>,
        #[serde(default)]
        asn: Option<u32>,
        #[serde(default)]
        environment: Option<String>,
        #[serde(default)]
        version: Option<String>,
    },
    /// What applies at once, without publishing: the install's pause and system bypasses, or a
    /// project's attack mode and system bypasses.
    Mitigations,
    /// A project's attack mode: on for `duration` (`1h`, `6h` or `24h`; default 1h), or `off`.
    AttackMode {
        #[serde(default)]
        duration: Option<String>,
        #[serde(default)]
        off: bool,
    },
    /// An address or range the system mitigations leave alone, on `domain` (`*` for every host).
    SystemBypassAdd {
        source_ip: String,
        #[serde(default)]
        domain: Option<String>,
        #[serde(default)]
        note: String,
    },
    /// Remove a system bypass, by its id or address.
    SystemBypassRemove { id: String },
    /// Turn the install's system mitigations off for 24 hours (owners).
    Pause,
    Resume,
    /// The install's limits on each address (owners): the fields given replace those set, after
    /// `reset` puts every one back to the routers' own.
    Limits {
        #[serde(default)]
        connections: Option<u32>,
        #[serde(default)]
        requests: Option<u32>,
        #[serde(default)]
        block_s: Option<u32>,
        #[serde(default)]
        reset: bool,
    },
}

impl FirewallOp {
    /// Its name, for logs.
    pub fn name(&self) -> &'static str {
        match self {
            FirewallOp::Get { .. } => "get",
            FirewallOp::Versions => "versions",
            FirewallOp::Change { .. } => "change",
            FirewallOp::Replace { .. } => "replace",
            FirewallOp::Diff { .. } => "diff",
            FirewallOp::Publish { .. } => "publish",
            FirewallOp::Restore { .. } => "restore",
            FirewallOp::Discard => "discard",
            FirewallOp::Test { .. } => "test",
            FirewallOp::Mitigations => "mitigations",
            FirewallOp::AttackMode { .. } => "attack_mode",
            FirewallOp::SystemBypassAdd { .. } => "system_bypass_add",
            FirewallOp::SystemBypassRemove { .. } => "system_bypass_remove",
            FirewallOp::Pause => "pause",
            FirewallOp::Resume => "resume",
            FirewallOp::Limits { .. } => "limits",
        }
    }

    /// Whether it changes anything (and so is for owners and members; the install's, for owners).
    pub fn changes(&self) -> bool {
        !matches!(self, FirewallOp::Get { .. } | FirewallOp::Versions | FirewallOp::Diff { .. } | FirewallOp::Test { .. } | FirewallOp::Mitigations)
    }
}

/// One change, as Vercel's PATCH takes it: `action`, the entry's `id` where it names one, and
/// `value`.
#[derive(Debug, Clone, Deserialize)]
pub struct Change {
    pub action: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub value: Value,
}

/// The versions of a firewall, with a check that the project exists.
pub async fn versions(ctx: &Ctx, scope: Option<&str>) -> Result<Versions> {
    if let Some(project) = scope
        && ctx.store.get_value::<Project>(&project_key(project)).await?.is_none()
    {
        bail!("there's no project {project}");
    }
    Ok(ctx.store.get_value(&versions_key(scope)).await?.unwrap_or_default())
}

async fn read_config(ctx: &Ctx, version: Option<&Version>) -> Result<Config> {
    let Some(version) = version else { return Ok(Config::default()) };
    let bytes = ctx.store.get_bytes(&firewall_key(&version.object)).await?.with_context(|| format!("version {}'s configuration is gone", version.id))?;
    serde_json::from_slice(&bytes).with_context(|| format!("version {}'s configuration", version.id))
}

/// Store `config` as a new staged version; `base` is the version the change started from.
async fn stage(ctx: &Ctx, scope: Option<&str>, mut config: Config, base: Option<String>, actor: Option<&str>, note: String) -> Result<Value> {
    config.assign_ids();
    config.validate().map_err(anyhow::Error::msg)?;
    let bytes = serde_json::to_vec(&config)?;
    let sha = forkrail_common::hex(&Sha256::digest(&bytes));
    ctx.store.put_bytes(&firewall_key(&sha), bytes, "application/json").await?;
    let count = config.rules.len() + config.ips.len();
    let version = Version {
        id: format!("v_{}", forkrail_common::random_hex(6)),
        object: sha,
        program: None,
        count,
        created: now_rfc3339(),
        created_by: actor.map(str::to_owned),
        note: Some(note),
        published: None,
    };
    let saved = version.clone();
    let what = describe_scope(scope);
    ctx.store
        .update::<Versions, _>(&versions_key(scope), move |current| {
            let mut v = current.unwrap_or_default();
            // Two people changing it at once: the second starts again from the first's.
            if v.working().map(|w| w.id.clone()) != base {
                bail!("the firewall of {what} changed while this change was made; try again");
            }
            v.versions.insert(0, version.clone());
            v.staging = Some(version.id.clone());
            let keep: Vec<String> = [v.live.clone(), v.staging.clone()].into_iter().flatten().collect();
            let mut kept = 0;
            v.versions.retain(|x| {
                kept += 1;
                kept <= KEEP_VERSIONS || keep.contains(&x.id)
            });
            Ok(v)
        })
        .await?;
    Ok(json!({"staging": saved.id, "config": config}))
}

/// Make version `which` (default the draft) live: its configuration in every region, then named in
/// the routing table.
async fn publish(ctx: &Ctx, scope: Option<&str>, which: Option<&str>) -> Result<Value> {
    let v = versions(ctx, scope).await?;
    let version = match which {
        None => v.staging.as_deref().and_then(|id| v.get(id)).context("there's no draft: nothing to publish")?,
        Some(id) => v.pick(Some(id))?.context("no such version")?,
    }
    .clone();
    let config = read_config(ctx, Some(&version)).await?;
    // A configuration that does nothing isn't named at all, so routers skip the project.
    let live_id = (!config.is_empty() || !config.firewall_enabled).then(|| version.object.clone());
    let settings = ctx.store.settings().await?;
    let home = ctx.install.home_region.clone();
    let regions: Vec<String> = std::iter::once(home.clone()).chain(settings.regions.iter().filter(|r| **r != home).cloned()).collect();
    if let Some(id) = &live_id {
        for region in &regions {
            statics::upload_object(ctx, region, None, &firewall_key(id), "application/json").await.with_context(|| format!("copying to {region}"))?;
        }
    }
    let name = scope.map(str::to_owned);
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            match &name {
                None => routes.firewall = live_id.clone(),
                Some(project) => {
                    let entry = routes.projects.entry(project.clone()).or_default();
                    entry.firewall = live_id.clone();
                    if entry.is_empty() {
                        routes.projects.remove(project);
                    }
                }
            }
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    let id = version.id.clone();
    let when = now_rfc3339();
    ctx.store
        .update::<Versions, _>(&versions_key(scope), move |current| {
            let mut v = current.unwrap_or_default();
            v.live = Some(id.clone());
            if v.staging.as_deref() == Some(id.as_str()) {
                v.staging = None;
            }
            if let Some(x) = v.versions.iter_mut().find(|x| x.id == id) {
                x.published = Some(when.clone());
            }
            Ok(v)
        })
        .await?;
    Ok(json!({"project": scope, "live": version.id, "rules": config.rules.len(), "ips": config.ips.len(), "regions": regions}))
}

/// Versions, newest first, for lists.
fn describe(v: &Versions) -> Value {
    let list: Vec<Value> = v
        .versions
        .iter()
        .map(|x| {
            json!({
                "id": x.id, "count": x.count, "created": x.created, "created_by": x.created_by, "note": x.note,
                "published": x.published, "live": v.live.as_deref() == Some(x.id.as_str()), "staging": v.staging.as_deref() == Some(x.id.as_str()),
            })
        })
        .collect();
    json!({"live": v.live, "staging": v.staging, "versions": list})
}

pub async fn run(ctx: &Ctx, scope: Option<&str>, op: FirewallOp, actor: Option<&str>) -> Result<Value> {
    if scope == Some(INSTALL) {
        bail!("there's no project {INSTALL}");
    }
    let v = versions(ctx, scope).await?;
    let base = v.working().map(|w| w.id.clone());
    match op {
        op @ (FirewallOp::Mitigations
        | FirewallOp::AttackMode { .. }
        | FirewallOp::SystemBypassAdd { .. }
        | FirewallOp::SystemBypassRemove { .. }
        | FirewallOp::Pause
        | FirewallOp::Resume
        | FirewallOp::Limits { .. }) => mitigations(ctx, scope, op, actor).await,
        FirewallOp::Versions => Ok(describe(&v)),
        FirewallOp::Get { version } => {
            let picked = v.pick(version.as_deref())?;
            let config = read_config(ctx, picked).await?;
            let live = read_config(ctx, v.live.as_deref().and_then(|id| v.get(id))).await?;
            let draft = match v.staging.as_deref().and_then(|id| v.get(id)) {
                Some(s) => Some(read_config(ctx, Some(s)).await?),
                None => None,
            };
            let pending = draft.as_ref().map(|d| diff(&live, d)).unwrap_or_else(|| json!([]));
            Ok(json!({"project": scope, "version": picked.map(|p| &p.id), "live": v.live, "staging": v.staging, "config": config, "pending": pending}))
        }
        FirewallOp::Change { changes } => {
            if changes.is_empty() {
                bail!("no changes given");
            }
            let mut config = read_config(ctx, v.working()).await?;
            let mut notes = Vec::new();
            for change in changes {
                notes.push(apply(&mut config, change)?);
            }
            stage(ctx, scope, config, base, actor, notes.join("; ")).await
        }
        FirewallOp::Replace { config } => {
            let config: Config = serde_json::from_value(config).context("not a firewall configuration")?;
            let note = format!("set {} rules and {} IP blocks", config.rules.len(), config.ips.len());
            stage(ctx, scope, config, base, actor, note).await
        }
        FirewallOp::Diff { from, to } => {
            let from_v = match from.as_deref() {
                None => v.live.as_deref().and_then(|id| v.get(id)),
                Some(x) => v.pick(Some(x))?,
            };
            let to_v = match to.as_deref() {
                None => v.staging.as_deref().and_then(|id| v.get(id)).or(from_v),
                Some(x) => v.pick(Some(x))?,
            };
            let (a, b) = (read_config(ctx, from_v).await?, read_config(ctx, to_v).await?);
            Ok(json!({"from": from_v.map(|x| &x.id), "to": to_v.map(|x| &x.id), "changes": diff(&a, &b)}))
        }
        FirewallOp::Publish { version } => publish(ctx, scope, version.as_deref()).await,
        FirewallOp::Restore { version } => publish(ctx, scope, Some(&version)).await,
        FirewallOp::Discard => {
            let v = ctx
                .store
                .update::<Versions, _>(&versions_key(scope), |current| {
                    let mut v = current.unwrap_or_default();
                    v.staging = None;
                    Ok(v)
                })
                .await?;
            Ok(json!({"project": scope, "live": v.live}))
        }
        FirewallOp::Test { url, method, headers, ip, country, asn, environment, version } => {
            let config = read_config(ctx, v.pick(version.as_deref())?).await?;
            test(&config, &url, method.as_deref().unwrap_or("GET"), &headers, ip.as_deref(), country.as_deref(), asn, environment.as_deref())
        }
    }
}

/// Apply one change; a few words saying what it did.
pub fn apply(config: &mut Config, change: Change) -> Result<String> {
    let id = || change.id.clone().filter(|i| !i.is_empty()).with_context(|| format!("{} needs the id of what it changes", change.action));
    fn find<T>(list: &[T], id: &str, key: fn(&T) -> (&str, &str), what: &str) -> Result<usize> {
        list.iter().position(|x| key(x).0 == id).or_else(|| list.iter().position(|x| key(x).1 == id)).with_context(|| format!("there's no {what} {id}"))
    }
    fn rule_key(r: &Rule) -> (&str, &str) {
        (r.id.as_str(), r.name.as_str())
    }
    fn ip_key(r: &IpRule) -> (&str, &str) {
        (r.id.as_str(), r.ip.as_str())
    }
    Ok(match change.action.as_str() {
        "firewallEnabled" => {
            let on = change.value.as_bool().context("firewallEnabled takes true or false")?;
            config.firewall_enabled = on;
            format!("custom rules {}", if on { "on" } else { "off" })
        }
        "rules.insert" => {
            let mut rule: Rule = serde_json::from_value(change.value).context("not a rule")?;
            rule.id = String::new();
            let note = format!("added rule {:?}", rule.name);
            config.rules.push(rule);
            note
        }
        "rules.update" => {
            let at = find(&config.rules, &id()?, rule_key, "rule")?;
            let mut rule: Rule = serde_json::from_value(change.value).context("not a rule")?;
            rule.id = config.rules[at].id.clone();
            let note = format!("changed rule {:?}", rule.name);
            config.rules[at] = rule;
            note
        }
        "rules.remove" => {
            let at = find(&config.rules, &id()?, rule_key, "rule")?;
            format!("removed rule {:?}", config.rules.remove(at).name)
        }
        "rules.priority" => {
            let at = find(&config.rules, &id()?, rule_key, "rule")?;
            let to = change.value.as_u64().context("rules.priority takes the rule's new position, from 0")? as usize;
            let rule = config.rules.remove(at);
            let to = to.min(config.rules.len());
            let note = format!("moved rule {:?} to {}", rule.name, to + 1);
            config.rules.insert(to, rule);
            note
        }
        "rules.active" => {
            let at = find(&config.rules, &id()?, rule_key, "rule")?;
            let on = change.value.as_bool().context("rules.active takes true or false")?;
            config.rules[at].active = on;
            format!("{} rule {:?}", if on { "turned on" } else { "turned off" }, config.rules[at].name)
        }
        "ip.insert" => {
            let mut entry: IpRule = serde_json::from_value(change.value).context("not an IP block")?;
            entry.id = String::new();
            let note = format!("{} {} on {}", entry.action.name(), entry.ip, entry.hostname);
            config.ips.push(entry);
            note
        }
        "ip.update" => {
            let at = find(&config.ips, &id()?, ip_key, "IP block")?;
            let mut entry: IpRule = serde_json::from_value(change.value).context("not an IP block")?;
            entry.id = config.ips[at].id.clone();
            let note = format!("changed IP block {}", entry.ip);
            config.ips[at] = entry;
            note
        }
        "ip.remove" => {
            let at = find(&config.ips, &id()?, ip_key, "IP block")?;
            format!("removed IP block {}", config.ips.remove(at).ip)
        }
        "bypass.insert" | "bypass.remove" => bail!("system bypasses apply at once, without publishing: use the system_bypass op"),
        "lists.set" => {
            let name = id()?;
            let entries: Vec<String> = serde_json::from_value(change.value).context("lists.set takes a list of addresses and ranges")?;
            let note = format!("list {name}: {} addresses", entries.len());
            config.lists.insert(name, entries);
            note
        }
        "lists.remove" => {
            let name = id()?;
            config.lists.remove(&name).with_context(|| format!("there's no list {name}"))?;
            format!("removed list {name}")
        }
        "crs.update" | "crs.disable" | "managedRules.update" => bail!("managed rulesets come with the hosted plan; this install has none"),
        other => bail!("unknown change {other:?}"),
    })
}

/// What changes from `a` to `b`: entries added, removed and changed, by id, and the switch.
pub fn diff(a: &Config, b: &Config) -> Value {
    let mut out = Vec::new();
    if a.firewall_enabled != b.firewall_enabled {
        out.push(json!({"kind": "firewall", "change": if b.firewall_enabled { "on" } else { "off" }}));
    }
    fn entries<T: serde::Serialize>(kind: &str, a: &[T], b: &[T], id: impl Fn(&T) -> &str, label: impl Fn(&T) -> String, out: &mut Vec<Value>) {
        for (i, x) in b.iter().enumerate() {
            match a.iter().position(|y| id(y) == id(x)) {
                None => out.push(json!({"kind": kind, "change": "added", "id": id(x), "label": label(x), "after": x})),
                Some(j) => {
                    let (before, after) = (serde_json::to_value(&a[j]).ok(), serde_json::to_value(x).ok());
                    if before != after {
                        out.push(json!({"kind": kind, "change": "changed", "id": id(x), "label": label(x), "before": &a[j], "after": x}));
                    } else if i != j && kind == "rule" {
                        out.push(json!({"kind": kind, "change": "moved", "id": id(x), "label": label(x), "from": j + 1, "to": i + 1}));
                    }
                }
            }
        }
        for y in a {
            if !b.iter().any(|x| id(x) == id(y)) {
                out.push(json!({"kind": kind, "change": "removed", "id": id(y), "label": label(y), "before": y}));
            }
        }
    }
    entries("rule", &a.rules, &b.rules, |r| r.id.as_str(), |r| r.name.clone(), &mut out);
    entries("ip", &a.ips, &b.ips, |r| r.id.as_str(), |r| format!("{} on {}", r.ip, r.hostname), &mut out);
    for (name, list) in &b.lists {
        match a.lists.get(name) {
            None => out.push(json!({"kind": "list", "change": "added", "id": name, "label": format!("{name} ({} addresses)", list.len())})),
            Some(old) if old != list => out.push(json!({"kind": "list", "change": "changed", "id": name, "label": format!("{name} ({} → {} addresses)", old.len(), list.len())})),
            _ => {}
        }
    }
    for name in a.lists.keys().filter(|n| !b.lists.contains_key(*n)) {
        out.push(json!({"kind": "list", "change": "removed", "id": name, "label": name}));
    }
    Value::Array(out)
}

/// Attack mode's lengths, as Vercel offers them.
const ATTACK_MODE_LENGTHS: [(&str, u64); 3] = [("1h", 3600), ("6h", 6 * 3600), ("24h", 24 * 3600)];
/// How long a pause of the system mitigations lasts.
const PAUSE_S: u64 = 24 * 3600;

/// The ops that change the routing table at once: attack mode, system bypasses, the pause.
async fn mitigations(ctx: &Ctx, scope: Option<&str>, op: FirewallOp, actor: Option<&str>) -> Result<Value> {
    let now = crate::state::now_ms() / 1000;
    let show = |routes: &Routes| -> Value {
        let m = match scope {
            Some(p) => routes.projects.get(p).map(|r| r.mitigations.clone()).unwrap_or_default(),
            None => routes.mitigations.clone(),
        };
        json!({
            "project": scope,
            "attack_mode": m.attack_mode(now),
            "attack_mode_until": m.attack_mode_until.filter(|u| *u > now),
            "paused": m.paused(now),
            "paused_until": m.paused_until.filter(|u| *u > now),
            "changed_by": m.changed_by,
            "changed_at": m.changed_at,
            "system_bypass": m.system_bypass,
            "limits": limits_view(routes.mitigations.limits),
        })
    };
    if let FirewallOp::Mitigations = op {
        let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
        return Ok(show(&routes));
    }
    let routes = ctx
        .store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            change_mitigations(&mut routes, scope, &op, actor, now)?;
            Ok(routes)
        })
        .await?;
    Ok(show(&routes))
}

/// The system mitigations' limits in force: each one the install sets, else the routers' own.
fn limits_view(set: Option<ShieldLimits>) -> Value {
    let set = set.unwrap_or_default();
    json!({
        "connections": set.connections.or(SHIELD_DEFAULTS.connections),
        "requests": set.requests.or(SHIELD_DEFAULTS.requests),
        "window_s": 10,
        "block_s": set.block_s.or(SHIELD_DEFAULTS.block_s),
        "defaults": {"connections": SHIELD_DEFAULTS.connections, "requests": SHIELD_DEFAULTS.requests, "block_s": SHIELD_DEFAULTS.block_s},
        "custom": !set.is_empty(),
    })
}

/// Change the routing table's attack mode, system bypasses, pause or limits, as `op` says.
fn change_mitigations(routes: &mut Routes, scope: Option<&str>, op: &FirewallOp, actor: Option<&str>, now: u64) -> Result<()> {
    match (op, scope) {
        (FirewallOp::AttackMode { .. }, None) => bail!("attack mode is a project's: name one"),
        (FirewallOp::Pause | FirewallOp::Resume, Some(_)) => bail!("the system mitigations are paused for the whole install: use --install"),
        (FirewallOp::Limits { .. }, Some(_)) => bail!("the system mitigations' limits are the whole install's: use --install"),
        _ => {}
    }
    let actor = actor.map(str::to_owned);
    let hosts_ok = |routes: &Routes, domain: &str| -> bool {
        let Some(project) = scope else { return true };
        if domain == "*" {
            return true;
        }
        let of = |h: &str| routes.lookup(h).is_some_and(|(_, d)| d.project == project) || routes.domain(h).is_some_and(|(_, d)| d.project == project);
        match domain.strip_prefix("*.") {
            Some(base) => routes.hosts.keys().chain(routes.domains.keys()).any(|h| h.ends_with(&format!(".{base}")) && of(h)),
            None => of(domain),
        }
    };
    let domain_checked = match op {
        FirewallOp::SystemBypassAdd { domain, .. } => {
            let d = domain.as_deref().map(normalize_bypass_domain).unwrap_or_else(|| "*".to_owned());
            if !hosts_ok(routes, &d) {
                bail!("{d} isn't one of {}'s hosts", scope.unwrap_or_default());
            }
            Some(d)
        }
        _ => None,
    };
    let m = match scope {
        Some(p) => &mut routes.projects.entry(p.to_owned()).or_default().mitigations,
        None => &mut routes.mitigations,
    };
    match op {
        FirewallOp::AttackMode { duration, off } => {
            if *off {
                m.attack_mode_until = None;
            } else {
                let d = duration.as_deref().unwrap_or("1h");
                let secs = ATTACK_MODE_LENGTHS.iter().find(|(n, _)| *n == d).map(|(_, s)| *s).with_context(|| format!("attack mode lasts 1h, 6h or 24h, not {d:?}"))?;
                m.attack_mode_until = Some(now + secs);
            }
            m.changed_by = actor.clone();
            m.changed_at = Some(now_rfc3339());
        }
        FirewallOp::Pause => {
            m.paused_until = Some(now + PAUSE_S);
            m.changed_by = actor.clone();
            m.changed_at = Some(now_rfc3339());
        }
        FirewallOp::Resume => {
            m.paused_until = None;
            m.changed_by = actor.clone();
            m.changed_at = Some(now_rfc3339());
        }
        FirewallOp::SystemBypassAdd { source_ip, note, .. } => {
            let cidr = Cidr::parse(source_ip).map_err(anyhow::Error::msg)?;
            let source = if cidr.len == if cidr.addr.is_ipv4() { 32 } else { 128 } { cidr.addr.to_string() } else { format!("{}/{}", cidr.addr, cidr.len) };
            let domain = domain_checked.clone().unwrap_or_else(|| "*".to_owned());
            if m.system_bypass.iter().any(|b| b.source_ip == source && b.domain == domain) {
                bail!("{source} is already left alone on {domain}");
            }
            if m.system_bypass.len() >= MAX_SYSTEM_BYPASS {
                bail!("{} has {MAX_SYSTEM_BYPASS} system bypasses already: remove one first", describe_scope(scope));
            }
            m.system_bypass.push(SystemBypass {
                id: format!("sb_{}", forkrail_common::random_hex(6)),
                source_ip: source,
                domain,
                note: note.clone(),
                added_by: actor.clone(),
                added_at: Some(now_rfc3339()),
            });
        }
        FirewallOp::Limits { connections, requests, block_s, reset } => {
            let mut limits = if *reset { ShieldLimits::default() } else { m.limits.unwrap_or_default() };
            // A value equal to the routers' own isn't kept: it follows them if they change.
            let own = |v: Option<u32>, default: Option<u32>| v.filter(|n| Some(*n) != default);
            if connections.is_some() {
                limits.connections = own(*connections, SHIELD_DEFAULTS.connections);
            }
            if requests.is_some() {
                limits.requests = own(*requests, SHIELD_DEFAULTS.requests);
            }
            if block_s.is_some() {
                limits.block_s = own(*block_s, SHIELD_DEFAULTS.block_s);
            }
            limits.validate().map_err(anyhow::Error::msg)?;
            m.limits = (!limits.is_empty()).then_some(limits);
            m.changed_by = actor.clone();
            m.changed_at = Some(now_rfc3339());
        }
        FirewallOp::SystemBypassRemove { id } => {
            let before = m.system_bypass.len();
            let as_ip = Cidr::parse(id).ok().map(|c| if c.len == if c.addr.is_ipv4() { 32 } else { 128 } { c.addr.to_string() } else { format!("{}/{}", c.addr, c.len) });
            m.system_bypass.retain(|b| b.id != *id && Some(&b.source_ip) != as_ip.as_ref());
            if m.system_bypass.len() == before {
                bail!("there's no system bypass {id}");
            }
        }
        _ => unreachable!("only immediate ops get here"),
    }
    if let Some(p) = scope
        && routes.projects.get(p).is_some_and(|r| r.is_empty())
    {
        routes.projects.remove(p);
    }
    routes.validate().map_err(anyhow::Error::msg)?;
    Ok(())
}

fn normalize_bypass_domain(domain: &str) -> String {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    match d.strip_prefix("*.") {
        Some(rest) => format!("*.{}", normalize_host(rest)),
        None if d == "*" => d,
        None => normalize_host(&d),
    }
}

/// What a request gets from `config` alone (not the install's, nor persistent blocks).
#[allow(clippy::too_many_arguments)]
pub fn test(config: &Config, url: &str, method: &str, headers: &BTreeMap<String, String>, ip: Option<&str>, country: Option<&str>, asn: Option<u32>, environment: Option<&str>) -> Result<Value> {
    let firewall = Firewall::new(config).map_err(anyhow::Error::msg)?;
    let (host, path, query) = split_url(url)?;
    let mut map = hyper::HeaderMap::new();
    for (k, v) in headers {
        let name = hyper::header::HeaderName::from_bytes(k.as_bytes()).with_context(|| format!("header name {k:?}"))?;
        map.append(name, hyper::header::HeaderValue::from_str(v).with_context(|| format!("header {k}'s value"))?);
    }
    let ip: IpAddr = ip.unwrap_or("192.0.2.1").parse().context("ip isn't an IP address")?;
    let counted = Counted::default();
    let Verdict { decision, rule } = firewall.check(&Request {
        method,
        host: &host,
        raw_path: &path,
        target_path: None,
        query: query.as_deref(),
        headers: &map,
        ip,
        protocol: "HTTP/2.0",
        scheme: "https",
        environment: environment.unwrap_or("production"),
        region: "",
        continent: None,
        country,
        country_region: None,
        city: None,
        asn,
        ja4: None,
        rate_limit_api: None,
        challenge_passed: false,
    }, &counted);
    let name = config.rules.iter().find(|r| Some(&r.id) == rule.as_ref()).map(|r| r.name.clone()).or_else(|| config.ips.iter().find(|r| Some(&r.id) == rule.as_ref()).map(|r| format!("IP block {}", r.ip)));
    let answer = match &decision {
        Decision::Deny { persist } => json!({"status": 403, "hold_s": persist.map(|d| d.as_secs())}),
        Decision::RateLimited { persist, .. } => json!({"status": 429, "hold_s": persist.map(|d| d.as_secs())}),
        Decision::Challenge { persist } => json!({"status": 403, "challenge": true, "hold_s": persist.map(|d| d.as_secs())}),
        Decision::Redirect { location, status } => json!({"status": status, "location": location}),
        _ => json!({"status": null}),
    };
    // The rate limits that would count it (a test counts nothing, so none is over).
    let counted: Vec<Value> = counted
        .0
        .into_inner()
        .into_iter()
        .map(|(id, spec)| {
            let name = config.rules.iter().find(|r| r.id == id).map(|r| r.name.clone());
            json!({"rule": id, "name": name, "limit": spec.limit, "window_s": spec.window_ns / 1_000_000_000})
        })
        .collect();
    Ok(json!({"action": match decision.name() { "" => "allow", n => n }, "rule": rule, "name": name, "answer": answer, "counted": counted}))
}

/// Notes which rate limits a test request would count toward, counting nothing.
#[derive(Default)]
struct Counted(std::cell::RefCell<Vec<(String, RateSpec)>>);

impl Limiter for Counted {
    fn hit(&self, rule: &str, spec: RateSpec, _: &[u8]) -> Hit {
        self.0.borrow_mut().push((rule.to_owned(), spec));
        Hit::UNDER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(action: &str, id: Option<&str>, value: Value) -> Change {
        Change { action: action.into(), id: id.map(str::to_owned), value }
    }

    fn deny_path(name: &str, path: &str) -> Value {
        json!({"name": name, "active": true, "conditionGroup": [{"conditions": [{"type": "path", "op": "pre", "value": path}]}], "action": {"mitigate": {"action": "deny"}}})
    }

    #[test]
    fn attack_mode_bypasses_and_the_pause_change_the_table_at_once() {
        let mut routes: Routes = serde_json::from_value(json!({
            "deployments": {"d1": {"project": "api", "function": "f", "qualifier": "1", "target": "production"}},
            "hosts": {"api.forkrail.com": "d1"},
            "domains": {"api.example.com": {"project": "api"}}
        }))
        .unwrap();
        let now = 1_800_000_000;
        let op = |v: Value| -> FirewallOp { serde_json::from_value(v).unwrap() };
        change_mitigations(&mut routes, Some("api"), &op(json!({"op": "attack_mode", "duration": "6h"})), Some("ana"), now).unwrap();
        let m = &routes.projects["api"].mitigations;
        assert_eq!((m.attack_mode_until, m.changed_by.as_deref()), (Some(now + 6 * 3600), Some("ana")));
        assert!(change_mitigations(&mut routes, Some("api"), &op(json!({"op": "attack_mode", "duration": "2h"})), None, now).unwrap_err().to_string().contains("1h, 6h or 24h"));
        assert!(change_mitigations(&mut routes, None, &op(json!({"op": "attack_mode"})), None, now).is_err());
        change_mitigations(&mut routes, Some("api"), &op(json!({"op": "attack_mode", "off": true})), None, now).unwrap();
        assert!(!routes.projects.contains_key("api"), "an empty section goes");

        // A project's bypass: its own hosts only; ranges written as ranges; no twins.
        change_mitigations(&mut routes, Some("api"), &op(json!({"op": "system_bypass_add", "source_ip": "192.0.2.77/24", "domain": "API.example.com", "note": "partner"})), None, now).unwrap();
        let b = &routes.projects["api"].mitigations.system_bypass[0];
        assert_eq!((b.source_ip.as_str(), b.domain.as_str()), ("192.0.2.0/24", "api.example.com"));
        assert!(change_mitigations(&mut routes, Some("api"), &op(json!({"op": "system_bypass_add", "source_ip": "192.0.2.0/24", "domain": "api.example.com"})), None, now).is_err());
        assert!(change_mitigations(&mut routes, Some("api"), &op(json!({"op": "system_bypass_add", "source_ip": "192.0.2.1", "domain": "other.test"})), None, now).unwrap_err().to_string().contains("isn't one of api's hosts"));
        change_mitigations(&mut routes, Some("api"), &op(json!({"op": "system_bypass_add", "source_ip": "2001:db8::1"})), None, now).unwrap();
        change_mitigations(&mut routes, Some("api"), &op(json!({"op": "system_bypass_remove", "id": "192.0.2.0/24"})), None, now).unwrap();
        assert_eq!(routes.projects["api"].mitigations.system_bypass.len(), 1);
        assert!(change_mitigations(&mut routes, Some("api"), &op(json!({"op": "system_bypass_remove", "id": "198.51.100.1"})), None, now).is_err());
        // The install's: any host; the pause is the install's only.
        change_mitigations(&mut routes, None, &op(json!({"op": "system_bypass_add", "source_ip": "198.51.100.0/24", "domain": "*.anything.test"})), None, now).unwrap();
        change_mitigations(&mut routes, None, &op(json!({"op": "pause"})), Some("owner"), now).unwrap();
        assert_eq!(routes.mitigations.paused_until, Some(now + 24 * 3600));
        assert!(change_mitigations(&mut routes, Some("api"), &op(json!({"op": "pause"})), None, now).is_err());
        change_mitigations(&mut routes, None, &op(json!({"op": "resume"})), None, now).unwrap();
        assert_eq!(routes.mitigations.paused_until, None);
        assert!(change_mitigations(&mut routes, None, &op(json!({"op": "system_bypass_add", "source_ip": "nope"})), None, now).is_err());
        // The limits: the install's, in range, a default not kept, reset to the routers' own.
        change_mitigations(&mut routes, None, &op(json!({"op": "limits", "connections": 1024, "requests": 5000})), Some("owner"), now).unwrap();
        assert_eq!(routes.mitigations.limits, Some(ShieldLimits { connections: Some(1024), ..ShieldLimits::default() }));
        change_mitigations(&mut routes, None, &op(json!({"op": "limits", "block_s": 120})), None, now).unwrap();
        assert_eq!(routes.mitigations.limits.unwrap().connections, Some(1024));
        assert!(change_mitigations(&mut routes, None, &op(json!({"op": "limits", "requests": 10})), None, now).unwrap_err().to_string().contains("from 100"));
        assert!(change_mitigations(&mut routes, Some("api"), &op(json!({"op": "limits", "connections": 512})), None, now).unwrap_err().to_string().contains("--install"));
        assert_eq!(limits_view(routes.mitigations.limits)["requests"], 5000);
        change_mitigations(&mut routes, None, &op(json!({"op": "limits", "reset": true})), None, now).unwrap();
        assert_eq!(routes.mitigations.limits, None);
        assert_eq!(limits_view(None)["custom"], false);
    }

    #[test]
    fn changes_apply_as_vercel_names_them() {
        let mut c = Config::default();
        apply(&mut c, change("rules.insert", None, deny_path("admin", "/admin"))).unwrap();
        apply(&mut c, change("rules.insert", None, deny_path("env", "/.env"))).unwrap();
        c.assign_ids();
        let first = c.rules[0].id.clone();
        apply(&mut c, change("rules.priority", Some("env"), json!(0))).unwrap();
        assert_eq!(c.rules[0].name, "env");
        apply(&mut c, change("rules.update", Some(&first), deny_path("admin area", "/admin/"))).unwrap();
        assert_eq!((c.rules[1].id.as_str(), c.rules[1].name.as_str()), (first.as_str(), "admin area"));
        apply(&mut c, change("rules.active", Some("env"), json!(false))).unwrap();
        assert!(!c.rules[0].active);
        apply(&mut c, change("ip.insert", None, json!({"hostname": "*", "ip": "192.0.2.0/24", "action": "deny"}))).unwrap();
        apply(&mut c, change("lists.set", Some("bad"), json!(["198.51.100.1"]))).unwrap();
        assert!(apply(&mut c, change("bypass.insert", None, json!({"domain": "*", "sourceIp": "203.0.113.5"}))).is_err());
        apply(&mut c, change("firewallEnabled", None, json!(false))).unwrap();
        c.assign_ids();
        c.validate().unwrap();
        apply(&mut c, change("ip.remove", Some("192.0.2.0/24"), Value::Null)).unwrap();
        apply(&mut c, change("rules.remove", Some("env"), Value::Null)).unwrap();
        assert_eq!(c.rules.len(), 1);
        assert!(c.ips.is_empty());
        assert!(apply(&mut c, change("rules.remove", Some("nope"), Value::Null)).is_err());
        assert!(apply(&mut c, change("managedRules.update", None, Value::Null)).unwrap_err().to_string().contains("hosted plan"));
        assert!(apply(&mut c, change("rules.update", None, deny_path("x", "/"))).unwrap_err().to_string().contains("needs the id"));
    }

    #[test]
    fn diff_names_what_publishing_changes() {
        let mut live = Config::default();
        apply(&mut live, change("rules.insert", None, deny_path("a", "/a"))).unwrap();
        apply(&mut live, change("rules.insert", None, deny_path("b", "/b"))).unwrap();
        apply(&mut live, change("ip.insert", None, json!({"hostname": "*", "ip": "192.0.2.1", "action": "deny"}))).unwrap();
        live.assign_ids();
        let mut draft = live.clone();
        apply(&mut draft, change("rules.priority", Some("b"), json!(0))).unwrap();
        apply(&mut draft, change("rules.update", Some("a"), deny_path("a", "/aa"))).unwrap();
        apply(&mut draft, change("ip.remove", Some("192.0.2.1"), Value::Null)).unwrap();
        apply(&mut draft, change("rules.insert", None, deny_path("c", "/c"))).unwrap();
        draft.assign_ids();
        let d = diff(&live, &draft);
        let kinds: Vec<(String, String)> = d.as_array().unwrap().iter().map(|x| (x["kind"].as_str().unwrap().to_owned(), x["change"].as_str().unwrap().to_owned())).collect();
        assert!(kinds.contains(&("rule".into(), "moved".into())));
        assert!(kinds.contains(&("rule".into(), "changed".into())));
        assert!(kinds.contains(&("rule".into(), "added".into())));
        assert!(kinds.contains(&("ip".into(), "removed".into())));
        assert_eq!(diff(&live, &live), json!([]));
    }

    #[test]
    fn a_test_request_says_which_rule_decides() {
        let mut c = Config::default();
        apply(&mut c, change("rules.insert", None, deny_path("admin", "/admin"))).unwrap();
        apply(&mut c, change("ip.insert", None, json!({"hostname": "*", "ip": "198.51.100.0/24", "action": "deny"}))).unwrap();
        c.assign_ids();
        let none = BTreeMap::new();
        let out = test(&c, "https://app.example.com/admin/x", "GET", &none, None, None, None, None).unwrap();
        assert_eq!((out["action"].as_str(), out["name"].as_str(), out["answer"]["status"].as_u64()), (Some("deny"), Some("admin"), Some(403)));
        let out = test(&c, "/home", "GET", &none, Some("198.51.100.4"), None, None, None).unwrap();
        assert_eq!(out["name"], "IP block 198.51.100.0/24");
        assert_eq!(test(&c, "/home", "GET", &none, None, None, None, None).unwrap()["action"], "allow");
    }
}
