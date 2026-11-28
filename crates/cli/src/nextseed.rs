//! The seed of Next.js's shared cache (N2; the design is in the router's `cache/next.rs`): the
//! build's prerenders, written as entries of the router's store and kept among the deployment's
//! static files, so they needn't ship in the function package and a new deployment's pages are
//! served from the first request with the build's time as their age.
//!
//! After `next build`, `make` runs `next-seed.mjs` in the folder Next's server runs from. It
//! reads every prerender through that Next's own file-system cache and writes `<out>/index.json`
//! and one file an entry. `upload` puts them under `static/next-seed/<deployment>/` in a region's
//! bucket (the index last, so a router never reads an index whose entries aren't there), and
//! `remove` deletes a deployment's when it's removed (`storage::release`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use aws_sdk_s3::primitives::ByteStream;
use forkrail_common::nextcache::{SEED_PREFIX, SeedIndex, seed_entry_key, seed_index_key};
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::deploy::Ctx;
use crate::state::s3_error;

/// The script, and the worker's handlers it writes entries with (so a seeded entry is byte for
/// byte what a worker would have written).
const SEED_JS: &str = include_str!("next-seed.mjs");
const HANDLERS_JS: &str = include_str!("../../runtime/src/next-cache.mjs");
/// Uploads at once.
const PARALLEL: usize = 32;

/// What `make` wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Seeded {
    pub entries: usize,
    pub bytes: u64,
    /// Prerenders Next's cache had nothing for, or over the largest page.
    pub skipped: usize,
    pub built_ms: u64,
}

/// Write the two scripts into `dir`; the path of the one to run.
pub fn write_tools(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("next-cache.mjs"), HANDLERS_JS)?;
    let script = dir.join("next-seed.mjs");
    std::fs::write(&script, SEED_JS)?;
    Ok(script)
}

/// Write the seed of the build in `app_dir` (where Next's `server.js` and its dist folder are)
/// into `out`, with `node`. `tools` is a folder for the scripts.
pub fn make(node: &Path, tools: &Path, app_dir: &Path, dist_dir: &str, out: &Path, deployment: &str) -> Result<Seeded> {
    // The script runs in `app_dir`: a relative path (`--out builds/x`) would be read from there.
    let (app_dir, out) = (std::path::absolute(app_dir)?, std::path::absolute(out)?);
    let (app_dir, out) = (app_dir.as_path(), out.as_path());
    let script = write_tools(&std::path::absolute(tools)?)?;
    // The script loads the project's own `next`: no AWS keys of this machine's (or the
    // builder's role) for its code, as for every other build step (`buildlog::run`).
    let mut command = std::process::Command::new(node);
    command.arg(&script).arg(app_dir).arg(out).arg(deployment).arg(dist_dir).current_dir(app_dir);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("AWS_") {
            command.env_remove(&key);
        }
    }
    let output = command
        .output()
        .with_context(|| format!("running {}", node.display()))?;
    if !output.status.success() {
        bail!("seeding Next.js's cache failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let line = output.stdout.split(|&b| b == b'\n').rfind(|l| !l.is_empty()).unwrap_or_default();
    serde_json::from_slice(line).with_context(|| format!("the seed script's summary: {}", String::from_utf8_lossy(line)))
}

/// Put the seed in `out` (what `make` wrote) in `region`'s bucket as `deployment`'s. Returns how
/// many objects were sent; none for a build without prerenders.
pub async fn upload(ctx: &Ctx, region: &str, out: &Path, deployment: &str) -> Result<usize> {
    let index_path = out.join("index.json");
    let index: SeedIndex = match std::fs::read(&index_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", index_path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("reading {}", index_path.display())),
    };
    if index.entries.is_empty() {
        return Ok(0);
    }
    let bucket = if region == ctx.install.home_region { ctx.store.bucket.clone() } else { crate::regions::ensure_code_bucket(ctx, region).await? };
    let s3 = ctx.s3_in(region);
    let slots = Arc::new(Semaphore::new(PARALLEL));
    let mut uploads = tokio::task::JoinSet::new();
    for entry in &index.entries {
        if !entry.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{entry:?} in {} isn't an entry's name", index_path.display());
        }
        let path = out.join(entry);
        let body = ByteStream::from_path(&path).await.with_context(|| format!("reading {}", path.display()))?;
        let (s3, bucket, key, slots) = (s3.clone(), bucket.clone(), seed_entry_key(deployment, entry), slots.clone());
        uploads.spawn(async move {
            let _slot = slots.acquire_owned().await.expect("never closed");
            put(&s3, &bucket, &key, "application/octet-stream", body).await
        });
    }
    while let Some(done) = uploads.join_next().await {
        done??;
    }
    let body = ByteStream::from_path(&index_path).await.with_context(|| format!("reading {}", index_path.display()))?;
    put(&s3, &bucket, &seed_index_key(deployment), "application/json", body).await?;
    Ok(index.entries.len() + 1)
}

/// Put deployment `from`'s seed in each of `regions` as deployment `to`'s: a deployment's build
/// published again under a new id (a redeploy). It's read from the home region's bucket, where
/// every deployment's is; each entry names its deployment and is named for it, so each is
/// written again for `to` (`rescope`). Returns how many objects were sent; none when `from` has no
/// seed (its pages are then made at their first request).
pub async fn carry(ctx: &Ctx, from: &str, to: &str, regions: &[&str]) -> Result<usize> {
    if !safe(from) || !safe(to) {
        bail!("{from:?} or {to:?} isn't a deployment's id");
    }
    let home = ctx.s3_in(&ctx.install.home_region);
    let home_bucket = ctx.store.bucket.clone();
    let Some(index_bytes) = get(&home, &home_bucket, &seed_index_key(from)).await? else { return Ok(0) };
    let index: SeedIndex = serde_json::from_slice(&index_bytes).with_context(|| format!("reading {}", seed_index_key(from)))?;
    if index.entries.is_empty() {
        return Ok(0);
    }
    let mut targets = Vec::new();
    for region in regions {
        let bucket = if *region == ctx.install.home_region { home_bucket.clone() } else { crate::regions::ensure_code_bucket(ctx, region).await? };
        targets.push((ctx.s3_in(region), bucket));
    }
    let targets = Arc::new(targets);
    let slots = Arc::new(Semaphore::new(PARALLEL));
    let mut copies = tokio::task::JoinSet::new();
    for entry in index.entries.clone() {
        if !entry.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{entry:?} in {} isn't an entry's name", seed_index_key(from));
        }
        let (home, home_bucket, targets, slots, from, to) = (home.clone(), home_bucket.clone(), targets.clone(), slots.clone(), from.to_owned(), to.to_owned());
        copies.spawn(async move {
            let _slot = slots.acquire_owned().await.expect("never closed");
            let Some(page) = get(&home, &home_bucket, &seed_entry_key(&from, &entry)).await? else { bail!("{} is in the seed's index but not in the bucket", seed_entry_key(&from, &entry)) };
            let page = rescope(&page, &to, &entry).with_context(|| format!("{} isn't a seed entry", seed_entry_key(&from, &entry)))?;
            for (s3, bucket) in targets.iter() {
                put(s3, bucket, &seed_entry_key(&to, &entry), "application/octet-stream", ByteStream::from(page.clone())).await?;
            }
            Ok(())
        });
    }
    while let Some(done) = copies.join_next().await {
        done??;
    }
    for (s3, bucket) in targets.iter() {
        put(s3, bucket, &seed_index_key(to), "application/json", ByteStream::from(index_bytes.clone())).await?;
    }
    Ok((index.entries.len() + 1) * targets.len())
}

/// An object's bytes, or `None` if there's no such object.
async fn get(s3: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Result<Option<Vec<u8>>> {
    match s3.get_object().bucket(bucket).key(key).send().await {
        Ok(object) => Ok(Some(object.body.collect().await.with_context(|| format!("reading s3://{bucket}/{key}"))?.into_bytes().to_vec())),
        Err(e) if e.as_service_error().is_some_and(|s| s.is_no_such_key()) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading s3://{bucket}/{key}: {}", s3_error(&e))),
    }
}

/// A seed entry (a page as the router keeps one: a format line, a line of JSON, headers, body)
/// written again as deployment `to`'s: its JSON's `deployment` and `hash` (its name in the store,
/// `nextcache::hash_input`), the rest as it was.
pub fn rescope(page: &[u8], to: &str, key_hash: &str) -> Option<Vec<u8>> {
    const MAGIC: &[u8] = b"forkrail-response 1\n";
    let rest = page.strip_prefix(MAGIC)?;
    let at = rest.iter().position(|&b| b == b'\n')?;
    let mut meta: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&rest[..at]).ok()?;
    let hash = forkrail_common::hex(&<sha2::Sha256 as sha2::Digest>::digest(forkrail_common::nextcache::hash_input(to, key_hash).as_bytes()));
    meta.insert("deployment".to_owned(), to.into());
    meta.insert("hash".to_owned(), hash.into());
    let mut out = Vec::with_capacity(page.len() + 64);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&serde_json::to_vec(&meta).ok()?);
    out.extend_from_slice(&rest[at..]);
    Some(out)
}

async fn put(s3: &aws_sdk_s3::Client, bucket: &str, key: &str, content_type: &str, body: ByteStream) -> Result<()> {
    s3.put_object().bucket(bucket).key(key).content_type(content_type).body(body).send().await.map_err(|e| anyhow::anyhow!("writing s3://{bucket}/{key}: {}", s3_error(&e)))?;
    Ok(())
}

/// Delete the seeds of `deployments` from every region's bucket. Returns how many objects went.
/// Nothing here fails what called it: what can't be deleted stays, and is said.
pub async fn remove(ctx: &Ctx, deployments: &[String], regions: &[String]) -> usize {
    let mut deleted = 0;
    let mut all: Vec<String> = regions.to_vec();
    if !all.contains(&ctx.install.home_region) {
        all.push(ctx.install.home_region.clone());
    }
    for region in &all {
        let bucket = if *region == ctx.install.home_region { ctx.store.bucket.clone() } else { ctx.install.code_bucket(region) };
        let s3 = ctx.s3_in(region);
        for deployment in deployments.iter().filter(|d| safe(d)) {
            let prefix = format!("{SEED_PREFIX}{deployment}/");
            let mut pages = s3.list_objects_v2().bucket(&bucket).prefix(&prefix).into_paginator().send();
            let mut keys: Vec<String> = Vec::new();
            while let Some(page) = pages.next().await {
                match page {
                    Ok(page) => keys.extend(page.contents().iter().filter_map(|o| o.key().map(str::to_owned))),
                    Err(e) => {
                        // A region without a bucket has no seeds.
                        if !e.as_service_error().is_some_and(|s| s.is_no_such_bucket()) {
                            eprintln!("warning: listing s3://{bucket}/{prefix}: {}", s3_error(&e));
                        }
                        break;
                    }
                }
            }
            for key in keys {
                match s3.delete_object().bucket(&bucket).key(&key).send().await {
                    Ok(_) => deleted += 1,
                    Err(e) => eprintln!("warning: deleting s3://{bucket}/{key}: {}", s3_error(&e)),
                }
            }
        }
    }
    deleted
}

/// A deployment's id as it may appear in a key: never a path of its own making.
fn safe(deployment: &str) -> bool {
    !deployment.is_empty() && deployment.len() <= 128 && deployment.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tools_are_written_and_ids_stay_in_their_prefix() {
        let dir = std::env::temp_dir().join(format!("forkrail-seed-tools-{}", forkrail_common::random_hex(6)));
        let script = write_tools(&dir).unwrap();
        assert!(script.ends_with("next-seed.mjs") && dir.join("next-cache.mjs").exists());
        // The script imports the handlers from beside it.
        assert!(std::fs::read_to_string(&script).unwrap().contains("from './next-cache.mjs'"));
        std::fs::remove_dir_all(dir).unwrap();
        assert!(safe("shop-1a2b3c4d") && !safe("../x") && !safe("a/b") && !safe(""));
        assert_eq!(seed_index_key("shop-1a2b3c4d"), "static/next-seed/shop-1a2b3c4d/index.json");
        let summary: Seeded = serde_json::from_str(r#"{"entries":7,"bytes":130334,"skipped":0,"built_ms":1794919997945}"#).unwrap();
        assert_eq!((summary.entries, summary.built_ms), (7, 1_794_919_997_945));
    }

    #[test]
    fn a_seed_entry_is_written_again_for_another_deployment() {
        let key = "ab".repeat(32);
        let hash = |scope: &str| forkrail_common::hex(&<sha2::Sha256 as sha2::Digest>::digest(forkrail_common::nextcache::hash_input(scope, &key).as_bytes()));
        let headers = [("x-forkrail-next".to_owned(), "1".to_owned()), ("vercel-cache-tag".to_owned(), "_N_T_/x".to_owned())];
        let page = forkrail_common::nextcache::encode_page("shop-old1", &hash("shop-old1"), 200, 1_000, Some(31_000), &headers, b"body\nwith lines\n");
        let again = rescope(&page, "shop-new2", &key).unwrap();
        // What the router would have written for the new deployment, byte for byte.
        assert_eq!(again, forkrail_common::nextcache::encode_page("shop-new2", &hash("shop-new2"), 200, 1_000, Some(31_000), &headers, b"body\nwith lines\n"));
        assert!(rescope(b"not a page", "shop-new2", &key).is_none());
    }
}
