//! A deployment's static files: read from the framework's public folder and compressed where it
//! pays when building, into the build folder by content with the manifest the router reads (laid
//! out as in S3: `forkrail_common::statics`); uploaded from there to every region's bucket when
//! publishing. Files already there from an earlier deployment aren't sent again.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use crate::state::s3_error;
use aws_sdk_s3::primitives::ByteStream;
use forkrail_common::statics::{
    Encoding, IMMUTABLE, Manifest, REVALIDATE, StaticFile, compressible, content_type, manifest_key, object_key, redirects_key, rules_key,
};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::deploy::Ctx;

/// A compressed copy is kept only if it's at most this share of the file, in percent.
const KEEP_IF_UNDER_PCT: u64 = 90;
/// Files smaller than this aren't worth compressing.
const MIN_COMPRESS: u64 = 256;
/// Uploads at once.
const PARALLEL: usize = 16;

/// The static files a build wrote: the manifest's id (its hash), how many files, and their size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepared {
    pub id: String,
    pub files: usize,
    pub bytes: u64,
}

/// One object to upload: its key, which is also its path in the build folder, its type, and the
/// size of the file it is (or is a compressed copy of).
struct Object {
    key: String,
    content_type: String,
    size: u64,
}

/// Names a site served from the project's own folder never serves: what Vercel's CLI leaves out
/// of a deployment, and forkrail's own configuration. Hidden files are left out too (but
/// `.well-known`), and whatever `.gitignore` and `.vercelignore` name.
const NEVER_SERVED: [&str; 8] = ["node_modules", "vercel.json", "__pycache__", "venv", "npm-debug.log", "CVS", "package-lock.json", "config.gypi"];

/// Never served from the top of a site served from its own folder: what Vercel's static builder
/// leaves out there (`VERCEL_PLATFORM_FILES` in `@vercel/fs-detectors`): the project's manifest,
/// lockfiles, README, middleware and `api/`; and the configuration files besides `vercel.json`.
const ROOT_NEVER_SERVED: [&str; 15] = [
    "api",
    "package.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "middleware.js",
    "middleware.ts",
    "README.md",
    "vercel.toml",
    "vercel.ts",
    "vercel.mts",
    "vercel.js",
    "vercel.mjs",
    "vercel.cjs",
];

/// Read `public`, compressing text files with Brotli and gzip, and write every object and the
/// manifest into the build folder `out` under their keys. `project_root` says `public` is the
/// project's own folder (a site with no output folder), whose ignore files and hidden files apply.
/// Files under the `hashed` folders are served to be cached for good; the rest are revalidated, as
/// Vercel serves files (a preset's or the project's `headers` can say otherwise). `leave_out` is
/// never served: the project's bulk redirects, which are configuration, not content.
pub fn prepare(public: &Path, out: &Path, project_root: bool, hashed: &[&str], leave_out: Option<&Path>) -> Result<Option<Prepared>> {
    prepare_with(public, out, project_root, hashed, leave_out, &Overrides::default())
}

/// A Build Output's `overrides`, by a file's path in its static folder (`blog.html`): the path it's
/// served at instead (`blog`; its own path is then not served, as on Vercel) and its type.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub paths: std::collections::BTreeMap<String, String>,
    pub types: std::collections::BTreeMap<String, String>,
    /// Files the router serves by their key rather than at a path (prerenders' fallback pages,
    /// `forkrail_common::statics::fallback_key`): in the manifest so they're uploaded and kept.
    pub extra: std::collections::BTreeMap<String, std::path::PathBuf>,
    /// Path prefixes inside a hashed folder whose files keep their names across builds (Next's
    /// service workers): revalidated, not cached for good.
    pub revalidated: Vec<String>,
}

/// `prepare`, with `overrides` applied to the files' paths and types.
pub fn prepare_with(public: &Path, out: &Path, project_root: bool, hashed: &[&str], leave_out: Option<&Path>, overrides: &Overrides) -> Result<Option<Prepared>> {
    if !public.is_dir() && overrides.extra.is_empty() {
        return Ok(None);
    }
    let mut manifest = Manifest::default();
    let mut seen = HashSet::new();
    let mut bytes = 0;
    let write = |key: &str, data: &[u8]| -> Result<()> {
        let path = out.join(key);
        std::fs::create_dir_all(path.parent().expect("keys have a folder"))?;
        std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))
    };
    let mut walk = WalkBuilder::new(public);
    walk.standard_filters(false).follow_links(true).sort_by_file_name(|a, b| a.cmp(b));
    let leave_out = leave_out.map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
    if !project_root && let Some(leave) = leave_out.clone() {
        walk.filter_entry(move |e| !e.path().canonicalize().is_ok_and(|p| p.starts_with(&leave)));
    }
    if project_root {
        walk.git_ignore(true).git_exclude(true).require_git(false).parents(false).add_custom_ignore_filename(".vercelignore");
        // The build folder may sit inside the project (a build run here into a temporary folder).
        let out = out.to_path_buf();
        walk.filter_entry(move |e| {
            let name = e.file_name().to_string_lossy();
            e.depth() == 0
                || (!(name.starts_with('.') && name != ".well-known")
                    && !NEVER_SERVED.contains(&name.as_ref())
                    && !(e.depth() == 1 && ROOT_NEVER_SERVED.contains(&name.as_ref()))
                    && !e.path().starts_with(&out)
                    && !leave_out.as_ref().is_some_and(|leave| e.path().canonicalize().is_ok_and(|p| p.starts_with(leave))))
        });
    }
    let found = if public.is_dir() { walk.build().collect() } else { Vec::new() };
    let mut files: Vec<(String, std::path::PathBuf, String)> = Vec::new();
    for entry in found {
        let entry = entry.context("listing the static files")?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(public).expect("walked from public").to_string_lossy().replace('\\', "/");
        if rel.split('/').any(|part| part == ".DS_Store") {
            continue;
        }
        let url = match overrides.paths.get(&rel) {
            Some(to) => format!("/{}", to.trim_start_matches('/')),
            None => format!("/{rel}"),
        };
        files.push((rel, path.to_path_buf(), url));
    }
    for (key, path) in &overrides.extra {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        files.push((name, path.clone(), key.clone()));
    }
    for (rel, path, url) in files {
        let path = path.as_path();
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let sha256 = forkrail_common::hex(&Sha256::digest(&data));
        let kind = overrides.types.get(&rel).cloned().unwrap_or_else(|| content_type(&rel).to_owned());
        let size = data.len() as u64;
        bytes += size;
        let (mut br, mut gz) = (None, None);
        if compressible(&kind) && size >= MIN_COMPRESS {
            let brotli = compress_brotli(&data)?;
            let gzip = compress_gzip(&data)?;
            if (brotli.len() as u64) * 100 <= size * KEEP_IF_UNDER_PCT {
                br = Some(brotli.len() as u64);
                if seen.insert(object_key(&sha256, Encoding::Brotli)) {
                    write(&object_key(&sha256, Encoding::Brotli), &brotli)?;
                }
            }
            if (gzip.len() as u64) * 100 <= size * KEEP_IF_UNDER_PCT {
                gz = Some(gzip.len() as u64);
                if seen.insert(object_key(&sha256, Encoding::Gzip)) {
                    write(&object_key(&sha256, Encoding::Gzip), &gzip)?;
                }
            }
        }
        if seen.insert(object_key(&sha256, Encoding::Identity)) {
            write(&object_key(&sha256, Encoding::Identity), &data)?;
        }
        let hashed_here = hashed.iter().any(|h| url.starts_with(h)) && !overrides.revalidated.iter().any(|r| url.starts_with(r.as_str()));
        let cache_control = if hashed_here { IMMUTABLE } else { REVALIDATE };
        manifest.files.insert(url, StaticFile { sha256, size, content_type: kind, cache_control: cache_control.to_owned(), br, gz });
    }
    if manifest.files.is_empty() {
        return Ok(None);
    }
    let manifest_json = serde_json::to_vec(&manifest)?;
    let id = forkrail_common::hex(&Sha256::digest(&manifest_json));
    write(&manifest_key(&id), &manifest_json)?;
    Ok(Some(Prepared { id, files: manifest.files.len(), bytes }))
}

/// The objects manifest `id` refers to, with their types, each once.
fn objects_of(manifest: &Manifest) -> Vec<Object> {
    let mut objects: Vec<Object> = Vec::new();
    let mut seen = HashSet::new();
    for file in manifest.files.values() {
        for encoding in [Encoding::Identity, Encoding::Brotli, Encoding::Gzip] {
            if file.size_of(encoding).is_some() && seen.insert(file.key(encoding)) {
                objects.push(Object { key: file.key(encoding), content_type: file.content_type.clone(), size: file.size });
            }
        }
    }
    objects
}

/// Whether `id` can name something by its content: a SHA-256, in lowercase hex.
pub fn content_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// That the file at `path` holds what `key` says it does. Everything under `static/` is named by
/// the SHA-256 of its bytes (a Brotli or gzip copy by its file's, `size` bytes once decompressed)
/// and shared by every project in the bucket: one project's file is another's when the bytes are
/// the same, and once a key is there it's never sent again. A build's folder is the build's own
/// (its code ran there and may have rewritten anything in it), so what's sent from it is hashed
/// here first: otherwise a build could put bytes of its choosing under a name another project's
/// deployment will use, and the router would serve them on that project's domain.
pub fn check_content(key: &str, path: &Path, size: Option<u64>) -> Result<()> {
    use std::io::Read;
    let name = key.rsplit('/').next().unwrap_or(key);
    let (name, encoding) = match (name.strip_suffix(".br"), name.strip_suffix(".gz")) {
        (Some(name), _) => (name, Encoding::Brotli),
        (_, Some(name)) => (name, Encoding::Gzip),
        _ => (name, Encoding::Identity),
    };
    let id = name.strip_suffix(".json").unwrap_or(name);
    if !content_id(id) {
        bail!("{key} isn't named by its content");
    }
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let reader: Box<dyn Read> = match encoding {
        Encoding::Identity => Box::new(file),
        Encoding::Brotli => Box::new(brotli::Decompressor::new(file, 64 << 10)),
        Encoding::Gzip => Box::new(flate2::read::GzDecoder::new(file)),
    };
    // A copy is read no further than the file it copies, so it can't unpack without end.
    let mut reader = reader.take(size.map_or(u64::MAX, |n| n.saturating_add(1)));
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 << 10];
    let mut read = 0u64;
    loop {
        let n = reader.read(&mut buf).with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read += n as u64;
    }
    if size.is_some_and(|n| n != read) || forkrail_common::hex(&hasher.finalize()) != id {
        bail!("{} isn't what its name {key} says: a build can only add files by their content", path.display());
    }
    Ok(())
}

fn read_manifest(out: &Path, id: &str) -> Result<Manifest> {
    if !content_id(id) {
        bail!("{id:?} isn't a static manifest's id");
    }
    let path = out.join(manifest_key(id));
    let text = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&text).with_context(|| format!("{} isn't a static manifest", path.display()))?;
    if forkrail_common::hex(&Sha256::digest(&text)) != id {
        bail!("{} doesn't match its name", path.display());
    }
    // Every file by its hash, whether or not the bucket has it already: a name that isn't one
    // (`next-seed/<another deployment>/…`) would serve another object of `static/` as this one's.
    if let Some((path, _)) = manifest.files.iter().find(|(_, f)| !content_id(&f.sha256)) {
        bail!("the static manifest names {path} by something that isn't a SHA-256");
    }
    Ok(manifest)
}

fn compress_brotli(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        // Quality 11 is slow, but runs once per file and the copy is served for good.
        let mut writer = brotli::CompressorWriter::new(&mut out, 64 << 10, 11, 22);
        writer.write_all(data)?;
    }
    Ok(out)
}

fn compress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Write a deployment's routing program into the build folder, at its key, and return its id.
pub fn write_rules(program: &forkrail_common::rules::Program, out: &Path) -> Result<String> {
    let json = serde_json::to_vec(program)?;
    let id = forkrail_common::hex(&Sha256::digest(&json));
    let path = out.join(rules_key(&id));
    std::fs::create_dir_all(path.parent().expect("keys have a folder"))?;
    std::fs::write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// Write a deployment's Image Optimization settings into the build folder, at their key
/// (`static/images/<id>.json`), and return their id. Uploaded with the routing rules.
pub fn write_images(config: &forkrail_common::images::ImagesConfig, out: &Path) -> Result<String> {
    let (json, id) = config.to_stored();
    let path = out.join(forkrail_common::statics::images_key(&id));
    std::fs::create_dir_all(path.parent().expect("keys have a folder"))?;
    std::fs::write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// Write a table of bulk redirects into the build folder, at its key, and return its id.
pub fn write_redirects(table: &[u8], out: &Path) -> Result<String> {
    let id = forkrail_common::hex(&Sha256::digest(table));
    let path = out.join(redirects_key(&id));
    std::fs::create_dir_all(path.parent().expect("keys have a folder"))?;
    std::fs::write(&path, table).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// Put routing program `id` in `region`'s bucket, and the bulk redirects it names before it: from
/// the build folder, or for a build published again (`out` None) from the home region's bucket,
/// where they must be already.
pub async fn upload_rules(ctx: &Ctx, region: &str, out: Option<&Path>, id: &str) -> Result<()> {
    if !content_id(id) {
        bail!("{id:?} isn't a routing program's id");
    }
    let program: forkrail_common::rules::Program = match out {
        Some(out) => {
            let path = out.join(rules_key(id));
            serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?)?
        }
        None => {
            let got = ctx.s3_in(&ctx.install.home_region).get_object().bucket(&ctx.store.bucket).key(rules_key(id)).send().await;
            let got = got.map_err(|e| anyhow::anyhow!("reading the routing rules {id}: {}", s3_error(&e)))?;
            serde_json::from_slice(&got.body.collect().await.context("reading the routing rules")?.into_bytes())?
        }
    };
    if let Some(table) = &program.bulk {
        upload_object(ctx, region, out, &redirects_key(table), "application/octet-stream").await?;
    }
    upload_object(ctx, region, out, &rules_key(id), "application/json").await
}

/// Put `key` in `region`'s bucket unless it's there: from the build folder, or from the home
/// region's bucket.
pub async fn upload_object(ctx: &Ctx, region: &str, out: Option<&Path>, key: &str, content_type: &str) -> Result<()> {
    let key = key.to_owned();
    let home = region == ctx.install.home_region;
    let bucket = if home { ctx.store.bucket.clone() } else { crate::regions::ensure_code_bucket(ctx, region).await? };
    let s3 = ctx.s3_in(region);
    // While a sweep deletes static files, a build's own objects are sent whatever the bucket has.
    let resend = out.is_some() && crate::storage::sweeping(ctx).await;
    match s3.head_object().bucket(&bucket).key(&key).send().await {
        Ok(_) if !resend => return Ok(()),
        Ok(_) => {}
        Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found()) => {}
        Err(e) => bail!("checking s3://{bucket}/{key}: {}", s3_error(&e)),
    }
    let body = match out {
        Some(out) => {
            let path = out.join(&key);
            let (checked_key, checked_path) = (key.clone(), path.clone());
            tokio::task::spawn_blocking(move || check_content(&checked_key, &checked_path, None)).await??;
            ByteStream::from_path(&path).await.with_context(|| format!("reading {}", path.display()))?
        }
        None if home => bail!("{key} is gone"),
        None => {
            let got = ctx.s3_in(&ctx.install.home_region).get_object().bucket(&ctx.store.bucket).key(&key).send().await;
            let got = got.map_err(|e| anyhow::anyhow!("reading s3://{}/{key}: {}", ctx.store.bucket, s3_error(&e)))?;
            ByteStream::from(got.body.collect().await.with_context(|| format!("reading {key}"))?.into_bytes())
        }
    };
    put(&s3, &bucket, &key, content_type, body).await
}

/// Upload what `region`'s bucket doesn't have, the manifest last, so a router never reads a
/// manifest whose files aren't there yet. Returns how many objects were sent.
pub async fn upload(ctx: &Ctx, region: &str, out: &Path, id: &str) -> Result<usize> {
    let objects = objects_of(&read_manifest(out, id)?);
    let bucket = if region == ctx.install.home_region { ctx.store.bucket.clone() } else { crate::regions::ensure_code_bucket(ctx, region).await? };
    let s3 = ctx.s3_in(region);
    // While a sweep deletes static files, nothing already there is counted on: it may be one the
    // sweep found unused a moment ago.
    let present = if crate::storage::sweeping(ctx).await { Default::default() } else { list(&s3, &bucket, "static/").await? };
    let manifest = manifest_key(id);
    let missing: Vec<&Object> = objects.iter().filter(|o| !present.contains(&o.key)).collect();
    let sent = missing.len();
    let slots = Arc::new(Semaphore::new(PARALLEL));
    let mut uploads = tokio::task::JoinSet::new();
    for object in missing {
        let path = out.join(&object.key);
        let (s3, bucket, key, kind, size, slots) = (s3.clone(), bucket.clone(), object.key.clone(), object.content_type.clone(), object.size, slots.clone());
        uploads.spawn(async move {
            let _slot = slots.acquire_owned().await.expect("never closed");
            let (checked_key, checked_path) = (key.clone(), path.clone());
            tokio::task::spawn_blocking(move || check_content(&checked_key, &checked_path, Some(size))).await??;
            let body = ByteStream::from_path(&path).await.with_context(|| format!("reading {}", path.display()))?;
            put(&s3, &bucket, &key, &kind, body).await
        });
    }
    while let Some(done) = uploads.join_next().await {
        done??;
    }
    if !present.contains(&manifest) {
        let path = out.join(&manifest);
        let body = ByteStream::from_path(&path).await.with_context(|| format!("reading {}", path.display()))?;
        put(&s3, &bucket, &manifest, "application/json", body).await?;
    }
    Ok(sent + usize::from(!present.contains(&manifest)))
}

/// Make sure `region`'s bucket has manifest `id` and its files, for publishing a deployment's build
/// again: in the home region they must be there already; elsewhere they're copied from it if the
/// manifest is missing. Returns how many objects were copied.
pub async fn ensure(ctx: &Ctx, region: &str, id: &str) -> Result<usize> {
    let manifest_key = manifest_key(id);
    if region == ctx.install.home_region {
        if !ctx.store.exists(&manifest_key).await? {
            bail!("the static files' manifest {manifest_key} is gone");
        }
        return Ok(0);
    }
    let bucket = crate::regions::ensure_code_bucket(ctx, region).await?;
    match ctx.s3_in(region).head_object().bucket(&bucket).key(&manifest_key).send().await {
        Ok(_) => Ok(0),
        Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found()) => replicate(ctx, region, id).await,
        Err(e) => bail!("checking s3://{bucket}/{manifest_key}: {}", s3_error(&e)),
    }
}

/// Copy manifest `id` and its files from the home region's bucket to `region`'s, for a deployment
/// published there after the fact. The manifest goes last. Returns how many objects were copied.
pub async fn replicate(ctx: &Ctx, region: &str, id: &str) -> Result<usize> {
    let manifest_key = manifest_key(id);
    let text = ctx.store.s3.get_object().bucket(&ctx.store.bucket).key(&manifest_key).send().await;
    let text = match text {
        Ok(out) => out.body.collect().await.context("reading the static manifest")?.into_bytes(),
        Err(e) => bail!("reading s3://{}/{manifest_key}: {}", ctx.store.bucket, s3_error(&e)),
    };
    let manifest: Manifest = serde_json::from_slice(&text).context("the static manifest isn't valid")?;
    let bucket = crate::regions::ensure_code_bucket(ctx, region).await?;
    let s3 = ctx.s3_in(region);
    let present = list(&s3, &bucket, "static/").await?;
    let mut keys: Vec<String> = objects_of(&manifest).into_iter().map(|o| o.key).filter(|k| !present.contains(k)).collect();
    keys.push(manifest_key.clone());
    let copied = keys.len();
    for key in keys {
        s3.copy_object()
            .bucket(&bucket)
            .key(&key)
            .copy_source(format!("{}/{key}", ctx.store.bucket))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("copying {key} to s3://{bucket}: {}", s3_error(&e)))?;
    }
    Ok(copied)
}

async fn put(s3: &aws_sdk_s3::Client, bucket: &str, key: &str, content_type: &str, body: ByteStream) -> Result<()> {
    s3.put_object()
        .bucket(bucket)
        .key(key)
        .content_type(content_type)
        .body(body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("uploading s3://{bucket}/{key}: {}", s3_error(&e)))?;
    Ok(())
}

async fn list(s3: &aws_sdk_s3::Client, bucket: &str, prefix: &str) -> Result<HashSet<String>> {
    let mut keys = HashSet::new();
    let mut pages = s3.list_objects_v2().bucket(bucket).prefix(prefix).into_paginator().send();
    while let Some(page) = pages.next().await {
        let page = page.map_err(|e| anyhow::anyhow!("listing s3://{bucket}/{prefix}: {}", s3_error(&e)))?;
        keys.extend(page.contents().iter().filter_map(|o| o.key().map(str::to_owned)));
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_compresses_text_and_dedups() {
        let dir = std::env::temp_dir().join(format!("forkrail-static-{}", forkrail_common::random_hex(6)));
        let css = "body { color: red; }\n".repeat(100);
        for (name, data) in [
            ("assets/app-1a2b.css", css.as_bytes().to_vec()),
            ("copy.css", css.as_bytes().to_vec()),
            ("logo.png", vec![7u8; 2000]),
            ("tiny.txt", b"hi".to_vec()),
        ] {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        }
        let out = dir.with_extension("out");
        let p = prepare(&dir, &out, false, &["/assets/"], None).unwrap().unwrap();
        let manifest = read_manifest(&out, &p.id).unwrap();
        let css_file = &manifest.files["/assets/app-1a2b.css"];
        assert!(css_file.br.is_some() && css_file.gz.is_some());
        assert_eq!(css_file.cache_control, IMMUTABLE);
        assert_eq!(manifest.files["/copy.css"].cache_control, REVALIDATE);
        // A prefix inside a hashed folder that's revalidated instead (Next's service workers).
        let overrides = Overrides { revalidated: vec!["/assets/app-".to_owned()], ..Overrides::default() };
        let q = prepare_with(&dir, &dir.with_extension("out2"), false, &["/assets/"], None, &overrides).unwrap().unwrap();
        assert_eq!(read_manifest(&dir.with_extension("out2"), &q.id).unwrap().files["/assets/app-1a2b.css"].cache_control, REVALIDATE);
        // The same contents are stored once.
        assert_eq!(manifest.files["/copy.css"].sha256, css_file.sha256);
        // PNG isn't compressed, and tiny files aren't worth it.
        assert!(manifest.files["/logo.png"].br.is_none() && manifest.files["/tiny.txt"].gz.is_none());
        // css (3 copies), png, txt, each in the build folder under its key, and the manifest.
        let objects = objects_of(&manifest);
        assert_eq!(objects.len(), 5);
        assert!(objects.iter().all(|o| out.join(&o.key).is_file()));
        let written = WalkBuilder::new(&out).standard_filters(false).build().filter(|e| e.as_ref().is_ok_and(|e| e.path().is_file())).count();
        assert_eq!(written, 6);
        // Each holds what its name says, compressed copies included.
        for o in &objects {
            check_content(&o.key, &out.join(&o.key), Some(o.size)).unwrap();
        }
        // A build that swaps a file's bytes (or its copy's) can't send them under its name.
        let css_key = css_file.key(Encoding::Identity);
        let css_path = out.join(&css_key);
        let kept = std::fs::read(&css_path).unwrap();
        std::fs::write(&css_path, "alert(1)").unwrap();
        assert!(check_content(&css_key, &css_path, Some(css_file.size)).is_err());
        std::fs::write(&css_path, &kept).unwrap();
        let br_key = css_file.key(Encoding::Brotli);
        std::fs::write(out.join(&br_key), compress_brotli(b"alert(1)").unwrap()).unwrap();
        assert!(check_content(&br_key, &out.join(&br_key), Some(css_file.size)).is_err());
        let gz_key = css_file.key(Encoding::Gzip);
        std::fs::write(out.join(&gz_key), compress_gzip(&[kept.clone(), kept.clone()].concat()).unwrap()).unwrap();
        assert!(check_content(&gz_key, &out.join(&gz_key), Some(css_file.size)).is_err(), "no more than the file");
        assert!(check_content("static/rules/../../etc/passwd.json", &css_path, None).is_err());
        assert!(read_manifest(&out, "../manifests/x").is_err());
        // A manifest whose file names another object of the bucket is refused, though it hashes right.
        let mut forged = read_manifest(&out, &p.id).unwrap();
        forged.files.values_mut().next().unwrap().sha256 = "next-seed/shop-0a1b2c3d/index.json".into();
        let text = serde_json::to_vec(&forged).unwrap();
        let forged_id = forkrail_common::hex(&Sha256::digest(&text));
        std::fs::write(out.join(manifest_key(&forged_id)), &text).unwrap();
        assert!(read_manifest(&out, &forged_id).is_err());
        assert_eq!(p.files, 4);
        // The id is the manifest's hash, so the same files give the same id.
        assert_eq!(prepare(&dir, &out, false, &["/assets/"], None).unwrap().unwrap().id, p.id);
        assert!(prepare(&dir.join("missing"), &out, false, &[], None).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn a_site_served_from_its_own_folder_keeps_its_secrets() {
        let dir = std::env::temp_dir().join(format!("forkrail-root-{}", forkrail_common::random_hex(6)));
        for name in [
            "index.html",
            "about/index.html",
            ".well-known/security.txt",
            ".env",
            ".git/config",
            "node_modules/x/index.js",
            "vercel.json",
            "drafts/secret.html",
            "notes.txt",
            "README.md",
            "package.json",
            "api/data.json",
            "docs/README.md",
        ] {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        std::fs::write(dir.join(".vercelignore"), "drafts\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "notes.txt\n").unwrap();
        let out = dir.join(".forkrail-out");
        let p = prepare(&dir, &out, true, &[], None).unwrap().unwrap();
        let manifest = read_manifest(&out, &p.id).unwrap();
        let served: Vec<&str> = manifest.files.keys().map(String::as_str).collect();
        assert_eq!(served, ["/.well-known/security.txt", "/about/index.html", "/docs/README.md", "/index.html"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
