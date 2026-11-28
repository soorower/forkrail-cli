//! What deployments leave in the install's buckets, and when it goes.
//!
//! A deployment has three files of its own kind, each named by its content's hash, so deployments
//! of the same content share one: its package (`packages/<project>/<sha>.zip`, copied to every
//! region's bucket it's published in), the source the dashboard shows
//! (`sources/<project>/<sha>.zip`) and, for one built in the builder, the source a rebuild starts
//! from (`uploads/<project>/<sha>.zip`). A file goes with the last deployment that uses it
//! (`release`, called when deployments are removed). Lambda has its own copy of every published
//! version's code, so nothing that runs reads these.
//!
//! `forkrail sweep` finds what earlier forkrails left behind (they deleted deployments and kept
//! their files) and deletes it when told to.
//!
//! Static files (`static/` in every region's bucket) are named by content alone and shared between
//! projects and deployments: a file, a manifest, a routing program, a table of bulk redirects or
//! a firewall configuration goes only when nothing names it any more. `forkrail sweep` (of the whole
//! install) works that out from every kept deployment, the routing table and every kept version of
//! projects' redirects, rules and firewalls, and leaves `static/` alone while a deployment is being
//! made: a deploy skips uploading files the bucket already has.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use forkrail_common::install::{SWEEP_KEY, upload_key};
use serde::{Deserialize, Serialize};
use forkrail_common::routes::Routes;
use forkrail_common::rules::Program;
use forkrail_common::statics::{Manifest, manifest_key, rules_key};

use crate::deploy::{Ctx, check_project};
use crate::project_routing::Versions;
use crate::source;
use crate::state::{self, DeploymentRecord, Project, ROUTES_KEY, s3_error};

/// While a sweep deletes static files: until when (Unix seconds), so a sweep that died is forgotten.
#[derive(Debug, Serialize, Deserialize)]
struct Sweeping {
    until: u64,
}

/// How long a sweep's marker holds at most.
const SWEEP_MARKER_S: u64 = 2 * 60 * 60;

/// Whether a sweep may be deleting static files now: a deploy then uploads every static file of its
/// build rather than trust what the bucket has. One that can't tell assumes so.
pub(crate) async fn sweeping(ctx: &Ctx) -> bool {
    match ctx.store.get_value::<Sweeping>(SWEEP_KEY).await {
        Ok(Some(s)) => s.until > state::now_secs(),
        Ok(None) => false,
        Err(_) => true,
    }
}

/// The kinds of file a deployment keeps, by the folder each is under.
const KINDS: [&str; 3] = ["packages", "sources", "uploads"];
/// A file newer than this is left alone by `sweep`: a deploy under way uploads its source and its
/// package before its deployment is recorded as using them.
const SETTLED_AFTER_S: u64 = 60 * 60;
/// Records read at once.
const READS_AT_ONCE: usize = 16;

/// The files deployment `rec` uses, by key. Only keys under its own project's folders count: a
/// record can't name anything else for deletion.
pub fn files_of(rec: &DeploymentRecord) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    if rec.package_key.starts_with(&format!("packages/{}/", rec.project)) {
        keys.insert(rec.package_key.clone());
    }
    if let Some(kept) = &rec.kept_source {
        keys.insert(source::archive_key(&rec.project, &kept.sha256));
    }
    if let Some(upload) = &rec.upload {
        keys.insert(upload_key(&rec.project, &upload.sha256));
    }
    keys
}

/// What of `removed`'s files nothing in `kept` uses any more. A deployment still being made from
/// one of the removed (a redeploy publishes its package again, a rebuild reads its upload) keeps
/// that one's files.
pub fn unused(removed: &[DeploymentRecord], kept: &[DeploymentRecord]) -> BTreeSet<String> {
    let mut in_use: BTreeSet<String> = kept.iter().flat_map(files_of).collect();
    for making in kept.iter().filter(|r| r.status.in_progress()) {
        if let Some(from) = removed.iter().find(|r| Some(&r.id) == making.redeploy_of.as_ref()) {
            in_use.extend(files_of(from));
        }
    }
    removed.iter().flat_map(files_of).filter(|key| !in_use.contains(key)).collect()
}

/// The records of deployments `ids`, read a few at a time. One that's gone is left out; one that
/// can't be read is an error, since what it uses is then unknown.
pub async fn records(ctx: &Ctx, ids: &[String]) -> Result<Vec<DeploymentRecord>> {
    let mut found = Vec::new();
    for batch in ids.chunks(READS_AT_ONCE) {
        let mut reads = tokio::task::JoinSet::new();
        for id in batch {
            let store = ctx.store.clone();
            let key = state::deployment_key(id);
            reads.spawn(async move { store.get_value::<DeploymentRecord>(&key).await });
        }
        while let Some(read) = reads.join_next().await {
            found.extend(read.context("reading a deployment's record stopped")??);
        }
    }
    Ok(found)
}

/// Deployments `removed` of `project` are gone: delete the files only they used, in every region's
/// bucket. `kept` are the ids of the project's deployments that remain. Returns how many files
/// went. Nothing here fails what called it: a file that can't be deleted stays, and `forkrail
/// sweep` finds it later.
pub async fn release(ctx: &Ctx, project: &str, removed: &[DeploymentRecord], kept: &[String]) -> usize {
    // A Next.js deployment's cache seed (`nextseed.rs`) is its own: gone with it, in every region.
    let seeds = match ctx.store.settings().await {
        Ok(settings) => {
            let ids: Vec<String> = removed.iter().map(|r| r.id.clone()).collect();
            crate::nextseed::remove(ctx, &ids, &settings.regions).await
        }
        Err(_) => 0,
    };
    if removed.iter().all(|r| files_of(r).is_empty()) {
        return seeds;
    }
    let kept = match records(ctx, kept).await {
        Ok(kept) => kept,
        Err(e) => {
            eprintln!("warning: {project}: the removed deployments' files are left in place: {e:#}");
            return seeds;
        }
    };
    let mut regions = match ctx.store.settings().await {
        Ok(settings) => settings.regions,
        Err(e) => {
            eprintln!("warning: {project}: the removed deployments' files are left in place: {e:#}");
            return seeds;
        }
    };
    // Packages were staged wherever the removed deployments' functions ran too.
    add_function_regions(&mut regions, removed);
    let mut deleted = 0;
    for key in unused(removed, &kept) {
        match delete_everywhere(ctx, &key, &regions).await {
            Ok(()) => deleted += 1,
            Err(e) => eprintln!("warning: {e:#}"),
        }
    }
    deleted + seeds
}

/// Add to `regions` every region `records`' functions were published in.
fn add_function_regions(regions: &mut Vec<String>, records: &[DeploymentRecord]) {
    for region in records.iter().flat_map(|r| r.region.iter().chain(r.replicas.keys())) {
        if !regions.contains(region) {
            regions.push(region.clone());
        }
    }
}

/// Delete `key` from the home bucket, and a package from the other regions' buckets too.
async fn delete_everywhere(ctx: &Ctx, key: &str, regions: &[String]) -> Result<()> {
    ctx.store.delete(key).await?;
    if key.starts_with("packages/") {
        for region in regions.iter().filter(|r| **r != ctx.install.home_region) {
            let bucket = ctx.install.code_bucket(region);
            ctx.s3_in(region)
                .delete_object()
                .bucket(&bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("deleting s3://{bucket}/{key}: {}", s3_error(&e)))?;
        }
    }
    Ok(())
}

/// One stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Object {
    key: String,
    bytes: u64,
    /// When it was written, Unix seconds.
    modified: u64,
}

/// Every object under `prefix` in `bucket`.
async fn list(s3: &aws_sdk_s3::Client, bucket: &str, prefix: &str) -> Result<Vec<Object>> {
    let mut objects = Vec::new();
    let mut pages = s3.list_objects_v2().bucket(bucket).prefix(prefix).into_paginator().send();
    while let Some(page) = pages.next().await {
        let page = page.map_err(|e| anyhow::anyhow!("listing s3://{bucket}/{prefix}: {}", s3_error(&e)))?;
        objects.extend(page.contents().iter().filter_map(|o| {
            Some(Object {
                key: o.key()?.to_owned(),
                bytes: u64::try_from(o.size().unwrap_or(0)).unwrap_or(0),
                modified: o.last_modified().and_then(|t| u64::try_from(t.secs()).ok()).unwrap_or(u64::MAX),
            })
        }));
    }
    Ok(objects)
}

/// The project a stored file belongs to, from its key: `packages/<project>/<sha>.zip`.
fn project_of(key: &str) -> Option<&str> {
    let mut parts = key.split('/');
    let (kind, project, file) = (parts.next()?, parts.next()?, parts.next()?);
    (KINDS.contains(&kind) && !project.is_empty() && !file.is_empty() && parts.next().is_none()).then_some(project)
}

/// The objects of `stored` that no deployment uses (`in_use`, by project; a project not in it has
/// no deployments at all) and that have been there long enough not to belong to a deploy under way.
fn leftovers<'a>(stored: &'a [Object], in_use: &BTreeMap<String, BTreeSet<String>>, now: u64) -> Vec<&'a Object> {
    stored
        .iter()
        .filter(|o| o.modified.saturating_add(SETTLED_AFTER_S) < now)
        .filter(|o| project_of(&o.key).is_some_and(|project| !in_use.get(project).is_some_and(|used| used.contains(&o.key))))
        .collect()
}

/// `forkrail sweep [PROJECT] [--yes]`: list the packages, kept sources and uploads that no
/// deployment uses, in every region's bucket, and with `delete` remove them.
pub async fn sweep(ctx: &Ctx, only: Option<&str>, delete: bool) -> Result<()> {
    if let Some(project) = only {
        check_project(project)?;
    }
    // Deleting static files: from now on every deploy uploads all of its own (statics.rs), and those
    // under way before are seen below as being made, which leaves static/ alone.
    let marked = delete && only.is_none();
    if marked {
        ctx.store.put_bytes(SWEEP_KEY, serde_json::to_vec(&Sweeping { until: state::now_secs() + SWEEP_MARKER_S })?, "application/json").await?;
    }
    let swept = sweep_files(ctx, only, delete).await;
    if marked && let Err(e) = ctx.store.delete(SWEEP_KEY).await {
        eprintln!("warning: {SWEEP_KEY} is left (it expires within two hours): {e:#}");
    }
    swept?;
    sweep_env_layers(ctx, only, delete).await
}

/// Sealed env files' layer versions that a publish failed to delete (lambda.rs), in every region.
async fn sweep_env_layers(ctx: &Ctx, only: Option<&str>, delete: bool) -> Result<()> {
    let settings = ctx.store.settings().await?;
    let home = ctx.install.home_region.clone();
    let mut regions: Vec<String> = std::iter::once(home.clone()).chain(settings.regions.iter().filter(|r| **r != home).cloned()).collect();
    // And every region a project picks for its functions.
    for project in crate::project::all(ctx).await? {
        for region in project.regions {
            if !regions.contains(&region) {
                regions.push(region);
            }
        }
    }
    for region in regions {
        let lambda = ctx.lambda_in(&region);
        let leftovers = lambda.env_layer_leftovers(only).await.with_context(|| format!("env layers in {region}"))?;
        for (layer, version) in &leftovers {
            if delete {
                lambda.delete_layer_version(layer, *version).await?;
            }
            println!("{region}  env layer {layer}:{version}{}", if delete { "  deleted" } else { "" });
        }
    }
    Ok(())
}

async fn sweep_files(ctx: &Ctx, only: Option<&str>, delete: bool) -> Result<()> {
    let settings = ctx.store.settings().await?;
    let home = ctx.install.home_region.clone();
    let mut regions: Vec<String> = std::iter::once(home.clone()).chain(settings.regions.iter().filter(|r| **r != home).cloned()).collect();

    // What every project's deployments use. A project whose records can't all be read is left
    // alone entirely.
    let mut in_use: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut skipped = BTreeSet::new();
    let mut known = BTreeSet::new();
    // Every kept deployment's record, and whether one is being made: for the static files.
    let mut all_records: Vec<DeploymentRecord> = Vec::new();
    let mut making = Vec::new();
    for key in ctx.store.list("state/projects/").await? {
        let Some(name) = key.strip_prefix("state/projects/").and_then(|k| k.strip_suffix(".json")) else { continue };
        if only.is_some_and(|o| o != name) {
            continue;
        }
        known.insert(name.to_owned());
        let used = async {
            let project: Project = ctx.store.get_value(&key).await?.with_context(|| format!("{key} is gone"))?;
            // Deleted deployments keep everything until they're purged, so they can be restored.
            let ids: Vec<String> = project.deployments.iter().map(|d| d.id.clone()).chain(project.deleted.iter().map(|t| t.id.clone())).collect();
            let found = records(ctx, &ids).await?;
            if found.len() != ids.len() {
                bail!("{} of its {} deployments have no record", ids.len() - found.len(), ids.len());
            }
            let now = state::now_secs();
            let under_way: Vec<String> = project.deployments.iter().filter(|d| d.status.at(&d.created, now).in_progress()).map(|d| d.id.clone()).collect();
            Ok((found.iter().flat_map(files_of).collect::<BTreeSet<String>>(), found, under_way))
        };
        match used.await {
            Ok((used, found, under_way)) => {
                in_use.insert(name.to_owned(), used);
                all_records.extend(found);
                making.extend(under_way);
            }
            Err(e) => {
                eprintln!("warning: {name} is left alone: {e:#}");
                skipped.insert(name.to_owned());
            }
        }
    }
    if let Some(project) = only
        && !known.contains(project)
    {
        // A project that's gone: everything under its name is left over.
        eprintln!("note: there is no project {project}: every file under its name is left over");
    }

    // The buckets of regions with functions and no router hold packages too.
    add_function_regions(&mut regions, &all_records);
    let now = state::now_secs();
    let (mut files, mut bytes) = (0u64, 0u64);
    for region in &regions {
        let (s3, bucket) = (ctx.s3_in(region), ctx.install.code_bucket(region));
        // Only packages are copied to the other regions.
        let kinds: &[&str] = if *region == home { &KINDS } else { &KINDS[..1] };
        let mut stored = Vec::new();
        for kind in kinds {
            let prefix = match only {
                Some(project) => format!("{kind}/{project}/"),
                None => format!("{kind}/"),
            };
            stored.extend(list(&s3, &bucket, &prefix).await?);
        }
        stored.retain(|o| project_of(&o.key).is_some_and(|p| !skipped.contains(p)));
        let left = leftovers(&stored, &in_use, now);
        // By project and kind, for the report.
        let mut groups: BTreeMap<(String, &str), (u64, u64)> = BTreeMap::new();
        for object in &left {
            let project = project_of(&object.key).unwrap_or_default().to_owned();
            let kind = KINDS.iter().find(|k| object.key.starts_with(&format!("{k}/"))).copied().unwrap_or("?");
            let group = groups.entry((project, kind)).or_default();
            (group.0, group.1) = (group.0 + 1, group.1 + object.bytes);
        }
        let total: u64 = stored.iter().map(|o| o.bytes).sum();
        let left_bytes: u64 = left.iter().map(|o| o.bytes).sum();
        println!("{region}  s3://{bucket}: {} of {} files left over, {} of {}", left.len(), stored.len(), size(left_bytes), size(total));
        for ((project, kind), (count, sum)) in &groups {
            let gone = if known.contains(project) || only.is_some() { "" } else { "  (no such project any more)" };
            println!("  {project:<24} {kind:<9} {count:>5} files  {:>10}{gone}", size(*sum));
        }
        files += left.len() as u64;
        bytes += left_bytes;
        if delete {
            for object in &left {
                s3.delete_object().bucket(&bucket).key(&object.key).send().await.map_err(|e| anyhow::anyhow!("deleting s3://{bucket}/{}: {}", object.key, s3_error(&e)))?;
            }
        }
    }
    // Static files, shared by every project: only in a sweep of the whole install, and only when
    // everything that may name one could be read.
    if only.is_none() {
        let why_not = if !skipped.is_empty() {
            Some(format!("{} couldn't be read", skipped.iter().cloned().collect::<Vec<_>>().join(", ")))
        } else if !making.is_empty() {
            Some(format!("deployment{} {} {} being made", if making.len() == 1 { "" } else { "s" }, making.join(", "), if making.len() == 1 { "is" } else { "are" }))
        } else {
            None
        };
        match why_not {
            Some(why) => println!("static/ is left alone: {why}. Sweep again when it isn't."),
            None => {
                let used = static_in_use(ctx, &all_records).await?;
                for region in &regions {
                    let (s3, bucket) = (ctx.s3_in(region), ctx.install.code_bucket(region));
                    let stored = list(&s3, &bucket, "static/").await?;
                    let left = static_leftovers(&stored, &used, now);
                    let total: u64 = stored.iter().map(|o| o.bytes).sum();
                    let left_bytes: u64 = left.iter().map(|o| o.bytes).sum();
                    println!("{region}  s3://{bucket}/static/: {} of {} files left over, {} of {}", left.len(), stored.len(), size(left_bytes), size(total));
                    let mut kinds: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
                    for object in &left {
                        let kind = static_kind(&object.key).map_or("other", |(kind, _)| kind.name());
                        let k = kinds.entry(kind).or_default();
                        (k.0, k.1) = (k.0 + 1, k.1 + object.bytes);
                    }
                    for (kind, (count, sum)) in &kinds {
                        println!("  {kind:<24} {count:>5} files  {:>10}", size(*sum));
                    }
                    files += left.len() as u64;
                    bytes += left_bytes;
                    if delete {
                        for object in &left {
                            s3.delete_object().bucket(&bucket).key(&object.key).send().await.map_err(|e| anyhow::anyhow!("deleting s3://{bucket}/{}: {}", object.key, s3_error(&e)))?;
                        }
                    }
                }
            }
        }
    }
    match (files, delete) {
        (0, _) => println!("Nothing is left over: every stored file is some deployment's."),
        (_, true) => println!("Deleted {files} files, {}.", size(bytes)),
        (_, false) => println!("{files} files, {}, belong to no deployment. `forkrail sweep --yes` deletes them; files under an hour old are never counted.", size(bytes)),
    }
    Ok(())
}

/// What under `static/` is a kind of: its id (a hex SHA-256).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StaticKind {
    /// `static/<sha>`, and its `.br` and `.gz` copies.
    File,
    Manifest,
    Rules,
    Redirects,
    Firewall,
}

impl StaticKind {
    fn name(self) -> &'static str {
        match self {
            StaticKind::File => "files",
            StaticKind::Manifest => "manifests",
            StaticKind::Rules => "routing programs",
            StaticKind::Redirects => "bulk redirects",
            StaticKind::Firewall => "firewall configurations",
        }
    }
}

fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The kind and id of a key under `static/`; None for anything else, which a sweep never touches.
fn static_kind(key: &str) -> Option<(StaticKind, &str)> {
    let rest = key.strip_prefix("static/")?;
    let (kind, id) = if let Some(id) = rest.strip_prefix("manifests/").and_then(|r| r.strip_suffix(".json")) {
        (StaticKind::Manifest, id)
    } else if let Some(id) = rest.strip_prefix("rules/").and_then(|r| r.strip_suffix(".json")) {
        (StaticKind::Rules, id)
    } else if let Some(id) = rest.strip_prefix("firewall/").and_then(|r| r.strip_suffix(".json")) {
        (StaticKind::Firewall, id)
    } else if let Some(id) = rest.strip_prefix("redirects/") {
        (StaticKind::Redirects, id)
    } else {
        (StaticKind::File, rest.strip_suffix(".br").or_else(|| rest.strip_suffix(".gz")).unwrap_or(rest))
    };
    is_sha(id).then_some((kind, id))
}

/// Everything under `static/` that something names, by kind and id.
type StaticUse = BTreeSet<(StaticKind, String)>;

/// What under `static/` is in use: kept deployments' manifests and programs (and the bulk
/// redirects a program names), the routing table's, the firewalls', every kept version of a
/// project's redirects, rules and firewall, and every file a manifest in use lists. A manifest or
/// program that something names but can't be read stops the sweep: what it uses is then unknown.
async fn static_in_use(ctx: &Ctx, records: &[DeploymentRecord]) -> Result<StaticUse> {
    let mut used = StaticUse::new();
    let mut manifests: BTreeSet<String> = BTreeSet::new();
    let mut programs: BTreeSet<String> = BTreeSet::new();
    for r in records {
        manifests.extend(r.static_files.clone());
        programs.extend(r.rules.clone());
    }
    let routes: Routes = ctx.store.get_value(ROUTES_KEY).await?.unwrap_or_default();
    for d in routes.deployments.values() {
        manifests.extend(d.static_files.clone());
        programs.extend(d.rules.clone());
    }
    used.extend(routes.firewall.iter().map(|id| (StaticKind::Firewall, id.clone())));
    for p in routes.projects.values() {
        used.extend(p.redirects.iter().map(|id| (StaticKind::Redirects, id.clone())));
        programs.extend(p.routing.clone());
        used.extend(p.firewall.iter().map(|id| (StaticKind::Firewall, id.clone())));
    }
    // Kept versions, which can be published again.
    // `state/routing/<project>/redirects.json` and `routes.json`; the rules as written beside them
    // (`rules/<sha>.json`) aren't under static/.
    let version_files = ctx.store.bucket_list("state/routing/").await?.into_iter().filter(|k| k.matches('/').count() == 3 && (k.ends_with("/redirects.json") || k.ends_with("/routes.json")));
    for key in version_files {
        let Some(versions) = ctx.store.get_value::<Versions>(&key).await? else { continue };
        for v in versions.versions {
            if key.ends_with("/redirects.json") {
                used.insert((StaticKind::Redirects, v.object));
            } else if key.ends_with("/routes.json") {
                programs.extend(v.program);
            }
        }
    }
    for key in ctx.store.bucket_list("state/firewall/").await?.into_iter().filter(|k| k.ends_with(".json")) {
        let Some(versions) = ctx.store.get_value::<Versions>(&key).await? else { continue };
        used.extend(versions.versions.into_iter().map(|v| (StaticKind::Firewall, v.object)));
    }
    // What was written in the last hour belongs to a deploy or a change under way.
    let now = state::now_secs();
    let recent = list(&ctx.store.s3, &ctx.store.bucket, "static/").await?;
    for o in recent.iter().filter(|o| o.modified.saturating_add(SETTLED_AFTER_S) >= now) {
        match static_kind(&o.key) {
            Some((StaticKind::Manifest, id)) => {
                manifests.insert(id.to_owned());
            }
            Some((StaticKind::Rules, id)) => {
                programs.insert(id.to_owned());
            }
            _ => {}
        }
    }
    for id in &programs {
        let bytes = ctx.store.get_bytes(&rules_key(id)).await?.with_context(|| format!("routing program {id} is named but missing: nothing under static/ is swept"))?;
        let program: Program = serde_json::from_slice(&bytes).with_context(|| format!("reading routing program {id}"))?;
        used.extend(program.bulk.map(|b| (StaticKind::Redirects, b)));
        used.insert((StaticKind::Rules, id.clone()));
    }
    for id in &manifests {
        let bytes = ctx.store.get_bytes(&manifest_key(id)).await?.with_context(|| format!("manifest {id} is named but missing: nothing under static/ is swept"))?;
        let manifest: Manifest = serde_json::from_slice(&bytes).with_context(|| format!("reading manifest {id}"))?;
        used.extend(manifest.files.into_values().map(|f| (StaticKind::File, f.sha256)));
        used.insert((StaticKind::Manifest, id.clone()));
    }
    Ok(used)
}

/// The objects under `static/` that nothing uses and that have been there long enough not to
/// belong to a deploy under way. Anything that isn't one of forkrail's kinds is left alone.
fn static_leftovers<'a>(stored: &'a [Object], used: &StaticUse, now: u64) -> Vec<&'a Object> {
    stored
        .iter()
        .filter(|o| o.modified.saturating_add(SETTLED_AFTER_S) < now)
        .filter(|o| static_kind(&o.key).is_some_and(|(kind, id)| !used.contains(&(kind, id.to_owned()))))
        .collect()
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.2} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{:.0} KB", b as f64 / 1024.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Archive;
    use crate::state::Status;

    fn record(id: &str, package: &str, source: Option<&str>, upload: Option<&str>) -> DeploymentRecord {
        let archive = |sha: &str| Archive { sha256: sha.to_owned(), files: 1, bytes: 1 };
        serde_json::from_value::<DeploymentRecord>(serde_json::json!({"id": id, "project": "shop", "target": "preview", "created": "2026-11-12T00:00:00Z"}))
            .map(|r| DeploymentRecord {
                package_key: if package.is_empty() { String::new() } else { format!("packages/shop/{package}.zip") },
                kept_source: source.map(archive),
                upload: upload.map(archive),
                ..r
            })
            .unwrap()
    }

    #[test]
    fn a_file_goes_with_the_last_deployment_that_uses_it() {
        let old = record("aaaa1111", "p1", Some("s1"), Some("u1"));
        let older = record("bbbb2222", "p0", Some("s0"), None);
        // A redeploy of `old` shares its package and source; another deployment shares its upload.
        let again = record("cccc3333", "p1", Some("s1"), None);
        let rebuilt = record("dddd4444", "p2", Some("s2"), Some("u1"));
        let keys = |set: BTreeSet<String>| set.into_iter().collect::<Vec<_>>();

        assert_eq!(keys(unused(&[old.clone(), older.clone()], &[again.clone(), rebuilt.clone()])), ["packages/shop/p0.zip", "sources/shop/s0.zip"]);
        assert_eq!(keys(unused(&[old.clone()], &[])), ["packages/shop/p1.zip", "sources/shop/s1.zip", "uploads/shop/u1.zip"]);
        assert!(unused(&[], &[old.clone()]).is_empty());

        // One still being made from a removed deployment has no package of its own yet: the
        // removed one's files stay for it.
        let mut making = record("eeee5555", "", None, None);
        (making.status, making.redeploy_of) = (Status::Building, Some("aaaa1111".into()));
        assert!(unused(&[old.clone()], &[making.clone()]).is_empty());
        making.status = Status::Error;
        assert_eq!(unused(&[old.clone()], &[making]).len(), 3);

        // A record can only name its own project's files.
        let mut odd = record("ffff6666", "p9", None, None);
        odd.package_key = "packages/other/p9.zip".into();
        assert!(files_of(&odd).is_empty());
        odd.package_key = "state/routes.json".into();
        assert!(files_of(&odd).is_empty());
    }

    #[test]
    fn a_sweep_takes_only_settled_files_nothing_uses() {
        let now = 1_800_000_000;
        let object = |key: &str, age: u64| Object { key: key.into(), bytes: 10, modified: now - age };
        let stored = [
            object("packages/shop/used.zip", 9_000),
            object("packages/shop/orphan.zip", 9_000),
            object("uploads/shop/just-uploaded.zip", 30),
            object("sources/gone/any.zip", 9_000),
            object("packages/shop/nested/x.zip", 9_000),
            object("cache/shop/production.tar.zst", 9_000),
            object("packages/stray.zip", 9_000),
        ];
        let in_use: BTreeMap<String, BTreeSet<String>> = [("shop".to_owned(), ["packages/shop/used.zip".to_owned()].into())].into();
        let left: Vec<&str> = leftovers(&stored, &in_use, now).iter().map(|o| o.key.as_str()).collect();
        assert_eq!(left, ["packages/shop/orphan.zip", "sources/gone/any.zip"]);
        assert_eq!(project_of("packages/shop/abc.zip"), Some("shop"));
        assert_eq!(project_of("static/abc"), None);

        // Static files: what nothing names, once settled; a file's compressed copies go with it.
        let (a, b, m) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        let stored = [
            object(&format!("static/{a}"), 9_000),
            object(&format!("static/{a}.br"), 9_000),
            object(&format!("static/{b}"), 9_000),
            object(&format!("static/{b}.gz"), 9_000),
            object(&format!("static/{b}.br"), 30),
            object(&format!("static/manifests/{m}.json"), 9_000),
            object(&format!("static/firewall/{m}.json"), 9_000),
            object("static/notes.txt", 9_000),
            object(&format!("static/rules/{a}.json"), 9_000),
        ];
        let used: StaticUse = [(StaticKind::File, a.clone()), (StaticKind::Manifest, m.clone()), (StaticKind::Rules, a.clone())].into();
        let left: Vec<String> = static_leftovers(&stored, &used, now).iter().map(|o| o.key.clone()).collect();
        assert_eq!(left, [format!("static/{b}"), format!("static/{b}.gz"), format!("static/firewall/{m}.json")]);
        assert_eq!(static_kind(&format!("static/redirects/{a}")), Some((StaticKind::Redirects, a.as_str())));
        assert_eq!(static_kind("static/manifests/short.json"), None);
        assert_eq!((size(512 << 10), size(3 << 20), size(5 << 30)), ("512 KB".to_owned(), "3.0 MB".to_owned(), "5.00 GB".to_owned()));
    }
}
