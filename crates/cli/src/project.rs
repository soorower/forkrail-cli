//! A project's settings: the function settings its next deployments use (memory, maxDuration,
//! concurrent compute), the IAM role its functions run as, and the private targets its apps
//! reach. Like Vercel's project settings, a change applies to the next deploy or redeploy, never
//! to deployments that exist; a deployment's `vercel.json` wins over the function settings. The
//! cap on requests per worker with concurrent compute on is the exception: it applies at once.
//!
//! Deleting a project (`delete`) takes everything of it out of the install.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use forkrail_common::install::{DASHBOARD_PROJECT, WEB_TABLE, cancel_key, function_name, previews_prefix};
use forkrail_common::routes::Routes;
use serde::{Deserialize, Serialize};

use crate::deploy::{Ctx, check_project, log, new_project, project_record, warn};
use crate::publish::{DELETE_HOLDER, Lock};
use crate::secrets::Secrets;
use crate::state::{self, Project, ROUTES_KEY, s3_error};
use crate::{remote, schedules, storage};

/// Lambda's limits on a function's memory, in MB.
pub const MEMORY_MB: std::ops::RangeInclusive<u32> = 128..=10_240;
/// The longest a request may run, in seconds: Lambda's limit.
pub const MAX_DURATION_S: std::ops::RangeInclusive<u64> = 1..=900;
/// Requests one worker may take at once with concurrent compute on. 1 is concurrent compute off
/// (`Deployment::single`), which is its own setting. The dashboard offers less for a small
/// function (about one per 50 MB of memory, at least the routers' 10).
pub const MAX_INFLIGHT: std::ops::RangeInclusive<u32> = 2..=50;

/// What to change; `None` leaves a setting as it is, and `Some(None)` clears it.
#[derive(Debug, Default, Clone)]
pub struct Change {
    pub memory_mb: Option<Option<u32>>,
    pub max_duration_s: Option<Option<u64>>,
    pub concurrent: Option<Option<bool>>,
    /// With concurrent compute on, the most requests one worker takes at once; `Some(None)` back
    /// to the router's default. Applied at once to its deployments that run concurrent compute,
    /// in the routing table, as well as kept for its next ones.
    pub max_inflight: Option<Option<u32>>,
    /// An IAM role by name or ARN; `Some(None)` for the shared forkrail-worker.
    pub role: Option<Option<String>>,
    pub egress: Option<Vec<String>>,
    /// Its framework preset, a Vercel slug (backends::all); `Some(None)`: found from its files.
    pub framework: Option<Option<String>>,
    /// "Auto-assign custom domains"; `Some(None)` back to the default (on).
    pub auto_assign_domains: Option<Option<bool>>,
    /// Skew protection, which lives in the routing table and applies at once (skew.rs).
    pub skew: crate::skew::Change,
    /// Environments the routers keep ready per region for its production deployment (warm.rs):
    /// 0 keeps none, `auto` follows the last day's busiest moment, `Some(None)` is back to the
    /// routers' default. In the routing table too, in force within seconds.
    pub keep_warm: Option<Option<forkrail_common::routes::KeepWarm>>,
    /// Web Analytics on or off, in the routing table too, in force within seconds
    /// (web_analytics.rs).
    pub analytics: Option<bool>,
    /// The regions its functions run in, by AWS's or Vercel's names (regions.rs `parse`);
    /// `Some(None)` back to the default, the home region alone.
    pub regions: Option<Option<Vec<String>>>,
}

/// forkrail's own roles, which no project may run as (the dashboard's is the dashboard
/// project's; the image optimizer's are `forkrail-image-<region>`).
const INTERNAL_ROLES: [&str; 11] = [
    "forkrail-control",
    "forkrail-router",
    "forkrail-notify",
    "forkrail-guard",
    "forkrail-builder",
    "forkrail-screenshot",
    "forkrail-cost-tags",
    "forkrail-analytics",
    "forkrail-retention",
    "forkrail-scheduler",
    "forkrail-spend-backstop",
];

impl Change {
    pub fn is_empty(&self) -> bool {
        self.project_file_unchanged() && self.skew.is_empty() && self.keep_warm.is_none() && self.analytics.is_none()
    }

    /// Whether only keep-warm changes, which needs no redeploy to apply.
    pub fn only_keep_warm(&self) -> bool {
        self.project_file_unchanged() && self.skew.is_empty() && self.keep_warm.is_some()
    }

    /// Whether only the cap on requests per worker changes, which applies at once.
    pub fn only_max_inflight(&self) -> bool {
        self.max_inflight.is_some()
            && Change { max_inflight: None, ..self.clone() }.project_file_unchanged()
            && self.skew.is_empty()
            && self.keep_warm.is_none()
            && self.analytics.is_none()
    }

    /// Whether only Web Analytics changes, which needs no redeploy to apply.
    pub fn only_analytics(&self) -> bool {
        self.project_file_unchanged() && self.skew.is_empty() && self.keep_warm.is_none() && self.analytics.is_some()
    }

    /// Whether nothing kept in the project's own file changes (everything but skew protection
    /// and keep-warm).
    fn project_file_unchanged(&self) -> bool {
        self.memory_mb.is_none()
            && self.max_duration_s.is_none()
            && self.concurrent.is_none()
            && self.max_inflight.is_none()
            && self.role.is_none()
            && self.egress.is_none()
            && self.framework.is_none()
            && self.auto_assign_domains.is_none()
            && self.regions.is_none()
    }

    /// Whether only skew protection changes, which needs no redeploy to apply.
    pub fn only_skew(&self) -> bool {
        self.project_file_unchanged() && !self.skew.is_empty()
    }

    pub fn check(&self) -> Result<()> {
        self.skew.clone().check()?;
        if let Some(Some(forkrail_common::routes::KeepWarm::Count(n))) = self.keep_warm
            && n > forkrail_common::routes::KEEP_WARM_MAX
        {
            bail!("keep-warm {n}: at most {} environments per region", forkrail_common::routes::KEEP_WARM_MAX);
        }
        if let Some(Some(mb)) = self.memory_mb
            && !MEMORY_MB.contains(&mb)
        {
            bail!("memory {mb} MB: Lambda allows {} to {} MB", MEMORY_MB.start(), MEMORY_MB.end());
        }
        if let Some(Some(s)) = self.max_duration_s
            && !MAX_DURATION_S.contains(&s)
        {
            bail!("maxDuration {s} s: from {} to {} seconds", MAX_DURATION_S.start(), MAX_DURATION_S.end());
        }
        if let Some(Some(n)) = self.max_inflight
            && !MAX_INFLIGHT.contains(&n)
        {
            bail!("{n} requests per worker: from {} to {} (one request per worker is concurrent compute off)", MAX_INFLIGHT.start(), MAX_INFLIGHT.end());
        }
        if let Some(Some(role)) = &self.role
            && !role.starts_with("arn:")
        {
            let valid = !role.is_empty() && role.len() <= 64 && role.chars().all(|c| c.is_ascii_alphanumeric() || "+=,.@_-".contains(c));
            if !valid {
                bail!("{role:?} isn't an IAM role name or ARN");
            }
        }
        // forkrail's own roles but the dashboard's: each trusts Lambda and holds rights other parts
        // of the install count on being theirs alone (the notifier's decrypts every webhook secret).
        if let Some(Some(role)) = &self.role {
            let name = role.rsplit('/').next().unwrap_or(role);
            if INTERNAL_ROLES.contains(&name) || name.starts_with("forkrail-image-") {
                bail!("{name} is one of forkrail's own roles: give the project a role of its own");
            }
        }
        if let Some(name) = self.egress.iter().flatten().find(|n| !forkrail_common::routes::valid_egress_name(n)) {
            bail!("egress {name:?}: use 1-32 of a-z, 0-9 and -");
        }
        if let Some(Some(regions)) = &self.regions {
            crate::regions::parse(regions)?;
        }
        if let Some(Some(slug)) = &self.framework
            && !crate::backends::choosable(slug)
        {
            let known: Vec<&str> = crate::backends::all().iter().map(|(s, _, _)| *s).collect();
            bail!("framework {slug:?} isn't a preset forkrail runs ({})", known.join(", "));
        }
        Ok(())
    }
}

/// Every project file (a few dozen), read together.
pub async fn all(ctx: &Ctx) -> Result<Vec<Project>> {
    let keys = ctx.store.list(forkrail_common::install::PROJECTS_PREFIX).await?;
    let read = futures_util::future::join_all(keys.iter().map(|key| ctx.store.get_value::<Project>(key))).await;
    Ok(read.into_iter().collect::<Result<Vec<_>>>()?.into_iter().flatten().collect())
}

/// The project a name means: a project's id, or else the name a project was renamed to. A name
/// that is neither comes back as it is (a project about to be made).
pub async fn resolve(ctx: &Ctx, name: &str) -> Result<String> {
    if ctx.store.get_value::<Project>(&state::project_key(name)).await?.is_some() {
        return Ok(name.to_owned());
    }
    Ok(all(ctx).await?.into_iter().find(|p| p.display_name.as_deref() == Some(name)).map_or_else(|| name.to_owned(), |p| p.name))
}

/// The project now called `name` after a rename, or whose domain `<name>.<base>` is (a renamed
/// project may take its name's address as a domain, and keep it after a later rename), if any.
pub async fn taken_by(ctx: &Ctx, name: &str) -> Result<Option<String>> {
    if let Some(p) = all(ctx).await?.into_iter().find(|p| p.display_name.as_deref() == Some(name)) {
        return Ok(Some(p.name));
    }
    let base = ctx.store.settings().await?.base_domain;
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    Ok(routes.domains.get(&format!("{name}.{base}")).map(|d| d.project.clone()))
}

/// A project about to be made (by a template, a repository, its first deploy, env var or
/// setting): refused when another project answers to the name.
pub async fn check_new(ctx: &Ctx, name: &str) -> Result<()> {
    if let Some(other) = taken_by(ctx, name).await? {
        bail!("{name} is taken by {other} (what it's called now, or its domain under the install's): pick another name");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Renamed {
    /// The project's id, which nothing changes.
    pub project: String,
    /// What it's called now, and before.
    pub name: String,
    pub previous: String,
}

/// Call the project `to`. Only the name shown changes: its id, and with it its addresses,
/// functions, stored files, env values and logs, stay as they are. `to` follows the rules of
/// project names and can't be another project's id or name; its own id puts the first name back.
pub async fn rename(ctx: &Ctx, project: &str, to: &str) -> Result<Renamed> {
    check_project(project)?;
    let to = to.trim();
    check_project(to)?;
    if let Some(other) = all(ctx).await?.into_iter().find(|p| p.name != project && (p.name == to || p.display_name.as_deref() == Some(to))) {
        if other.name == to {
            bail!("{to:?} is another project's name; pick another");
        }
        bail!("{to:?} is what {} is called; pick another", other.name);
    }
    if let Some(other) = taken_by(ctx, to).await?.filter(|p| p != project) {
        bail!("{to}.{} is a domain of {other}; pick another name", ctx.store.settings().await?.base_domain);
    }
    let mut previous = String::new();
    let p = ctx
        .store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = current.with_context(|| format!("no project {project}"))?;
            previous = p.shown_name().to_owned();
            p.display_name = (to != project).then(|| to.to_owned());
            Ok(p)
        })
        .await?;
    log(&format!("{project}: called {} (was {previous})", p.shown_name()));
    Ok(Renamed { project: project.to_owned(), name: p.shown_name().to_owned(), previous })
}

/// Apply `change` to the project and return it as it is now. With `create`, a project that doesn't
/// exist yet is created, so it can be set up before its first deploy.
pub async fn update(ctx: &Ctx, project: &str, change: &Change, create: bool) -> Result<Project> {
    check_project(project)?;
    change.check()?;
    if change.is_empty() || (change.project_file_unchanged() && !create) {
        let p = project_record(ctx, project).await?;
        if !change.skew.is_empty() {
            crate::skew::update(ctx, project, &change.skew).await?;
        }
        if let Some(count) = change.keep_warm {
            crate::warm::keep_set(ctx, project, count).await?;
        }
        if let Some(on) = change.analytics {
            crate::web_analytics::set(ctx, project, on).await?;
        }
        return Ok(p);
    }
    if create && !ctx.store.exists(&state::project_key(project)).await? {
        check_new(ctx, project).await?;
    }
    // Regions functions can't run in yet are refused now, rather than at the next deploy.
    if let Some(Some(names)) = &change.regions {
        crate::regions::check_ready(ctx, &crate::regions::parse(names)?).await?;
    }
    let p = ctx
        .store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = match current {
                Some(p) => p,
                None if create => new_project(project),
                None => bail!("no project {project}"),
            };
            if let Some(v) = change.memory_mb {
                p.memory_mb = v;
            }
            if let Some(v) = change.max_duration_s {
                p.max_duration_s = v;
            }
            if let Some(v) = change.concurrent {
                p.concurrent = v;
            }
            if let Some(v) = change.max_inflight {
                p.max_inflight = v;
            }
            if let Some(v) = &change.role {
                p.role = v.clone();
            }
            if let Some(v) = &change.egress {
                p.egress = v.clone();
            }
            if let Some(v) = &change.framework {
                p.framework = v.clone();
            }
            if let Some(v) = change.auto_assign_domains {
                // On is the default: kept as unset, so files stay as they were.
                p.auto_assign_domains = v.filter(|on| !on);
            }
            if let Some(v) = &change.regions {
                // By AWS's names, each once; the default (the home region alone) as unset.
                p.regions = match v {
                    Some(names) => crate::regions::parse(names)?,
                    None => Vec::new(),
                };
            }
            Ok(p)
        })
        .await?;
    if let Some(max_inflight) = change.max_inflight {
        let changed = follow_max_inflight(ctx, project, max_inflight).await?;
        log(&format!("{project}: {changed} deployments running concurrent compute take {}; the routers apply it within seconds", inflight_words(max_inflight)));
    }
    if !change.skew.is_empty() {
        crate::skew::update(ctx, project, &change.skew).await?;
    }
    if let Some(count) = change.keep_warm {
        crate::warm::keep_set(ctx, project, count).await?;
    }
    if let Some(on) = change.analytics {
        crate::web_analytics::set(ctx, project, on).await?;
    }
    Ok(p)
}

/// The project's cap on requests per worker on each of its deployments in the routing table that
/// runs concurrent compute (`Deployment::follow_max_inflight`): the routers replace a pool whose
/// entry changed, so requests in flight finish on its workers and new ones start on workers that
/// take the new number. How many changed.
async fn follow_max_inflight(ctx: &Ctx, project: &str, max_inflight: Option<u32>) -> Result<usize> {
    let mut changed = 0;
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            changed = routes.deployments.values_mut().filter(|d| d.project == project).map(|d| d.follow_max_inflight(max_inflight)).filter(|&c| c).count();
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    Ok(changed)
}

/// A cap on requests per worker, in words.
pub fn inflight_words(max_inflight: Option<u32>) -> String {
    match max_inflight {
        Some(n) => format!("up to {n} requests per worker"),
        None => "the router's default per worker (10)".to_owned(),
    }
}

/// What deleting a project took out of the install.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Deleted {
    pub project: String,
    /// Its deployments: their records, build logs and Lambda versions.
    pub deployments: usize,
    /// Builds under way, canceled first so that none publishes into the project once it's gone.
    pub canceled: Vec<String>,
    /// Its addresses the routers served: `<project>.<base>`, each deployment's, each branch's.
    pub hosts: usize,
    /// Its custom domains.
    pub domains: Vec<String>,
    /// The regions its function was deleted in.
    pub functions: Vec<String>,
    /// Env var values deleted (every target).
    pub env_vars: usize,
    /// Whether it had a schedule group.
    pub schedules: bool,
    /// Stored files deleted: packages (every region's bucket), sources, uploads, build caches and
    /// logs, page previews, and the versions of its redirects, routing rules and firewall.
    pub files: usize,
}

/// The folders of a project's own files in the home bucket. A deployment's static files are shared
/// by content between projects: `forkrail sweep` deletes those nothing uses.
fn own_prefixes(project: &str) -> Vec<String> {
    let mut prefixes: Vec<String> = ["packages", "sources", "uploads", "cache", "builds", "logs/builds"].iter().map(|kind| format!("{kind}/{project}/")).collect();
    prefixes.push(previews_prefix(project));
    prefixes.push(format!("state/routing/{project}/"));
    prefixes
}

/// Take the project out of the routing table: every host that serves one of `ids` (its deployments,
/// production's and branches' addresses with them), those deployments, its custom domains and its
/// own section. Returns how many hosts went and the domains.
fn drop_from_routes(routes: &mut Routes, project: &str, ids: &BTreeSet<String>) -> (usize, Vec<String>) {
    let function = function_name(project);
    let mut ids = ids.clone();
    ids.extend(routes.deployments.iter().filter(|(_, d)| d.project == project || d.function == function).map(|(id, _)| id.clone()));
    let domains: Vec<String> = routes.domains.iter().filter(|(_, d)| d.project == project).map(|(name, _)| name.clone()).collect();
    let before = routes.hosts.len();
    routes.hosts.retain(|host, id| !ids.contains(id) && !domains.contains(host));
    let hosts = before - routes.hosts.len();
    routes.deployments.retain(|id, _| !ids.contains(id));
    routes.domains.retain(|_, d| d.project != project);
    routes.projects.remove(project);
    (hosts, domains)
}

/// Delete `project`: it stops being served at once, then its schedules, env vars, functions in
/// every region, deployments and files go, and its project file last. Every step can run again, so
/// one that fails is finished by deleting again. The dashboard's own project is refused.
pub async fn delete(ctx: &Ctx, project: &str) -> Result<Deleted> {
    check_project(project)?;
    if project == DASHBOARD_PROJECT {
        bail!("{project} is the dashboard itself: it can't be deleted from here");
    }
    let p = project_record(ctx, project).await?;
    let mut out = Deleted { project: project.to_owned(), ..Default::default() };
    // Builds in the builder are stopped; once their records are gone, the builder's result is refused.
    for d in p.deployments.iter().filter(|d| d.status.at(&d.created, state::now_secs()).in_progress()) {
        match remote::cancel(ctx, &d.id).await {
            Ok(_) => out.canceled.push(d.id.clone()),
            Err(e) => warn(&format!("{}: {e:#}", d.id)),
        }
    }
    // A publish under way finishes first, so the project is read again once it has: one that waits
    // for this finds no project and publishes nothing (publish.rs).
    let lock = Lock::acquire(ctx, project, DELETE_HOLDER).await?;
    let done = async {
        let p = project_record(ctx, project).await?;
        // Its deleted deployments too, still stored until their time is up (retention.rs).
        let ids: Vec<String> = p.deployments.iter().map(|d| d.id.clone()).chain(p.deleted.iter().map(|t| t.id.clone())).collect();
        let records = storage::records(ctx, &ids).await?;
        // Every region the function may be in: the install's, the project's, and wherever a
        // deployment was copied.
        let mut regions: BTreeSet<String> = ctx.store.settings().await?.regions.into_iter().collect();
        regions.insert(ctx.install.home_region.clone());
        regions.extend(p.regions.iter().cloned());
        for r in &records {
            regions.extend(r.region.iter().cloned());
            regions.extend(r.replicas.keys().cloned());
        }
        // A function that isn't this project's is refused before anything changes.
        let function = function_name(project);
        for region in &regions {
            ctx.lambda_in(region).check(&function, project).await.with_context(|| format!("in {region}"))?;
        }
        out.deployments = ids.len();
        remove_everything(ctx, project, &ids, &records, &regions, &mut out).await
    }
    .await;
    lock.release(ctx).await;
    done?;
    log(&format!(
        "deleted {project}: {} deployment{}, {} domain{}, its function in {}, {} env var value{}, {} stored file{}",
        out.deployments,
        plural(out.deployments),
        out.domains.len(),
        plural(out.domains.len()),
        if out.functions.is_empty() { "no region".to_owned() } else { out.functions.join(", ") },
        out.env_vars,
        plural(out.env_vars),
        out.files,
        plural(out.files),
    ));
    Ok(out)
}

/// What the dashboard keeps of a deleted project in its own table: its protection password, bypass
/// names and share links (`protection#<project>`). The dashboard deletes them itself when it deletes
/// a project; a delete from the CLI with the install's credentials does it here, so a new project
/// of the same name never finds them. Returns how many went; none without a dashboard table.
pub async fn forget_dashboard_items(ctx: &Ctx, project: &str) -> Result<usize> {
    use aws_sdk_dynamodb::types::AttributeValue;
    let db = aws_sdk_dynamodb::Client::new(&ctx.config);
    let pk = format!("protection#{project}");
    let mut keys = Vec::new();
    let mut pages = db
        .query()
        .table_name(WEB_TABLE)
        .key_condition_expression("pk = :pk")
        .expression_attribute_values(":pk", AttributeValue::S(pk.clone()))
        .projection_expression("sk")
        .into_paginator()
        .send();
    while let Some(page) = pages.next().await {
        let page = match page {
            Ok(page) => page,
            Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_not_found_exception()) => return Ok(0),
            Err(e) => bail!("reading {WEB_TABLE}: {}", aws_sdk_dynamodb::error::DisplayErrorContext(&e)),
        };
        keys.extend(page.items().iter().filter_map(|i| i.get("sk").and_then(|v| v.as_s().ok()).cloned()));
    }
    for sk in &keys {
        db.delete_item()
            .table_name(WEB_TABLE)
            .key("pk", AttributeValue::S(pk.clone()))
            .key("sk", AttributeValue::S(sk.clone()))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("deleting {pk} {sk} from {WEB_TABLE}: {}", aws_sdk_dynamodb::error::DisplayErrorContext(&e)))?;
    }
    Ok(keys.len())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

async fn remove_everything(
    ctx: &Ctx,
    project: &str,
    ids: &[String],
    records: &[state::DeploymentRecord],
    regions: &BTreeSet<String>,
    out: &mut Deleted,
) -> Result<()> {
    let wanted: BTreeSet<String> = ids.iter().cloned().collect();
    let mut dropped = (0, Vec::new());
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            dropped = drop_from_routes(&mut routes, project, &wanted);
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    (out.hosts, out.domains) = dropped;
    out.schedules = schedules::delete_group(ctx, project).await?;
    out.env_vars = Secrets::new(ctx).delete_project(project).await?;
    let function = function_name(project);
    for region in regions {
        if ctx.lambda_in(region).delete_function(&function, project).await.with_context(|| format!("in {region}"))? {
            out.functions.push(region.clone());
        }
    }
    for id in ids {
        ctx.store.delete(&state::deployment_key(id)).await?;
        ctx.store.delete(&cancel_key(id)).await?;
    }
    for key in records.iter().filter_map(|r| r.log.as_ref().and_then(|l| l.kept.clone())) {
        ctx.store.delete(&key).await?;
    }
    for prefix in own_prefixes(project) {
        for key in ctx.store.bucket_list(&prefix).await? {
            ctx.store.delete(&key).await?;
            out.files += 1;
        }
    }
    for key in [format!("state/firewall/{project}.json")] {
        if ctx.store.bucket_list(&key).await?.contains(&key) {
            ctx.store.delete(&key).await?;
            out.files += 1;
        }
    }
    // Packages in the other regions' buckets.
    let prefix = format!("packages/{project}/");
    for region in regions.iter().filter(|r| **r != ctx.install.home_region) {
        let bucket = ctx.install.code_bucket(region);
        let s3 = ctx.s3_in(region);
        let mut pages = s3.list_objects_v2().bucket(&bucket).prefix(&prefix).into_paginator().send();
        let mut keys = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("listing s3://{bucket}/{prefix}: {}", s3_error(&e)))?;
            keys.extend(page.contents().iter().filter_map(|o| o.key().map(str::to_owned)));
        }
        for key in keys {
            s3.delete_object().bucket(&bucket).key(&key).send().await.map_err(|e| anyhow::anyhow!("deleting s3://{bucket}/{key}: {}", s3_error(&e)))?;
            out.files += 1;
        }
    }
    // Its Next.js deployments' cache seeds (`nextseed.rs`), in every region.
    out.files += crate::nextseed::remove(ctx, &ids, &regions.iter().cloned().collect::<Vec<_>>()).await;
    ctx.store.delete(&state::project_key(project)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks() {
        assert!(Change { memory_mb: Some(Some(768)), max_duration_s: Some(Some(300)), ..Default::default() }.check().is_ok());
        assert!(Change { memory_mb: Some(None), ..Default::default() }.check().is_ok());
        assert!(Change { memory_mb: Some(Some(64)), ..Default::default() }.check().is_err());
        assert!(Change { memory_mb: Some(Some(10_241)), ..Default::default() }.check().is_err());
        // Function regions: known ones by either name, or back to the default.
        let regions = |r: Option<&[&str]>| Change { regions: Some(r.map(|r| r.iter().map(|s| s.to_string()).collect())), ..Default::default() };
        assert!(regions(Some(&["iad1", "eu-central-1"])).check().is_ok());
        assert!(regions(None).check().is_ok());
        assert!(regions(Some(&["moon1"])).check().is_err());
        assert!(regions(Some(&[])).check().is_err());
        assert!(!regions(None).project_file_unchanged());
        // Requests per worker: 2 to 100 (1 is concurrent compute off), or back to the default.
        assert!(Change { max_inflight: Some(Some(2)), ..Default::default() }.check().is_ok());
        assert!(Change { max_inflight: Some(Some(50)), ..Default::default() }.check().is_ok());
        assert!(Change { max_inflight: Some(None), ..Default::default() }.check().is_ok());
        assert!(Change { max_inflight: Some(Some(1)), ..Default::default() }.check().is_err());
        assert!(Change { max_inflight: Some(Some(51)), ..Default::default() }.check().is_err());
        assert!(Change { max_inflight: Some(Some(3)), ..Default::default() }.only_max_inflight());
        assert!(!Change { max_inflight: Some(Some(3)), memory_mb: Some(None), ..Default::default() }.only_max_inflight());
        assert!(!Change { max_inflight: Some(Some(3)), ..Default::default() }.project_file_unchanged());
        assert!(!Change { memory_mb: Some(None), ..Default::default() }.only_max_inflight());
        assert!(Change { max_duration_s: Some(Some(0)), ..Default::default() }.check().is_err());
        assert!(Change { max_duration_s: Some(Some(901)), ..Default::default() }.check().is_err());
        assert!(Change { role: Some(Some("forkrail-dashboard".into())), ..Default::default() }.check().is_ok());
        assert!(Change { role: Some(Some("no spaces".into())), ..Default::default() }.check().is_err());
        for internal in ["forkrail-notify", "arn:aws:iam::123456789012:role/forkrail-builder", "forkrail-image-us-east-1"] {
            assert!(Change { role: Some(Some(internal.into())), ..Default::default() }.check().is_err(), "{internal}");
        }
        assert!(Change { egress: Some(vec!["Analytics".into()]), ..Default::default() }.check().is_err());
        assert!(Change::default().is_empty());
    }

    #[test]
    fn a_deleted_project_leaves_nothing_in_the_routing_table() {
        let (sha_a, sha_d) = ("a".repeat(64), "d".repeat(64));
        let routes: Routes = serde_json::from_value(serde_json::json!({
            "deployments": {
                "aaaa1111": {"project": "shop", "function": "forkrail-app-shop", "qualifier": "3"},
                "bbbb2222": {"project": "shop", "function": "forkrail-app-shop", "qualifier": "4"},
                "cccc3333": {"project": "shop-admin", "function": "forkrail-app-shop-admin", "qualifier": "1"}
            },
            "hosts": {
                "shop.forkrail.com": "bbbb2222",
                "shop-aaaa1111.forkrail.com": "aaaa1111",
                "shop-bbbb2222.forkrail.com": "bbbb2222",
                "shop-git-feature.forkrail.com": "aaaa1111",
                "shop.example.com": "bbbb2222",
                "shop-admin.forkrail.com": "cccc3333"
            },
            "domains": {
                "shop.example.com": {"project": "shop"},
                "www.shop.example.com": {"project": "shop", "redirect": {"to": "shop.example.com", "status": 308}},
                "admin.example.com": {"project": "shop-admin"}
            },
            "projects": {"shop": {"routing": sha_a}, "shop-admin": {"routing": sha_d}}
        }))
        .unwrap();
        let mut after = routes.clone();
        // Not every deployment is in the project's list: the table's own entries for its function count too.
        let ids: BTreeSet<String> = ["aaaa1111".to_owned()].into();
        let (hosts, domains) = drop_from_routes(&mut after, "shop", &ids);
        assert_eq!(hosts, 5);
        assert_eq!(domains, vec!["shop.example.com".to_owned(), "www.shop.example.com".to_owned()]);
        assert_eq!(after.hosts.keys().collect::<Vec<_>>(), vec!["shop-admin.forkrail.com"]);
        assert_eq!(after.deployments.keys().collect::<Vec<_>>(), vec!["cccc3333"]);
        assert_eq!(after.domains.keys().collect::<Vec<_>>(), vec!["admin.example.com"]);
        assert_eq!(after.projects.keys().collect::<Vec<_>>(), vec!["shop-admin"]);
        after.validate().unwrap();
    }

    #[test]
    fn own_files_are_in_folders_of_their_own() {
        // `shop/` never matches `shop-admin/`'s files.
        assert!(own_prefixes("shop").iter().all(|p| p.ends_with("shop/")));
    }
}
