//! A project's own bulk redirects and routing rules: changed without a redeploy, from the
//! dashboard, the CLI or the API (all through here), as Vercel's project-level redirects and
//! routing rules are.
//!
//! Every change makes a new version, staged: it routes nothing until it's published. Publishing
//! (or restoring an older version) puts its table or program in every region's bucket, then names
//! it in the project's section of `state/routes.json`, which every router follows within seconds.
//!
//! In the home bucket:
//! - `state/routing/<project>/redirects.json` and `routes.json`: each set's versions, newest
//!   first, and which is live and which staged.
//! - `static/redirects/<sha>`: a version's redirects, as the router's table (`rules::bulk`).
//! - `state/routing/<project>/rules/<sha>.json`: a version's rules as written; its compiled
//!   program at `static/rules/<sha>.json`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use forkrail_common::install::project_key;
use forkrail_common::routes::Routes;
use forkrail_common::rules::bulk::{self, BulkRedirect, Table};
use forkrail_common::rules::project::{self as rules_project, ProjectRule, RuleType};
use forkrail_common::rules::{Engine, Files, Outcome, Request};
use forkrail_common::statics::{redirects_key, rules_key};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::deploy::Ctx;
use crate::state::{Project, ROUTES_KEY, now_rfc3339};
use crate::statics;

/// Versions kept of each set; older ones go (their objects stay until swept).
pub(crate) const KEEP_VERSIONS: usize = 100;
/// What a list returns at most at once.
const PAGE: usize = 1000;

/// Which of a project's two sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Set {
    Redirects,
    Routes,
}

impl Set {
    fn name(self) -> &'static str {
        match self {
            Set::Redirects => "redirects",
            Set::Routes => "routes",
        }
    }
}

fn versions_key(project: &str, set: Set) -> String {
    format!("state/routing/{project}/{}.json", set.name())
}

fn rules_list_key(project: &str, sha: &str) -> String {
    format!("state/routing/{project}/rules/{sha}.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Versions {
    /// The version the routers run, if any.
    #[serde(default)]
    pub live: Option<String>,
    /// Changes not yet published.
    #[serde(default)]
    pub staging: Option<String>,
    /// Newest first.
    #[serde(default)]
    pub versions: Vec<Version>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    /// The redirects' table, or the rules as written, by SHA-256.
    pub object: String,
    /// For rules: the compiled program's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub count: usize,
    pub created: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// What changed, in a few words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// When it was last made live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
}

impl Versions {
    pub(crate) fn get(&self, id: &str) -> Option<&Version> {
        self.versions.iter().find(|v| v.id == id || (id.len() >= 6 && v.id.starts_with(id)))
    }

    /// The version changes start from: the staged one, else the live one.
    pub(crate) fn working(&self) -> Option<&Version> {
        self.staging.as_deref().or(self.live.as_deref()).and_then(|id| self.get(id))
    }

    /// The version `which` names: `live`, `staging`, or an id (or a prefix of one).
    pub(crate) fn pick(&self, which: Option<&str>) -> Result<Option<&Version>> {
        Ok(match which {
            None => self.working(),
            Some("live" | "production") => self.live.as_deref().and_then(|id| self.get(id)),
            Some("staging") => self.staging.as_deref().and_then(|id| self.get(id)),
            Some(id) => Some(self.get(id).with_context(|| format!("there's no version {id}"))?),
        })
    }
}

/// The versions of a project's set, with a check that the project exists.
pub async fn versions(ctx: &Ctx, project: &str, set: Set) -> Result<Versions> {
    if ctx.store.get_value::<Project>(&project_key(project)).await?.is_none() {
        bail!("there's no project {project}");
    }
    Ok(ctx.store.get_value(&versions_key(project, set)).await?.unwrap_or_default())
}

/// Record a new version (its object already stored) and make it the staged one.
async fn stage(ctx: &Ctx, project: &str, set: Set, mut version: Version, base: Option<String>) -> Result<Version> {
    version.id = format!("v_{}", forkrail_common::random_hex(6));
    version.created = now_rfc3339();
    let saved = version.clone();
    ctx.store
        .update::<Versions, _>(&versions_key(project, set), move |current| {
            let mut v = current.unwrap_or_default();
            // Two people changing the same set at once: the second starts again from the first's.
            let working = v.working().map(|w| w.id.clone());
            if working != base {
                bail!("the {} changed while this change was made; try again", set.name());
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
    Ok(saved)
}

/// Make version `which` (default: the staged one) live: its objects in every region, then named
/// in the routing table.
pub async fn publish(ctx: &Ctx, project: &str, set: Set, which: Option<&str>) -> Result<Value> {
    let v = versions(ctx, project, set).await?;
    let version = match which {
        None => v.staging.as_deref().and_then(|id| v.get(id)).context("nothing is staged: there's nothing to publish")?,
        Some(id) => v.pick(Some(id))?.context("no such version")?,
    }
    .clone();
    let settings = ctx.store.settings().await?;
    let home = ctx.install.home_region.clone();
    let regions: Vec<String> = std::iter::once(home.clone()).chain(settings.regions.iter().filter(|r| **r != home).cloned()).collect();
    let live_id = match set {
        Set::Redirects => (version.count > 0).then(|| version.object.clone()),
        Set::Routes => version.program.clone().filter(|_| version.count > 0),
    };
    if let Some(id) = &live_id {
        for region in &regions {
            match set {
                Set::Redirects => statics::upload_object(ctx, region, None, &redirects_key(id), "application/octet-stream").await,
                Set::Routes => statics::upload_object(ctx, region, None, &rules_key(id), "application/json").await,
            }
            .with_context(|| format!("copying to {region}"))?;
        }
    }
    let name = project.to_owned();
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            let entry = routes.projects.entry(name.clone()).or_default();
            match set {
                Set::Redirects => entry.redirects = live_id.clone(),
                Set::Routes => entry.routing = live_id.clone(),
            }
            if entry.is_empty() {
                routes.projects.remove(&name);
            }
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    let id = version.id.clone();
    let when = now_rfc3339();
    ctx.store
        .update::<Versions, _>(&versions_key(project, set), move |current| {
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
    Ok(json!({"project": project, "live": version.id, "count": version.count, "regions": regions}))
}

/// Drop the staged changes.
pub async fn discard(ctx: &Ctx, project: &str, set: Set) -> Result<Value> {
    versions(ctx, project, set).await?;
    let v = ctx
        .store
        .update::<Versions, _>(&versions_key(project, set), |current| {
            let mut v = current.unwrap_or_default();
            v.staging = None;
            Ok(v)
        })
        .await?;
    Ok(json!({"project": project, "live": v.live}))
}

/// Versions, newest first, for lists.
pub fn describe(v: &Versions) -> Value {
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

// ---- Bulk redirects ----

/// What the dashboard, the CLI and the API ask of a project's redirects.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RedirectsOp {
    /// One page of a version's redirects (`version`: live, staging, or an id; default the staged
    /// one, else the live one), those whose source or destination contains `query`.
    List {
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        limit: Option<usize>,
    },
    Versions,
    /// Add or change redirects (one with the same source is replaced).
    Add { redirects: Vec<BulkRedirect> },
    /// Remove redirects by source.
    Remove { sources: Vec<String> },
    /// Redirects from a file's text (`name` says its format: `.csv`, `.json`, `.jsonl`), added to
    /// the current ones, or instead of them with `replace`.
    Import {
        name: String,
        text: String,
        #[serde(default)]
        replace: bool,
    },
    /// A version's redirects as CSV.
    Export {
        #[serde(default)]
        version: Option<String>,
    },
    Publish {
        #[serde(default)]
        version: Option<String>,
    },
    Restore { version: String },
    Discard,
    /// What a URL gets from a version (default the staged one, else the live one).
    Test {
        url: String,
        #[serde(default)]
        version: Option<String>,
    },
}

impl RedirectsOp {
    /// Its name, for logs.
    pub fn name(&self) -> &'static str {
        match self {
            RedirectsOp::List { .. } => "list",
            RedirectsOp::Versions => "versions",
            RedirectsOp::Add { .. } => "add",
            RedirectsOp::Remove { .. } => "remove",
            RedirectsOp::Import { .. } => "import",
            RedirectsOp::Export { .. } => "export",
            RedirectsOp::Publish { .. } => "publish",
            RedirectsOp::Restore { .. } => "restore",
            RedirectsOp::Discard => "discard",
            RedirectsOp::Test { .. } => "test",
        }
    }

    /// Whether it changes anything (and so is for owners and members).
    pub fn changes(&self) -> bool {
        !matches!(self, RedirectsOp::List { .. } | RedirectsOp::Versions | RedirectsOp::Export { .. } | RedirectsOp::Test { .. })
    }
}

async fn read_table(ctx: &Ctx, version: Option<&Version>) -> Result<Option<Table<Vec<u8>>>> {
    let Some(version) = version else { return Ok(None) };
    if version.count == 0 {
        return Ok(None);
    }
    let bytes = ctx.store.get_bytes(&redirects_key(&version.object)).await?.with_context(|| format!("version {}'s redirects are gone", version.id))?;
    Ok(Some(Table::new(bytes).map_err(anyhow::Error::msg)?))
}

async fn current_redirects(ctx: &Ctx, v: &Versions) -> Result<Vec<BulkRedirect>> {
    Ok(read_table(ctx, v.working()).await?.map(|t| t.entries().collect()).unwrap_or_default())
}

/// Store a set of redirects as a new staged version.
async fn stage_redirects(ctx: &Ctx, project: &str, list: Vec<BulkRedirect>, base: Option<String>, actor: Option<&str>, note: String) -> Result<Value> {
    let table = tokio::task::spawn_blocking(move || bulk::build(&list).map(|t| (t, list.len()))).await?.map_err(anyhow::Error::msg)?;
    let (table, count) = table;
    let sha = forkrail_common::hex(&Sha256::digest(&table));
    ctx.store.put_bytes(&redirects_key(&sha), table, "application/octet-stream").await?;
    let version = Version { id: String::new(), object: sha, program: None, count, created: String::new(), created_by: actor.map(str::to_owned), note: Some(note), published: None };
    let v = stage(ctx, project, Set::Redirects, version, base).await?;
    Ok(json!({"project": project, "staging": v.id, "count": v.count}))
}

pub async fn redirects(ctx: &Ctx, project: &str, op: RedirectsOp, actor: Option<&str>) -> Result<Value> {
    let v = versions(ctx, project, Set::Redirects).await?;
    let base = v.working().map(|w| w.id.clone());
    match op {
        RedirectsOp::Versions => Ok(describe(&v)),
        RedirectsOp::List { version, query, offset, limit } => {
            let picked = v.pick(version.as_deref())?;
            let table = read_table(ctx, picked).await?;
            let needle = query.map(|q| q.to_lowercase()).filter(|q| !q.is_empty());
            let matching: Vec<BulkRedirect> = table
                .as_ref()
                .map(|t| {
                    t.entries()
                        .filter(|r| needle.as_ref().is_none_or(|q| r.source.to_lowercase().contains(q) || r.destination.to_lowercase().contains(q)))
                        .collect()
                })
                .unwrap_or_default();
            let mut matching = matching;
            matching.sort_by(|a, b| a.source.cmp(&b.source));
            let total = matching.len();
            let limit = limit.unwrap_or(PAGE).min(PAGE);
            let page: Vec<BulkRedirect> = matching.into_iter().skip(offset).take(limit).collect();
            Ok(json!({"version": picked.map(|p| &p.id), "total": total, "offset": offset, "redirects": page, "live": v.live, "staging": v.staging}))
        }
        RedirectsOp::Add { redirects } => {
            if redirects.is_empty() {
                bail!("no redirects to add");
            }
            let mut list = current_redirects(ctx, &v).await?;
            let n = redirects.len();
            for r in redirects {
                r.check().map_err(anyhow::Error::msg)?;
                let key = bulk::source_key(&r.source, r.case_sensitive).map_err(anyhow::Error::msg)?;
                list.retain(|x| x.case_sensitive != r.case_sensitive || bulk::source_key(&x.source, x.case_sensitive).ok().as_deref() != Some(key.as_str()));
                list.push(r);
            }
            stage_redirects(ctx, project, list, base, actor, format!("added {n}")).await
        }
        RedirectsOp::Remove { sources } => {
            let mut list = current_redirects(ctx, &v).await?;
            let before = list.len();
            let keys: Vec<String> = sources.iter().filter_map(|s| bulk::source_key(s, false).ok()).collect();
            list.retain(|x| !(sources.contains(&x.source) || bulk::source_key(&x.source, false).is_ok_and(|k| keys.contains(&k))));
            let removed = before - list.len();
            if removed == 0 {
                bail!("none of those sources is a redirect");
            }
            stage_redirects(ctx, project, list, base, actor, format!("removed {removed}")).await
        }
        RedirectsOp::Import { name, text, replace } => {
            let imported = bulk::parse(&name, &text).map_err(anyhow::Error::msg)?;
            let n = imported.len();
            let list = if replace {
                imported
            } else {
                let mut list = current_redirects(ctx, &v).await?;
                let mut by_key: BTreeMap<(String, bool), usize> = BTreeMap::new();
                for (i, r) in list.iter().enumerate() {
                    if let Ok(k) = bulk::source_key(&r.source, r.case_sensitive) {
                        by_key.insert((k, r.case_sensitive), i);
                    }
                }
                for r in imported {
                    let k = (bulk::source_key(&r.source, r.case_sensitive).map_err(anyhow::Error::msg)?, r.case_sensitive);
                    match by_key.get(&k) {
                        Some(&i) => list[i] = r,
                        None => {
                            by_key.insert(k, list.len());
                            list.push(r);
                        }
                    }
                }
                list
            };
            let note = if replace { format!("imported {n} from {name}, replacing all") } else { format!("imported {n} from {name}") };
            stage_redirects(ctx, project, list, base, actor, note).await
        }
        RedirectsOp::Export { version } => {
            let table = read_table(ctx, v.pick(version.as_deref())?).await?;
            let csv = match table {
                Some(t) => bulk::to_csv(t.entries()),
                None => bulk::to_csv(std::iter::empty()),
            };
            Ok(json!({"csv": csv}))
        }
        RedirectsOp::Publish { version } => publish(ctx, project, Set::Redirects, version.as_deref()).await,
        RedirectsOp::Restore { version } => publish(ctx, project, Set::Redirects, Some(&version)).await,
        RedirectsOp::Discard => discard(ctx, project, Set::Redirects).await,
        RedirectsOp::Test { url, version } => {
            let picked = v.pick(version.as_deref())?;
            let (host, path, query) = split_url(&url)?;
            let table = read_table(ctx, picked).await?;
            let found = table.as_ref().and_then(|t| t.lookup(&host, &path).map(|f| json!({"status": f.status, "location": f.location(query.as_deref())})));
            Ok(json!({"version": picked.map(|p| &p.id), "url": url, "redirect": found}))
        }
    }
}

/// A URL (or a path) to test, as host, path and query.
pub(crate) fn split_url(url: &str) -> Result<(String, String, Option<String>)> {
    let url = url.trim();
    let (host, rest) = if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        (rest[..end].to_ascii_lowercase(), &rest[end..])
    } else if url.starts_with('/') {
        (String::new(), url)
    } else {
        bail!("give a URL (https://example.com/path) or a path starting with /");
    };
    let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host.clone(), |(h, _)| h.to_owned());
    let rest = rest.split('#').next().unwrap_or("");
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q.to_owned())),
        None => (rest, None),
    };
    Ok((host, if path.is_empty() { "/".into() } else { path.to_owned() }, query))
}

// ---- Routing rules ----

/// Where a rule goes in the list.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", tag = "placement")]
pub enum Position {
    Start,
    End,
    Before { id: String },
    After { id: String },
    /// 1-based.
    At { index: usize },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RoutesOp {
    List {
        #[serde(default)]
        version: Option<String>,
    },
    Versions,
    /// A new rule (its id is made here), at `position` (default the end).
    Add {
        rule: Value,
        #[serde(default)]
        position: Option<Position>,
    },
    /// Replace rule `id` (by id or name).
    Update { id: String, rule: Value },
    Remove { ids: Vec<String> },
    Enable { id: String, enabled: bool },
    Move { id: String, position: Position },
    /// Every rule at once (the API's stage with overwrite).
    Replace { rules: Value },
    Publish {
        #[serde(default)]
        version: Option<String>,
    },
    Restore { version: String },
    Discard,
    /// What a request gets from a version's rules: the rule that decides, and what it does.
    Test {
        url: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        version: Option<String>,
    },
}

impl RoutesOp {
    /// Its name, for logs.
    pub fn name(&self) -> &'static str {
        match self {
            RoutesOp::List { .. } => "list",
            RoutesOp::Versions => "versions",
            RoutesOp::Add { .. } => "add",
            RoutesOp::Update { .. } => "update",
            RoutesOp::Remove { .. } => "remove",
            RoutesOp::Enable { .. } => "enable",
            RoutesOp::Move { .. } => "move",
            RoutesOp::Replace { .. } => "replace",
            RoutesOp::Publish { .. } => "publish",
            RoutesOp::Restore { .. } => "restore",
            RoutesOp::Discard => "discard",
            RoutesOp::Test { .. } => "test",
        }
    }

    pub fn changes(&self) -> bool {
        !matches!(self, RoutesOp::List { .. } | RoutesOp::Versions | RoutesOp::Test { .. })
    }
}

async fn read_rules(ctx: &Ctx, project: &str, version: Option<&Version>) -> Result<Vec<ProjectRule>> {
    let Some(version) = version else { return Ok(Vec::new()) };
    ctx.store.get_value(&rules_list_key(project, &version.object)).await?.with_context(|| format!("version {}'s rules are gone", version.id))
}

/// A rule, with what it is for lists.
fn shown(rule: &ProjectRule) -> Value {
    let mut v = serde_json::to_value(rule).unwrap_or(Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("type".into(), json!(rule.kind()));
        obj.insert("srcSyntax".into(), json!(rule.syntax()));
    }
    v
}

async fn stage_rules(ctx: &Ctx, project: &str, rules: Vec<ProjectRule>, base: Option<String>, actor: Option<&str>, note: String) -> Result<Value> {
    let program = rules_project::compile(&rules).map_err(anyhow::Error::msg)?;
    let list = serde_json::to_vec(&rules)?;
    let sha = forkrail_common::hex(&Sha256::digest(&list));
    ctx.store.put_bytes(&rules_list_key(project, &sha), list, "application/json").await?;
    let compiled = serde_json::to_vec(&program)?;
    let program_id = forkrail_common::hex(&Sha256::digest(&compiled));
    ctx.store.put_bytes(&rules_key(&program_id), compiled, "application/json").await?;
    let count = rules.len();
    let version = Version { id: String::new(), object: sha, program: Some(program_id), count, created: String::new(), created_by: actor.map(str::to_owned), note: Some(note), published: None };
    let v = stage(ctx, project, Set::Routes, version, base).await?;
    Ok(json!({"project": project, "staging": v.id, "rules": rules.iter().map(shown).collect::<Vec<_>>()}))
}

fn find(rules: &[ProjectRule], id: &str) -> Result<usize> {
    rules.iter().position(|r| r.id == id).or_else(|| rules.iter().position(|r| r.name == id)).with_context(|| format!("there's no rule {id}"))
}

fn place(rules: &[ProjectRule], position: Option<&Position>) -> Result<usize> {
    Ok(match position {
        None | Some(Position::End) => rules.len(),
        Some(Position::Start) => 0,
        Some(Position::Before { id }) => find(rules, id)?,
        Some(Position::After { id }) => find(rules, id)? + 1,
        Some(Position::At { index }) => index.saturating_sub(1).min(rules.len()),
    })
}

fn one_rule(value: Value, id: String) -> Result<ProjectRule> {
    let mut value = value;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("id".into(), Value::String(id));
        obj.remove("type");
    }
    serde_json::from_value(value).context("not a rule")
}

pub async fn routes(ctx: &Ctx, project: &str, op: RoutesOp, actor: Option<&str>) -> Result<Value> {
    let v = versions(ctx, project, Set::Routes).await?;
    let base = v.working().map(|w| w.id.clone());
    match op {
        RoutesOp::Versions => Ok(describe(&v)),
        RoutesOp::List { version } => {
            let picked = v.pick(version.as_deref())?;
            let rules = read_rules(ctx, project, picked).await?;
            Ok(json!({"version": picked.map(|p| &p.id), "live": v.live, "staging": v.staging, "rules": rules.iter().map(shown).collect::<Vec<_>>()}))
        }
        RoutesOp::Add { rule, position } => {
            let mut rules = read_rules(ctx, project, v.working()).await?;
            let rule = one_rule(rule, rules_project::new_id())?;
            let at = place(&rules, position.as_ref())?;
            let note = format!("added {:?}", rule.name);
            rules.insert(at, rule);
            stage_rules(ctx, project, rules, base, actor, note).await
        }
        RoutesOp::Update { id, rule } => {
            let mut rules = read_rules(ctx, project, v.working()).await?;
            let at = find(&rules, &id)?;
            let rule = one_rule(rule, rules[at].id.clone())?;
            let note = format!("changed {:?}", rule.name);
            rules[at] = rule;
            stage_rules(ctx, project, rules, base, actor, note).await
        }
        RoutesOp::Remove { ids } => {
            let mut rules = read_rules(ctx, project, v.working()).await?;
            let mut names = Vec::new();
            for id in &ids {
                let at = find(&rules, id)?;
                names.push(format!("{:?}", rules.remove(at).name));
            }
            stage_rules(ctx, project, rules, base, actor, format!("removed {}", names.join(", "))).await
        }
        RoutesOp::Enable { id, enabled } => {
            let mut rules = read_rules(ctx, project, v.working()).await?;
            let at = find(&rules, &id)?;
            rules[at].enabled = enabled;
            let note = format!("{} {:?}", if enabled { "enabled" } else { "disabled" }, rules[at].name);
            stage_rules(ctx, project, rules, base, actor, note).await
        }
        RoutesOp::Move { id, position } => {
            let mut rules = read_rules(ctx, project, v.working()).await?;
            let at = find(&rules, &id)?;
            let rule = rules.remove(at);
            let to = place(&rules, Some(&position))?;
            let note = format!("moved {:?} to {}", rule.name, to + 1);
            rules.insert(to, rule);
            stage_rules(ctx, project, rules, base, actor, note).await
        }
        RoutesOp::Replace { rules } => {
            let rules = rules_project::from_json(&rules).map_err(anyhow::Error::msg)?;
            let note = format!("set {} rules", rules.len());
            stage_rules(ctx, project, rules, base, actor, note).await
        }
        RoutesOp::Publish { version } => publish(ctx, project, Set::Routes, version.as_deref()).await,
        RoutesOp::Restore { version } => publish(ctx, project, Set::Routes, Some(&version)).await,
        RoutesOp::Discard => discard(ctx, project, Set::Routes).await,
        RoutesOp::Test { url, method, headers, version } => {
            let picked = v.pick(version.as_deref())?;
            let rules = read_rules(ctx, project, picked).await?;
            Ok(test_rules(&rules, &url, method.as_deref().unwrap_or("GET"), &headers)?)
        }
    }
}

/// What a request gets from `rules` alone: which rule decides and what it does, and every change
/// the rules make on the way. What the deployment does after them isn't shown.
pub fn test_rules(rules: &[ProjectRule], url: &str, method: &str, headers: &BTreeMap<String, String>) -> Result<Value> {
    let mut program = rules_project::compile(rules).map_err(anyhow::Error::msg)?;
    // Stands for the deployment: whatever the rules leave goes on to it.
    program.fallback = true;
    let engine = Engine::new(&program).map_err(anyhow::Error::msg)?;
    let (host, path, query) = split_url(url)?;
    let mut map = hyper::HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(k), Ok(v)) = (hyper::header::HeaderName::from_bytes(k.as_bytes()), hyper::header::HeaderValue::from_str(v)) {
            map.append(k, v);
        }
    }
    let host = if host.is_empty() { "example.com".to_owned() } else { host };
    // Every path is taken to be the deployment's, so a rewrite shows where it goes.
    struct Anything;
    impl Files for Anything {
        fn has(&self, _: &str) -> bool {
            true
        }
    }
    let d = engine.route(&Request { method, path: &path, query: query.as_deref(), host: &host, headers: &map }, &Anything);
    let rule = d.route.map(|i| engine.rule_name(i));
    let decided = rules.iter().find(|r| r.enabled && rule.as_deref() == Some(format!("project rule {:?}", r.name).as_str()));
    let kind = decided.map(ProjectRule::kind);
    let (action, to) = match (&d.outcome, kind) {
        (Outcome::Redirect { location }, _) => ("redirect", Some(location.clone())),
        (Outcome::External { url }, _) => ("rewrite", Some(url.clone())),
        (Outcome::Static { file }, Some(RuleType::Rewrite)) => ("rewrite", Some(file.clone())),
        (_, Some(RuleType::SetStatus)) => ("status", None),
        _ => ("continue", None),
    };
    Ok(json!({
        "url": url, "rule": rule.filter(|_| action != "continue"), "rule_id": decided.filter(|_| action != "continue").map(|r| &r.id), "action": action, "to": to, "status": d.status.filter(|_| action != "continue"),
        "headers": d.headers.iter().map(|(k, v)| json!({"key": k, "value": v})).collect::<Vec<_>>(),
        "transforms": d.transforms,
    }))
}
