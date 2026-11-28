//! A deployment's static files, served by the router before the function, from S3 in the router's
//! own region (see `forkrail_common::statics` for the layout). S3 to the router is free in-region,
//! so a file costs only its egress to the client, once; the function never sees the request.
//!
//! Manifests are kept once read: they never change. File contents are kept in memory up to
//! `FORKRAIL_STATIC_CACHE_MB` (default 64), least recently used out first. A file the manifest
//! names but S3 can't give is left to the function, which carries its own copy.
//!
//! Deployments' routing programs (`forkrail_common::rules`) are read from the same place, at
//! `static/rules/<id>.json`, and kept compiled; a project's own rules are compiled in front of
//! each of its deployments' programs. Tables of bulk redirects (`static/redirects/<id>`) are kept
//! in memory, or past `MAP_FROM` written to `FORKRAIL_REDIRECTS_DIR` (default
//! `/var/lib/forkrail/redirects`) and mapped from there, as the GeoIP files are.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use forkrail_common::rules::bulk::Table;
use forkrail_common::rules::{Engine, Program};
use forkrail_common::statics::{Encoding, Manifest, StaticFile, manifest_key, negotiate, redirects_key, rules_key};
use http_body_util::{BodyExt, Empty, Full};
use hyper::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, ACCESS_CONTROL_ALLOW_ORIGIN, AGE, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    HeaderName, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, LAST_MODIFIED, RANGE, VARY,
};
use hyper::{HeaderMap, Method, Response, StatusCode};
use tracing::warn;

use crate::proxy::ResponseBody;

/// Says whether the file came from the router's memory (`HIT`) or from S3 (`MISS`).
pub(crate) const CACHE_HEADER: HeaderName = HeaderName::from_static("x-forkrail-cache");
/// After a manifest fails to load, requests go to the function for this long before it's tried again.
const RETRY_FAILED_MANIFEST: Duration = Duration::from_secs(30);
/// A routing program that couldn't be read is tried again this soon: until then its deployment
/// answers 502, never without its rules (`proxy.rs`).
const RETRY_FAILED_PROGRAM: Duration = Duration::from_secs(2);
/// The most of each a build's output may hold, read whole into memory: a deployment's own file
/// can't take the router's memory, and every project on it, down. Far above what a build makes.
const MAX_MANIFEST_BYTES: u64 = 64 << 20;
const MAX_PROGRAM_BYTES: u64 = 64 << 20;
const MAX_TABLE_BYTES: u64 = 512 << 20;
/// Configurations kept elsewhere (`object`): a firewall's, an image's.
const MAX_OBJECT_BYTES: u64 = 64 << 20;
/// Manifests kept; past it, all are dropped and read again as needed. Each is a few KB.
const MAX_MANIFESTS: usize = 512;
/// Larger files are served but not kept, so one big file can't empty the cache.
const MAX_CACHED_SHARE: usize = 8;

pub struct Statics {
    source: Source,
    manifests: Mutex<HashMap<String, ManifestSlot>>,
    /// Deployments' routing programs, compiled, by id (with a project's rules, by both ids): like
    /// manifests, they never change.
    programs: Mutex<HashMap<String, ProgramSlot>>,
    /// Tables of bulk redirects, by id.
    tables: Mutex<HashMap<String, TableSlot>>,
    /// Held while a table loads, so a big one is fetched once.
    loading: tokio::sync::Mutex<()>,
    cache: Mutex<Lru<Kept>>,
}

/// A file's bytes in memory, and when they came: Vercel's `last-modified` and `age` for files
/// are its cache's, so the router's are its memory's.
#[derive(Clone)]
struct Kept {
    bytes: Bytes,
    at: SystemTime,
}

/// What a deployment's file is served as (`Statics::respond`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Serve {
    /// A file a client asked for, or a page served with a status (the 404 page): with the headers
    /// Vercel's CDN puts on files, byte ranges and conditional requests.
    File,
    /// A page given in a function's place (a prerender's fallback): the file alone, the response
    /// cache adding its own headers.
    Page,
}

/// A table's bytes: in memory, or mapped from a file.
pub enum TableBytes {
    Memory(Bytes),
    Mapped(memmap2::Mmap),
}

impl AsRef<[u8]> for TableBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            TableBytes::Memory(b) => b,
            TableBytes::Mapped(m) => m,
        }
    }
}

pub type Redirects = Table<TableBytes>;

#[derive(Clone)]
enum TableSlot {
    Loaded(Arc<Redirects>),
    Failed(Instant),
}

/// Tables larger than this are kept on disk and mapped, not held in memory.
const MAP_FROM: usize = 16 << 20;
/// Tables kept; past it, all are dropped and read again as needed.
const MAX_TABLES: usize = 256;

/// Which of a request's routing programs couldn't be read.
pub enum Unreadable {
    Deployment,
}

#[derive(Clone)]
enum ProgramSlot {
    Loaded(Arc<Engine>),
    Failed(Instant),
}

/// Where the files are: a bucket, or for local runs a directory with the same layout.
pub enum Source {
    S3 { client: aws_sdk_s3::Client, bucket: String },
    Dir(std::path::PathBuf),
}

#[derive(Clone)]
enum ManifestSlot {
    Loaded(Arc<Manifest>),
    Failed(Instant),
}

/// What `serve` answered, for the access log.
pub struct Served {
    pub response: Response<ResponseBody>,
    pub bytes: u64,
    pub encoding: Option<&'static str>,
    /// A 304: as Vercel answers one for a file, without the routes' headers.
    pub not_modified: bool,
}

impl Statics {
    pub fn new(source: Source, cache_bytes: usize) -> Self {
        Self {
            source,
            manifests: Mutex::new(HashMap::new()),
            programs: Mutex::new(HashMap::new()),
            tables: Mutex::new(HashMap::new()),
            loading: tokio::sync::Mutex::new(()),
            cache: Mutex::new(Lru::new(cache_bytes)),
        }
    }

    fn describe(&self) -> String {
        match &self.source {
            Source::S3 { bucket, .. } => format!("s3://{bucket}"),
            Source::Dir(dir) => dir.display().to_string(),
        }
    }

    /// The response for one of a deployment's files, with `status` instead of 200 if given (a
    /// rewrite's `statusCode`, the 404 page), served as `serve` says. `None` if S3 can't give it.
    ///
    /// As a file (`Serve::File`), it's answered as Vercel's CDN answers its files (seen on every
    /// Vercel copy, 2026-11-16): `access-control-allow-origin: *`; `x-vercel-cache` HIT when the
    /// router's memory had it, MISS when it was read from S3, with `last-modified` the time it
    /// came into memory and `age` the seconds since (Vercel's are its region's cache's: a file
    /// first asked for now says MISS, age 0, last-modified now); `accept-ranges: bytes` when it
    /// goes as it is (not compressed). A plain answer (no status) also takes one byte range (206,
    /// of the file as it is whatever the client accepts; when it lies outside the file, 416 with
    /// `content-range: bytes */size` and Vercel's `RANGE_NOT_SATISFIABLE` error, one of the two
    /// answers Vercel gives, the other being the whole file), `If-Range` (an entity tag, or the
    /// exact `last-modified`), `If-None-Match` and, without it, `If-Modified-Since` (a 304 with
    /// `cache-control`, `etag` and `x-vercel-cache` only, Vercel's). A page with a status takes
    /// neither ranges nor 304s (Vercel answers both for a 404 page; RFC 9110 has them for 200s).
    pub async fn respond<F, Fut, P>(&self, file: &StaticFile, method: &Method, headers: &HeaderMap, status: Option<StatusCode>, serve: Serve, room: F) -> Option<Served>
    where
        F: FnOnce(usize) -> Fut,
        Fut: std::future::Future<Output = P>,
    {
        let file = file.clone();
        let as_file = serve == Serve::File;
        let plain = status.is_none_or(|s| s == StatusCode::OK);
        let now = SystemTime::now();
        // A byte range is of the file as it is, whatever encodings the client takes.
        let range = match headers.get(RANGE).and_then(|v| v.to_str().ok()) {
            Some(value) if as_file && plain && matches!(*method, Method::GET | Method::HEAD) => {
                let identity = file.key(Encoding::Identity);
                let modified = unix(self.kept_at(&identity).unwrap_or(now));
                if if_range_holds(headers, &file.etag(Encoding::Identity), modified) { byte_range(value, file.size) } else { None }
            }
            _ => None,
        };
        let encoding = match range {
            Some(_) => Encoding::Identity,
            None => negotiate(&file, headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok())),
        };
        let size = file.size_of(encoding)? as usize;
        let key = file.key(encoding);
        let mut response_headers = HeaderMap::new();
        if plain {
            let kept_at = self.kept_at(&key);
            let modified = unix(kept_at.unwrap_or(now));
            let since = headers.get(IF_MODIFIED_SINCE).and_then(|v| v.to_str().ok()).and_then(parse_http_date);
            let unchanged = not_modified(headers, &file) || (as_file && !headers.contains_key(IF_NONE_MATCH) && since.is_some_and(|since| modified <= since));
            if unchanged {
                set(&mut response_headers, CACHE_CONTROL, &file.cache_control);
                set(&mut response_headers, ETAG, &file.etag(encoding));
                if file.compressed() {
                    response_headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
                }
                if as_file {
                    response_headers.insert(crate::cache::STATUS_HEADER, HeaderValue::from_static(if kept_at.is_some() { "HIT" } else { "MISS" }));
                } else {
                    set(&mut response_headers, CONTENT_TYPE, &file.content_type);
                    if let Some(coding) = encoding.header() {
                        response_headers.insert(CONTENT_ENCODING, HeaderValue::from_static(coding));
                    }
                }
                let mut response = Response::new(empty());
                *response.status_mut() = StatusCode::NOT_MODIFIED;
                *response.headers_mut() = response_headers;
                return Some(Served { response, bytes: 0, encoding: None, not_modified: true });
            }
        }
        set(&mut response_headers, CONTENT_TYPE, &file.content_type);
        set(&mut response_headers, CACHE_CONTROL, &file.cache_control);
        set(&mut response_headers, ETAG, &file.etag(encoding));
        if file.compressed() {
            response_headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
        }
        if let Some(coding) = encoding.header() {
            response_headers.insert(CONTENT_ENCODING, HeaderValue::from_static(coding));
        }
        // What's kept under the key is used only at the size this manifest says: two entries naming
        // one key with different sizes would otherwise slice past its end.
        let kept = self.cache.lock().unwrap().get(&key).filter(|k| k.bytes.len() == size);
        let hit = kept.is_some();
        let Kept { bytes, at } = match kept {
            Some(kept) => kept,
            None => {
                let _room = room(size).await;
                match self.fetch(&key, size as u64).await {
                    Ok(bytes) if bytes.len() == size => {
                        let kept = Kept { bytes, at: now };
                        self.cache.lock().unwrap().put(key, kept.clone(), size);
                        kept
                    }
                    Ok(bytes) => {
                        warn!(key, expected = size, got = bytes.len(), "static file has the wrong size; leaving it to the function");
                        return None;
                    }
                    Err(e) => {
                        warn!(source = %self.describe(), key, error = %e, "reading a static file failed; leaving it to the function");
                        return None;
                    }
                }
            }
        };
        response_headers.insert(CACHE_HEADER, HeaderValue::from_static(if hit { "HIT" } else { "MISS" }));
        if as_file {
            response_headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            if encoding == Encoding::Identity {
                response_headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
            }
            set(&mut response_headers, LAST_MODIFIED, &crate::images::http_date(unix(at)));
            let age = if hit { now.duration_since(at).map_or(0, |d| d.as_secs()) } else { 0 };
            response_headers.insert(AGE, HeaderValue::from(age));
            response_headers.insert(crate::cache::STATUS_HEADER, HeaderValue::from_static(if hit { "HIT" } else { "MISS" }));
        }
        let (status, bytes) = match range {
            Some(Ranged::Bytes(first, last)) => {
                set(&mut response_headers, CONTENT_RANGE, &format!("bytes {first}-{last}/{}", file.size));
                (Some(StatusCode::PARTIAL_CONTENT), bytes.slice(first as usize..=last as usize))
            }
            Some(Ranged::Unsatisfiable) => {
                // Vercel's error, with the file's caching headers and none of a representation's.
                set(&mut response_headers, CONTENT_RANGE, &format!("bytes */{}", file.size));
                response_headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
                response_headers.insert(HeaderName::from_static("x-forkrail-error"), HeaderValue::from_static("RANGE_NOT_SATISFIABLE"));
                for name in [ACCEPT_RANGES, VARY] {
                    response_headers.remove(name);
                }
                (Some(StatusCode::RANGE_NOT_SATISFIABLE), Bytes::from_static(b"An error occurred\n\nRANGE_NOT_SATISFIABLE\n"))
            }
            None => (status, bytes),
        };
        response_headers.insert(CONTENT_LENGTH, HeaderValue::from(bytes.len()));
        let len = bytes.len() as u64;
        let body = if *method == Method::HEAD { empty() } else { Full::new(bytes).map_err(|never| match never {}).boxed() };
        let mut response = Response::new(body);
        *response.headers_mut() = response_headers;
        if let Some(status) = status {
            *response.status_mut() = status;
        }
        Some(Served { response, bytes: if *method == Method::HEAD { 0 } else { len }, encoding: encoding.header(), not_modified: false })
    }

    /// When `key`'s bytes came into memory, if they're there.
    fn kept_at(&self, key: &str) -> Option<SystemTime> {
        self.cache.lock().unwrap().get(key).map(|kept| kept.at)
    }

    /// The routing to run for a deployment: its program (`deployment`, or the plain one: its
    /// files, then its function if it has one), with its project's rules (`project`) in front,
    /// compiled once. A project program that can't be read is left out, with a warning; a
    /// deployment's that can't be is `Unreadable`. While a failed read is recent it isn't tried
    /// again.
    pub async fn program(&self, deployment: Option<&str>, project: Option<&str>, has_function: bool) -> Result<Arc<Engine>, Unreadable> {
        let base = match deployment {
            Some(id) => id.to_owned(),
            None => format!("plain-{has_function}"),
        };
        let key = match project {
            Some(p) => format!("{p}+{base}"),
            None => base.clone(),
        };
        let slot = self.programs.lock().unwrap().get(&key).cloned();
        match slot {
            Some(ProgramSlot::Loaded(engine)) => return Ok(engine),
            Some(ProgramSlot::Failed(at)) if at.elapsed() < RETRY_FAILED_PROGRAM => {
                return match project {
                    // The project's rules failed: the deployment's alone.
                    Some(_) => Box::pin(self.program(deployment, None, has_function)).await,
                    None => Err(Unreadable::Deployment),
                };
            }
            _ => {}
        }
        let mut program = match deployment {
            Some(id) => match self.read_program(id).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(source = %self.describe(), key = rules_key(id), error = %e, "reading a routing program failed");
                    self.programs.lock().unwrap().insert(base, ProgramSlot::Failed(Instant::now()));
                    if project.is_some() {
                        self.programs.lock().unwrap().insert(key, ProgramSlot::Failed(Instant::now()));
                    }
                    return Err(Unreadable::Deployment);
                }
            },
            None => Program { fallback: has_function, ..Program::default() },
        };
        if let Some(pid) = project {
            match self.read_program(pid).await {
                Ok(own) => {
                    // The project's rules run first: in front of the deployment's, at the start.
                    // One that doesn't say matches case-insensitively, as the dashboard tested it
                    // (published before rules said so), never as the deployment's routes do.
                    let mut routes = own.routes;
                    for route in &mut routes {
                        if let forkrail_common::rules::Route::Source(source) = route {
                            source.case_sensitive.get_or_insert(false);
                        }
                    }
                    routes.extend(program.routes);
                    program.routes = routes;
                }
                Err(e) => {
                    warn!(source = %self.describe(), key = rules_key(pid), error = %e, "reading a project's routing rules failed; its deployments route without them");
                    self.programs.lock().unwrap().insert(key, ProgramSlot::Failed(Instant::now()));
                    return Box::pin(self.program(deployment, None, has_function)).await;
                }
            }
        }
        // Compiling a program of thousands of patterns takes a while: off the request threads.
        let compiled = tokio::task::spawn_blocking(move || Engine::new(&program)).await.unwrap_or_else(|e| Err(e.to_string()));
        let compiled = compiled.map(Arc::new);
        {
            let mut programs = self.programs.lock().unwrap();
            if programs.len() >= MAX_MANIFESTS {
                programs.clear();
            }
            match &compiled {
                Ok(engine) => programs.insert(key.clone(), ProgramSlot::Loaded(engine.clone())),
                Err(_) => programs.insert(key.clone(), ProgramSlot::Failed(Instant::now())),
            };
        }
        match compiled {
            Ok(engine) => Ok(engine),
            Err(e) => {
                warn!(key, error = %e, "a routing program doesn't compile");
                match project {
                    Some(_) => Box::pin(self.program(deployment, None, has_function)).await,
                    None => Err(Unreadable::Deployment),
                }
            }
        }
    }

    async fn read_program(&self, id: &str) -> Result<Program, String> {
        let bytes = self.fetch(&rules_key(id), MAX_PROGRAM_BYTES).await?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }

    /// A table of bulk redirects, read once. While a failed read is recent, `None` without trying
    /// again.
    pub async fn redirects(&self, id: &str) -> Option<Arc<Redirects>> {
        let cached = || match self.tables.lock().unwrap().get(id).cloned() {
            Some(TableSlot::Loaded(t)) => Some(Some(t)),
            Some(TableSlot::Failed(at)) if at.elapsed() < RETRY_FAILED_MANIFEST => Some(None),
            _ => None,
        };
        if let Some(found) = cached() {
            return found;
        }
        // Its name is its hash, and a file on disk is named after it (`map`): nothing else is one.
        if !forkrail_common::statics::valid_manifest_id(id) {
            warn!(id, "bulk redirects named by something that isn't a hash; requests go on without them");
            return None;
        }
        let _loading = self.loading.lock().await;
        if let Some(found) = cached() {
            return found;
        }
        let key = redirects_key(id);
        let loaded = match self.fetch(&key, MAX_TABLE_BYTES).await {
            Ok(bytes) => {
                let id = id.to_owned();
                tokio::task::spawn_blocking(move || -> Result<Redirects, String> {
                    let stored = if bytes.len() >= MAP_FROM { map(&id, &bytes) } else { None };
                    Table::new(stored.unwrap_or(TableBytes::Memory(bytes)))
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            }
            Err(e) => Err(e),
        };
        let mut tables = self.tables.lock().unwrap();
        if tables.len() >= MAX_TABLES {
            tables.clear();
        }
        match loaded {
            Ok(table) => {
                let table = Arc::new(table);
                tables.insert(id.to_owned(), TableSlot::Loaded(table.clone()));
                Some(table)
            }
            Err(e) => {
                warn!(source = %self.describe(), key, error = %e, "reading bulk redirects failed; requests go on without them");
                tables.insert(id.to_owned(), TableSlot::Failed(Instant::now()));
                None
            }
        }
    }

    /// The manifest, read once. While a failed read is recent, `None` without trying again.
    pub async fn manifest(&self, id: &str) -> Option<Arc<Manifest>> {
        let slot = self.manifests.lock().unwrap().get(id).cloned();
        match slot {
            Some(ManifestSlot::Loaded(manifest)) => return Some(manifest),
            Some(ManifestSlot::Failed(at)) if at.elapsed() < RETRY_FAILED_MANIFEST => return None,
            _ => {}
        }
        let key = manifest_key(id);
        let loaded = match self.fetch(&key, MAX_MANIFEST_BYTES).await {
            Ok(bytes) => serde_json::from_slice::<Manifest>(&bytes).map_err(|e| e.to_string()).and_then(|m| {
                // Every file is named by its hash: anything else would read another object of the
                // bucket's `static/` (another deployment's) as this one's.
                match m.files.iter().find(|(_, f)| !forkrail_common::statics::valid_manifest_id(&f.sha256)) {
                    Some((path, _)) => Err(format!("{path}: not named by a SHA-256")),
                    None => Ok(m),
                }
            }),
            Err(e) => Err(e),
        };
        let mut manifests = self.manifests.lock().unwrap();
        if manifests.len() >= MAX_MANIFESTS {
            manifests.clear();
        }
        match loaded {
            Ok(manifest) => {
                let manifest = Arc::new(manifest);
                manifests.insert(id.to_owned(), ManifestSlot::Loaded(manifest.clone()));
                Some(manifest)
            }
            Err(e) => {
                warn!(source = %self.describe(), key, error = %e, "reading a static manifest failed; its files go to the function");
                manifests.insert(id.to_owned(), ManifestSlot::Failed(Instant::now()));
                None
            }
        }
    }

    /// Any object in the store, read now: for configurations kept elsewhere (`firewall.rs`).
    pub async fn object(&self, key: &str) -> Result<Bytes, String> {
        self.fetch(key, MAX_OBJECT_BYTES).await
    }

    /// One of a deployment's files as it is (not compressed), from memory or the store: for the
    /// image optimizer's sources served as they are. `None` if it can't be read.
    pub async fn file_bytes(&self, file: &StaticFile) -> Option<Bytes> {
        let key = file.key(Encoding::Identity);
        if let Some(kept) = self.cache.lock().unwrap().get(&key).filter(|k| k.bytes.len() as u64 == file.size) {
            return Some(kept.bytes);
        }
        match self.fetch(&key, file.size).await {
            Ok(bytes) if bytes.len() as u64 == file.size => {
                let size = bytes.len();
                self.cache.lock().unwrap().put(key, Kept { bytes: bytes.clone(), at: SystemTime::now() }, size);
                Some(bytes)
            }
            Ok(_) => None,
            Err(e) => {
                warn!(source = %self.describe(), key, error = %e, "reading a static file failed");
                None
            }
        }
    }

    /// An object with its type and metadata, or `None` if there's none at `key`. In a directory,
    /// the metadata are in `<key>.meta.json` beside it (what the local optimizer writes). S3 says
    /// 403 for a missing key to a reader without `s3:ListBucket`, so that's none too.
    pub async fn stored(&self, key: &str) -> Result<Option<Stored>, String> {
        match &self.source {
            Source::S3 { client, bucket } => {
                crate::spend::ledger().s3(forkrail_common::prices::Meter::S3Get);
                let out = match client.get_object().bucket(bucket).key(key).send().await {
                    Ok(out) => out,
                    Err(e) => {
                        let status = e.raw_response().map(|r| r.status().as_u16());
                        if status == Some(403) {
                            // A missing key without ListBucket, or no right to read it at all:
                            // the second makes every read a miss, so it's said, once a minute.
                            refused_warning(bucket, key);
                            return Ok(None);
                        }
                        if status == Some(404) || e.as_service_error().is_some_and(|s| s.is_no_such_key()) {
                            return Ok(None);
                        }
                        return Err(aws_sdk_s3::error::DisplayErrorContext(&e).to_string());
                    }
                };
                let content_type = out.content_type().unwrap_or_default().to_owned();
                let meta = out.metadata().cloned().unwrap_or_default();
                let body = out.body.collect().await.map_err(|e| e.to_string())?;
                Ok(Some(Stored { bytes: body.into_bytes(), content_type, meta }))
            }
            Source::Dir(dir) => {
                let bytes = match tokio::fs::read(dir.join(key)).await {
                    Ok(bytes) => Bytes::from(bytes),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e.to_string()),
                };
                let mut meta: HashMap<String, String> = match tokio::fs::read(dir.join(format!("{key}.meta.json"))).await {
                    Ok(json) => serde_json::from_slice(&json).map_err(|e| format!("{key}.meta.json: {e}"))?,
                    Err(_) => HashMap::new(),
                };
                let content_type = meta.remove("content-type").unwrap_or_default();
                Ok(Some(Stored { bytes, content_type, meta }))
            }
        }
    }

    /// The object at `key`, if it's no more than `max` bytes: read whole into memory, so a larger
    /// one is refused before it's read.
    async fn fetch(&self, key: &str, max: u64) -> Result<Bytes, String> {
        let too_large = |size: u64| format!("{size} bytes, more than the {max} this may be");
        match &self.source {
            Source::S3 { client, bucket } => {
                crate::spend::ledger().s3(forkrail_common::prices::Meter::S3Get);
                let out = client
                    .get_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .map_err(|e| aws_sdk_s3::error::DisplayErrorContext(&e).to_string())?;
                let size = out.content_length().map_or(u64::MAX, |n| n.max(0) as u64);
                if size > max {
                    return Err(too_large(size));
                }
                let body = out.body.collect().await.map_err(|e| e.to_string())?;
                Ok(body.into_bytes())
            }
            Source::Dir(dir) => {
                let path = dir.join(key);
                let size = tokio::fs::metadata(&path).await.map_err(|e| e.to_string())?.len();
                if size > max {
                    return Err(too_large(size));
                }
                tokio::fs::read(path).await.map(Bytes::from).map_err(|e| e.to_string())
            }
        }
    }
}

/// A large table written to disk (once: its name is its hash, and it's renamed into place) and
/// mapped, so it doesn't sit in the router's memory. `None` if that can't be done.
fn map(id: &str, bytes: &[u8]) -> Option<TableBytes> {
    let dir = std::env::var("FORKRAIL_REDIRECTS_DIR").unwrap_or_else(|_| "/var/lib/forkrail/redirects".into());
    let dir = std::path::Path::new(&dir);
    let path = dir.join(id);
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{id}.{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)
    };
    let fits = std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64);
    if !fits && let Err(e) = write() {
        warn!(path = %path.display(), error = %e, "can't keep a large table of redirects on disk; it stays in memory");
        return None;
    }
    let file = std::fs::File::open(&path).ok()?;
    // SAFETY: the file is only ever replaced by rename, never written in place, so the mapping
    // never changes under us.
    let mapped = unsafe { memmap2::Mmap::map(&file) }.ok()?;
    Some(TableBytes::Mapped(mapped))
}

fn set(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn empty() -> ResponseBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

/// Whether the client's `If-None-Match` names any copy of the file (or `*`): what it has is
/// current, whichever encoding it got.
fn not_modified(headers: &HeaderMap, file: &StaticFile) -> bool {
    let etags = [Encoding::Identity, Encoding::Brotli, Encoding::Gzip].map(|e| file.etag(e));
    headers.get_all(IF_NONE_MATCH).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(|tag| {
        let tag = tag.trim();
        let tag = tag.strip_prefix("W/").unwrap_or(tag);
        tag == "*" || etags.iter().any(|e| e == tag)
    })
}

/// Seconds since 1970.
fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// What a `Range` header asks of a file of `size` bytes, as Vercel answers one (seen on its copies,
/// 2026-11-16): one range of bytes, `a-b`, `a-` or `-n`, spaces around the numbers allowed; its end
/// past the file is the file's end, `-n` longer than the file the whole file. A start past the
/// end, an end before the start, or `-0` can't be given (416). Anything else is no range at all,
/// the whole file: several ranges (Vercel never answers multipart), another unit, a malformed one.
/// Where Vercel's own answers are odd it isn't copied: a 206 for `bytes=N-N` on a file of N bytes,
/// with an impossible `content-range`; 416 for some malformed ones and for an end too large to be
/// a number; ranges on a 404 page.
fn byte_range(value: &str, size: u64) -> Option<Ranged> {
    let (unit, spec) = value.split_once('=')?;
    if !unit.trim().eq_ignore_ascii_case("bytes") || spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let number = |text: &str| -> Option<u64> {
        let text = text.trim();
        // Too large to be a number is past any file's end.
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse::<u64>().unwrap_or(u64::MAX))
    };
    let (first, last) = (first.trim(), last.trim());
    Some(match (first.is_empty(), last.is_empty()) {
        (true, true) => return None,
        (true, false) => match number(last)? {
            0 => Ranged::Unsatisfiable,
            _ if size == 0 => Ranged::Unsatisfiable,
            n => Ranged::Bytes(size.saturating_sub(n), size - 1),
        },
        (false, true) => match number(first)? {
            a if a >= size => Ranged::Unsatisfiable,
            a => Ranged::Bytes(a, size - 1),
        },
        (false, false) => match (number(first)?, number(last)?) {
            (a, b) if a > b || a >= size => Ranged::Unsatisfiable,
            (a, b) => Ranged::Bytes(a, b.min(size - 1)),
        },
    })
}

/// One range of a file, or none it can give.
#[derive(Debug, PartialEq, Eq)]
enum Ranged {
    /// From the first byte to the last, both included.
    Bytes(u64, u64),
    Unsatisfiable,
}

/// Whether a request's `If-Range`, if it has one, still names the file: its entity tag (weak or
/// strong, as Vercel compares them), or exactly its `last-modified` (`modified`). Vercel serves the
/// range for any date; that would give a resumed download the bytes of another deployment's file
/// at the same path, so a date has to match here (RFC 9110), and otherwise the whole file goes.
fn if_range_holds(headers: &HeaderMap, etag: &str, modified: u64) -> bool {
    let Some(value) = headers.get(IF_RANGE) else { return true };
    let Ok(value) = value.to_str() else { return false };
    let value = value.trim();
    if value.starts_with('"') || value.starts_with("W/") {
        return value.strip_prefix("W/").unwrap_or(value) == etag.strip_prefix("W/").unwrap_or(etag);
    }
    parse_http_date(value) == Some(modified)
}

/// An HTTP date (`Mon, 16 Nov 2026 05:37:41 GMT`, the form every client sends) as Unix seconds.
fn parse_http_date(text: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut parts = text.split_ascii_whitespace();
    let (_weekday, day, month, year, time, zone) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if zone != "GMT" || parts.next().is_some() {
        return None;
    }
    let day: i64 = day.parse().ok()?;
    let month = MONTHS.iter().position(|m| *m == month)? as i64 + 1;
    let year: i64 = year.parse().ok()?;
    let mut hms = time.split(':').map(|n| n.parse::<i64>().ok());
    let (h, m, s) = (hms.next()??, hms.next()??, hms.next()??);
    if !(1..=31).contains(&day) || !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..61).contains(&s) || year < 1970 {
        return None;
    }
    // Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + m * 60 + s).ok()
}

/// Warn that S3 refused a read, at most once a minute.
fn refused_warning(bucket: &str, key: &str) {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let last = LAST.load(std::sync::atomic::Ordering::Relaxed);
    if now >= last + 60 && LAST.compare_exchange(last, now, std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed).is_ok() {
        warn!(bucket, key, "S3 says 403 for a kept object: if the router's role can't read it (s3:GetObject) or list the bucket (s3:ListBucket), every read is a miss");
    }
}

/// An object as stored: its bytes, `Content-Type` and metadata (S3's `x-amz-meta-*`, by name
/// without the prefix).
pub struct Stored {
    pub bytes: Bytes,
    pub content_type: String,
    pub meta: HashMap<String, String>,
}

/// Values by key, least recently used out first, within `max` bytes of what they hold.
pub(crate) struct Lru<V> {
    max: usize,
    used: usize,
    tick: u64,
    entries: HashMap<String, (V, usize, u64)>,
}

impl<V: Clone> Lru<V> {
    pub(crate) fn new(max: usize) -> Self {
        Self { max, used: 0, tick: 0, entries: HashMap::new() }
    }

    pub(crate) fn get(&mut self, key: &str) -> Option<V> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(key).map(|(value, _, used)| {
            *used = tick;
            value.clone()
        })
    }

    /// Keep `value`, which holds `size` bytes.
    pub(crate) fn put(&mut self, key: String, value: V, size: usize) {
        if size > self.max / MAX_CACHED_SHARE {
            return;
        }
        self.tick += 1;
        self.used += size;
        if let Some((_, old, _)) = self.entries.insert(key, (value, size, self.tick)) {
            self.used -= old;
        }
        while self.used > self.max {
            let Some(oldest) = self.entries.iter().min_by_key(|(_, (_, _, used))| *used).map(|(k, _)| k.clone()) else { break };
            if let Some((_, size, _)) = self.entries.remove(&oldest) {
                self.used -= size;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forkrail_common::statics::REVALIDATE;

    #[test]
    fn etag_matching() {
        let file = StaticFile {
            sha256: "ab".repeat(32),
            size: 10,
            content_type: "text/css".into(),
            cache_control: REVALIDATE.into(),
            br: Some(4),
            gz: None,
        };
        let mut headers = HeaderMap::new();
        assert!(!not_modified(&headers, &file));
        headers.insert(IF_NONE_MATCH, HeaderValue::from_str(&format!("\"x\", W/{}", file.etag(Encoding::Brotli))).unwrap());
        assert!(not_modified(&headers, &file));
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        assert!(!not_modified(&headers, &file));
    }

    #[test]
    fn byte_ranges_as_vercel_answers_them() {
        let r = |v: &str| byte_range(v, 1342);
        assert_eq!(r("bytes=0-9"), Some(Ranged::Bytes(0, 9)));
        assert_eq!(r("bytes=5-"), Some(Ranged::Bytes(5, 1341)));
        assert_eq!(r("bytes=-5"), Some(Ranged::Bytes(1337, 1341)));
        assert_eq!(r("bytes=1330-2000"), Some(Ranged::Bytes(1330, 1341)));
        assert_eq!(r("bytes=-2000"), Some(Ranged::Bytes(0, 1341)));
        assert_eq!(r("bytes=0-0"), Some(Ranged::Bytes(0, 0)));
        assert_eq!(r("bytes=01-09"), Some(Ranged::Bytes(1, 9)));
        assert_eq!(r("bytes= 0-9"), Some(Ranged::Bytes(0, 9)));
        assert_eq!(r("bytes =0-9"), Some(Ranged::Bytes(0, 9)));
        assert_eq!(r("bytes=0 - 9"), Some(Ranged::Bytes(0, 9)));
        assert_eq!(r("bytes=0-9 "), Some(Ranged::Bytes(0, 9)));
        // Outside the file, backwards, nothing: 416.
        for v in ["bytes=1342-", "bytes=9-5", "bytes=-0", "bytes=1342-1342", "bytes=99999999999999999999-"] {
            assert_eq!(r(v), Some(Ranged::Unsatisfiable), "{v}");
        }
        // Not one range of bytes: the whole file.
        for v in ["bytes=0-1,3-4", "bytes=0-9,", "bytes=,0-9", "bytes=abc", "bytes=", "bytes", "bytes=0-9x", "bytes=+1-9", "items=0-9", "bytes=-"] {
            assert_eq!(r(v), None, "{v}");
        }
        assert_eq!(byte_range("bytes=0-", 0), Some(Ranged::Unsatisfiable));
        assert_eq!(byte_range("bytes=-1", 0), Some(Ranged::Unsatisfiable));
    }

    #[test]
    fn if_range_and_dates() {
        let etag = "\"3b1afddfb0ac8ae0a66bf0ae2061c432\"";
        let at = parse_http_date("Mon, 16 Nov 2026 15:42:30 GMT").unwrap();
        assert_eq!(crate::images::http_date(at), "Mon, 16 Nov 2026 15:42:30 GMT");
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Sun, 29 Feb 2032 23:59:59 GMT").map(crate::images::http_date).as_deref(), Some("Sun, 29 Feb 2032 23:59:59 GMT"));
        assert_eq!(parse_http_date("Monday, 16-Nov-26 15:42:30 GMT"), None);
        assert_eq!(parse_http_date("garbage"), None);
        let with = |value: &str| {
            let mut h = HeaderMap::new();
            h.insert(IF_RANGE, HeaderValue::from_str(value).unwrap());
            if_range_holds(&h, etag, at)
        };
        assert!(if_range_holds(&HeaderMap::new(), etag, at));
        assert!(with(etag) && with(&format!("W/{etag}")));
        assert!(!with("\"nope\"") && !with("W/\"nope\"") && !with(&format!("{etag}, \"x\"")));
        assert!(with("Mon, 16 Nov 2026 15:42:30 GMT"));
        assert!(!with("Mon, 16 Nov 2026 15:00:00 GMT") && !with("garbage"));
    }

    #[test]
    fn files_answer_as_vercel_cdn_answers_them() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = std::env::temp_dir().join(format!("forkrail-statics-{}", std::process::id()));
            let body: Vec<u8> = (0..100u8).collect();
            let sha = "cd".repeat(32);
            let file = StaticFile { sha256: sha.clone(), size: 100, content_type: "application/javascript".into(), cache_control: REVALIDATE.into(), br: Some(40), gz: None };
            std::fs::create_dir_all(dir.join("static")).unwrap();
            std::fs::write(dir.join(file.key(Encoding::Identity)), &body).unwrap();
            std::fs::write(dir.join(file.key(Encoding::Brotli)), vec![7u8; 40]).unwrap();
            let statics = Statics::new(Source::Dir(dir.clone()), 1 << 20);
            let ask = |pairs: &[(&'static str, &str)]| {
                let mut h = HeaderMap::new();
                for (k, v) in pairs {
                    h.insert(HeaderName::from_static(k), HeaderValue::from_str(v).unwrap());
                }
                h
            };
            let get = |h: HeaderMap, method: Method, status: Option<StatusCode>| {
                let statics = &statics;
                let file = file.clone();
                async move {
                    let served = statics.respond(&file, &method, &h, status, Serve::File, |_| async {}).await.unwrap();
                    let (parts, body) = served.response.into_parts();
                    (parts.status.as_u16(), parts.headers, body.collect().await.unwrap().to_bytes())
                }
            };
            // First: from S3 (here the folder), MISS, age 0; then from memory, HIT.
            let (status, h, b) = get(HeaderMap::new(), Method::GET, None).await;
            assert_eq!((status, b.len()), (200, 100));
            assert_eq!((&h["x-vercel-cache"], &h["age"], &h["accept-ranges"], &h["access-control-allow-origin"]), (&"MISS".parse::<HeaderValue>().unwrap(), &HeaderValue::from(0), &HeaderValue::from_static("bytes"), &HeaderValue::from_static("*")));
            let modified = h["last-modified"].to_str().unwrap().to_owned();
            let (_, h, _) = get(HeaderMap::new(), Method::GET, None).await;
            assert_eq!((h["x-vercel-cache"].to_str().unwrap(), h["last-modified"].to_str().unwrap()), ("HIT", modified.as_str()));
            // Compressed: no ranges offered.
            let (_, h, b) = get(ask(&[("accept-encoding", "br")]), Method::GET, None).await;
            assert_eq!((h.get("accept-ranges"), h["content-encoding"].to_str().unwrap(), b.len()), (None, "br", 40));
            // A range: 206 of the file as it is, whatever the client takes.
            let (status, h, b) = get(ask(&[("range", "bytes=10-19"), ("accept-encoding", "br")]), Method::GET, None).await;
            assert_eq!((status, h["content-range"].to_str().unwrap(), h.get("content-encoding"), &b[..]), (206, "bytes 10-19/100", None, &body[10..20]));
            assert_eq!(h["content-length"], HeaderValue::from(10));
            // HEAD: the same headers, no body.
            let (status, h, b) = get(ask(&[("range", "bytes=-5")]), Method::HEAD, None).await;
            assert_eq!((status, h["content-length"].to_str().unwrap(), b.len()), (206, "5", 0));
            // Outside: 416, Vercel's error, the file's caching headers but no ranges offered.
            let (status, h, b) = get(ask(&[("range", "bytes=100-")]), Method::GET, None).await;
            assert_eq!((status, h["content-range"].to_str().unwrap(), &b[..]), (416, "bytes */100", &b"An error occurred\n\nRANGE_NOT_SATISFIABLE\n"[..]));
            assert_eq!((h["x-forkrail-error"].to_str().unwrap(), h.get("accept-ranges"), h["x-vercel-cache"].to_str().unwrap()), ("RANGE_NOT_SATISFIABLE", None, "HIT"));
            // Several ranges, or an If-Range that names another file: the whole file.
            assert_eq!(get(ask(&[("range", "bytes=0-1,3-4")]), Method::GET, None).await.0, 200);
            assert_eq!(get(ask(&[("range", "bytes=0-1"), ("if-range", "\"other\"")]), Method::GET, None).await.0, 200);
            assert_eq!(get(ask(&[("range", "bytes=0-1"), ("if-range", &file.etag(Encoding::Identity))]), Method::GET, None).await.0, 206);
            assert_eq!(get(ask(&[("range", "bytes=0-1"), ("if-range", &modified)]), Method::GET, None).await.0, 206);
            // 304 by entity tag, or by date without one: Vercel's three headers.
            let (status, h, _) = get(ask(&[("if-none-match", &file.etag(Encoding::Identity))]), Method::GET, None).await;
            assert_eq!(status, 304);
            let mut names: Vec<&str> = h.keys().map(|k| k.as_str()).collect();
            names.sort();
            assert_eq!(names, ["cache-control", "etag", "vary", "x-vercel-cache"]);
            assert_eq!(get(ask(&[("if-modified-since", &modified)]), Method::GET, None).await.0, 304);
            assert_eq!(get(ask(&[("if-modified-since", "Thu, 01 Jan 1970 00:00:00 GMT")]), Method::GET, None).await.0, 200);
            assert_eq!(get(ask(&[("if-none-match", "\"x\""), ("if-modified-since", "Mon, 01 Jan 2124 00:00:00 GMT")]), Method::GET, None).await.0, 200);
            // A page with a status: the headers, but never a range or a 304.
            let (status, h, b) = get(ask(&[("range", "bytes=0-1"), ("if-none-match", "*")]), Method::GET, Some(StatusCode::NOT_FOUND)).await;
            assert_eq!((status, b.len(), h["access-control-allow-origin"].to_str().unwrap()), (404, 100, "*"));
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn lru_evicts_least_recent() {
        let mut lru = Lru::new(80);
        lru.put("a".into(), Bytes::from(vec![0; 10]), 10);
        lru.put("b".into(), Bytes::from(vec![0; 10]), 10);
        // Over a share of the cache: served, never kept.
        lru.put("big".into(), Bytes::from(vec![0; 11]), 11);
        assert!(lru.get("big").is_none());
        assert!(lru.get("a").is_some());
        for k in ["c", "d", "e", "f", "g", "h", "i"] {
            lru.put(k.into(), Bytes::from(vec![0; 10]), 10);
        }
        assert!(lru.used <= 80);
        // `a` was used after `b`, so `b` went first.
        assert!(lru.get("b").is_none());
        assert!(lru.get("a").is_some() && lru.get("i").is_some());
    }
}
