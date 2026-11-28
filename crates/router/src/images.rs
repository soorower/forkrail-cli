//! Image Optimization (`/_vercel/image`) on the router: a deployment with `images` settings
//! (`Deployment.images`, see `forkrail_common::images` for what a request may ask) answers its
//! optimizer path itself, from a cache, and has the `forkrail-image` function resize on a miss.
//! Resizing never runs here: the routers are small boxes serving real traffic.
//!
//! **What's cached, and under what name.** Vercel keeps an optimized image per project (a redeploy
//! doesn't empty the cache), and names a local image by its content (vercel.com/docs/image-
//! optimization). So here: a deployment's static file is named by its SHA-256 (from the manifest,
//! without reading it), shared by every deployment of the project that has the same file, and
//! never stale; an image from a site elsewhere by its URL, per project, kept for the longer of its
//! own `max-age` and `minimumCacheTTL`; one a function answers by its path and query, per
//! deployment (its code may answer differently in the next), kept the same way. Each also by
//! width, quality and output format. The optimized image is kept once for the whole install, in
//! the home bucket (`images/<project>/<id>` in the routing table's bucket, `FORKRAIL_STATE_REGION`;
//! written by the optimizer of whichever region made it, read here), as Vercel keeps one "shared
//! global cache" of optimized images, and the most used in each router's memory
//! (`FORKRAIL_IMAGE_CACHE_MB`, default 64). A router in another region reads it across regions
//! ($0.02/GB, a memory miss only). S3 deletes one 31 days after it was made (the bucket's
//! lifecycle rule, `deploy/aws/image.sh`), and the next request makes it again. A stale one is
//! served once more (`x-forkrail-cache: STALE`) while it's made again in the background, as
//! Vercel does.
//!
//! **How the optimizer is called.** A direct Lambda Invoke of `FORKRAIL_IMAGE_FUNCTION` (default
//! `forkrail-image`) in the router's region, or for local runs an HTTP POST of the same JSON to
//! `FORKRAIL_IMAGE_URL` (`deploy/image/index.mjs serve`). A static file goes by its key (the
//! function reads it from the same bucket); a function's answer goes in the request, up to
//! `MAX_INLINE_SOURCE`; a URL elsewhere the function fetches itself, behind its address guard.
//! Requests for the same image at once share one call.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use bytes::Bytes;
use forkrail_common::images::{Compiled, ImagesConfig};
use forkrail_common::statics::{image_result_key, images_key};
use http_body_util::{BodyExt, Empty, Full};
use hyper::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue, IF_NONE_MATCH, LAST_MODIFIED, VARY};
use hyper::{HeaderMap, Method, Response, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::OnceCell;
use tracing::warn;

use crate::proxy::ResponseBody;
use crate::statics::{CACHE_HEADER, Lru, Statics};

/// The router's error code header, as on every response it makes itself.
const ERROR_HEADER: HeaderName = HeaderName::from_static("x-forkrail-error");
const ERROR_MESSAGE_HEADER: HeaderName = HeaderName::from_static("x-forkrail-error-message");
const AGE: HeaderName = HeaderName::from_static("age");
/// Larger answers from a function aren't sent to the optimizer: an Invoke's request is at most
/// 6 MB, and base64 makes 4.5 MB of it.
pub const MAX_INLINE_SOURCE: usize = 4 << 20;
/// How long the optimizer may take: a fetch elsewhere (10 s) and the resize (7 s), with room.
const OPTIMIZER_TIMEOUT: Duration = Duration::from_secs(30);
/// After settings fail to load, requests are refused for this long before they're tried again.
const RETRY_FAILED: Duration = Duration::from_secs(30);
/// Settings kept; past it, all are dropped and read again as needed.
const MAX_CONFIGS: usize = 512;
/// The version of how results are named: a change here starts every cache afresh.
const KEY_VERSION: &str = "1";

pub struct Images {
    /// The region's bucket: deployments' image settings (`static/images/`) and files.
    store: Arc<Statics>,
    /// The install's one store of optimized images (`images/`), in the home bucket.
    kept: Arc<Statics>,
    configs: Mutex<HashMap<String, ConfigSlot>>,
    memory: Mutex<Lru<Arc<Optimized>>>,
    /// Misses being made now, by result key: later requests for the same wait for the first.
    flights: Mutex<HashMap<String, Arc<OnceCell<Result<Arc<Optimized>, Failure>>>>>,
    optimizer: Optimizer,
}

#[derive(Clone)]
enum ConfigSlot {
    Loaded(Arc<Compiled>),
    Failed(Instant),
}

/// Where the resizing happens.
pub enum Optimizer {
    Lambda { client: aws_sdk_lambda::Client, function: String },
    Http { url: hyper::Uri, client: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> },
}

/// An optimized image (or a source served as it is), as kept.
#[derive(Debug)]
pub struct Optimized {
    pub bytes: Bytes,
    pub content_type: String,
    /// The answer's `Cache-Control` max-age: a deployment's file's own, or for an image fetched (a
    /// function's, one elsewhere) the longer of its own and `minimumCacheTTL`, as Vercel answers.
    pub max_age: u64,
    /// Whether the answer says `must-revalidate`: a deployment's file's does, a fetched one's
    /// doesn't.
    pub revalidate: bool,
    /// A deployment's file whose own `Cache-Control` says `immutable`: the answer says
    /// `immutable` in place of `must-revalidate`, as Vercel's does.
    pub immutable: bool,
    /// When it's stale (Unix seconds); 0 for never (a static file's, named by its content).
    pub expires: u64,
    /// When it was made (Unix seconds): its `Last-Modified`, and `Age` from there.
    pub created: u64,
}

impl Optimized {
    pub fn fresh(&self, now: u64) -> bool {
        self.expires == 0 || now < self.expires
    }
}

/// Why an image couldn't be served, as the answer says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The source isn't an image (or is an SVG the settings don't allow): 400.
    NotAnImage,
    /// The source answered with an error, or isn't there: 404.
    BadUpstream,
    /// Anything else, with its status, code and message.
    Other { status: u16, code: String, message: String },
}

impl Failure {
    pub fn other(status: StatusCode, code: &str, message: &str) -> Self {
        Failure::Other { status: status.as_u16(), code: code.to_owned(), message: message.to_owned() }
    }
}

/// What the optimizer is asked to do.
pub struct Job {
    pub source: JobSource,
    /// The source's `Content-Type` and `Cache-Control`, when the router has them.
    pub content_type: Option<String>,
    pub cache_control: Option<String>,
    pub width: u32,
    pub quality: u32,
    /// The output type `Accept` chose; `None` keeps the source's.
    pub format: Option<String>,
    pub svg: bool,
    pub minimum_cache_ttl: u64,
    /// Named by its content, so never stale.
    pub immutable: bool,
    /// Fetched (a function's answer, one elsewhere, a file by a path written another way), which
    /// Vercel answers with `minimumCacheTTL` in `Cache-Control`.
    pub fetched: bool,
    /// Where the optimizer keeps the result.
    pub store: String,
    /// The loop guard's mark, for a URL that is one of the install's own hosts: the optimizer's
    /// fetch of it is the chain's next call (`loops.rs`).
    pub loop_mark: Option<String>,
    /// Whose image it is: the optimizer's invocation is on their spend (`spend.rs`).
    pub owner: crate::spend::Owner,
}

pub enum JobSource {
    /// An object in the bucket (a static file).
    Key(String),
    /// A URL elsewhere, for the optimizer to fetch.
    Url(String),
    /// The bytes themselves (a function's answer).
    Body(Bytes),
}

/// The optimizer's answer.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Answer {
    ok: bool,
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    max_age: u64,
    #[serde(default = "yes")]
    revalidate: bool,
    #[serde(default)]
    immutable: bool,
    #[serde(default)]
    expires: u64,
    #[serde(default)]
    created: u64,
    /// The image in base64, when it's small enough to send back; otherwise it's only in the store.
    #[serde(default)]
    body: Option<String>,
}

fn yes() -> bool {
    true
}

impl Images {
    pub fn new(store: Arc<Statics>, kept: Arc<Statics>, optimizer: Optimizer, memory_bytes: usize) -> Self {
        Self { store, kept, configs: Mutex::new(HashMap::new()), memory: Mutex::new(Lru::new(memory_bytes)), flights: Mutex::new(HashMap::new()), optimizer }
    }

    /// A deployment's settings, compiled once (they never change: they're named by content).
    pub async fn config(&self, id: &str) -> Option<Arc<Compiled>> {
        match self.configs.lock().unwrap().get(id).cloned() {
            Some(ConfigSlot::Loaded(c)) => return Some(c),
            Some(ConfigSlot::Failed(at)) if at.elapsed() < RETRY_FAILED => return None,
            _ => {}
        }
        let key = images_key(id);
        let loaded = match self.store.object(&key).await {
            Ok(bytes) => serde_json::from_slice::<ImagesConfig>(&bytes).map_err(|e| e.to_string()).and_then(Compiled::new),
            Err(e) => Err(e),
        };
        let mut configs = self.configs.lock().unwrap();
        if configs.len() >= MAX_CONFIGS {
            configs.clear();
        }
        match loaded {
            Ok(c) => {
                let c = Arc::new(c);
                configs.insert(id.to_owned(), ConfigSlot::Loaded(c.clone()));
                Some(c)
            }
            Err(e) => {
                warn!(key, error = %e, "reading a deployment's image settings failed");
                configs.insert(id.to_owned(), ConfigSlot::Failed(Instant::now()));
                None
            }
        }
    }

    /// A kept image: from memory, or the store.
    pub async fn cached(&self, key: &str) -> Option<Arc<Optimized>> {
        if let Some(hit) = self.memory.lock().unwrap().get(key) {
            return Some(hit);
        }
        let stored = match self.kept.stored(key).await {
            Ok(Some(stored)) => stored,
            Ok(None) => return None,
            Err(e) => {
                warn!(key, error = %e, "reading a kept image failed; making it again");
                return None;
            }
        };
        let num = |name: &str| stored.meta.get(name).and_then(|v| v.parse::<u64>().ok());
        let (Some(created), Some(expires)) = (num("forkrail-created"), num("forkrail-expires")) else {
            warn!(key, "a kept image has no metadata; making it again");
            return None;
        };
        let found = Arc::new(Optimized {
            content_type: stored.content_type,
            max_age: num("forkrail-max-age").unwrap_or(0),
            revalidate: num("forkrail-revalidate") != Some(0),
            immutable: num("forkrail-immutable") == Some(1),
            expires,
            created,
            bytes: stored.bytes,
        });
        self.memory.lock().unwrap().put(key.to_owned(), found.clone(), found.bytes.len());
        Some(found)
    }

    /// Make an image once for everyone asking for it at the same time: the first request's `make`
    /// runs, the others wait for its result. A request whose client leaves hands the work to the
    /// next one waiting.
    pub async fn once<F, Fut>(&self, key: &str, make: F) -> Result<Arc<Optimized>, Failure>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Arc<Optimized>, Failure>>,
    {
        let cell = self.flights.lock().unwrap().entry(key.to_owned()).or_default().clone();
        let result = cell.get_or_init(make).await.clone();
        // Done: the next request asks again (after a failure, or once this is stale).
        let mut flights = self.flights.lock().unwrap();
        if flights.get(key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            flights.remove(key);
        }
        result
    }

    /// Have the optimizer make an image, and keep it in memory.
    pub async fn optimize(&self, job: Job) -> Result<Arc<Optimized>, Failure> {
        let store = job.store.clone();
        let event = event(&job);
        let started = Instant::now();
        let raw = match &self.optimizer {
            Optimizer::Lambda { client, function } => {
                let _spent = crate::spend::ledger().invocation(job.owner.clone(), optimizer_memory_mb(), false);
                let call = client.invoke().function_name(function).payload(aws_sdk_lambda::primitives::Blob::new(event)).send();
                match tokio::time::timeout(OPTIMIZER_TIMEOUT, call).await {
                    Ok(Ok(out)) if out.function_error().is_none() => out.payload().map(|p| p.as_ref().to_vec()).unwrap_or_default(),
                    Ok(Ok(out)) => {
                        let detail = out.payload().map(|p| String::from_utf8_lossy(p.as_ref()).chars().take(300).collect::<String>()).unwrap_or_default();
                        warn!(function, error = out.function_error().unwrap_or_default(), detail, "the image optimizer failed");
                        return Err(unavailable());
                    }
                    Ok(Err(e)) => {
                        warn!(function, error = %aws_sdk_lambda::error::DisplayErrorContext(&e), "invoking the image optimizer failed");
                        return Err(unavailable());
                    }
                    Err(_) => {
                        warn!(function, "the image optimizer took too long");
                        return Err(unavailable());
                    }
                }
            }
            Optimizer::Http { url, client } => {
                let request = hyper::Request::post(url.clone()).header(CONTENT_TYPE, "application/json").body(Full::new(Bytes::from(event))).expect("a valid request");
                let sent = async {
                    let response = client.request(request).await.map_err(|e| e.to_string())?;
                    response.into_body().collect().await.map(|b| b.to_bytes().to_vec()).map_err(|e| e.to_string())
                };
                match tokio::time::timeout(OPTIMIZER_TIMEOUT, sent).await {
                    Ok(Ok(body)) => body,
                    Ok(Err(e)) => {
                        warn!(%url, error = %e, "the local image optimizer failed");
                        return Err(unavailable());
                    }
                    Err(_) => return Err(unavailable()),
                }
            }
        };
        let answer: Answer = match serde_json::from_slice(&raw) {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, "the image optimizer's answer isn't what it should be");
                return Err(unavailable());
            }
        };
        if !answer.ok {
            let code = answer.code.unwrap_or_default();
            return Err(match code.as_str() {
                "NOT_AN_IMAGE" => Failure::NotAnImage,
                "BAD_UPSTREAM" => Failure::BadUpstream,
                _ => {
                    let status = answer.status.and_then(|s| StatusCode::from_u16(s).ok()).unwrap_or(StatusCode::BAD_GATEWAY);
                    Failure::other(status, &code, answer.message.as_deref().unwrap_or("The image couldn't be optimized."))
                }
            });
        }
        let bytes = match answer.body {
            Some(b64) => match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(bytes) => Bytes::from(bytes),
                Err(_) => return Err(unavailable()),
            },
            // Too big to send back: it's in the store.
            None => match self.kept.stored(&store).await {
                Ok(Some(stored)) => stored.bytes,
                _ => {
                    warn!(key = store, "the image optimizer kept an image the router can't read");
                    return Err(unavailable());
                }
            },
        };
        let made = Arc::new(Optimized {
            bytes,
            content_type: answer.content_type.unwrap_or_else(|| "application/octet-stream".into()),
            max_age: answer.max_age,
            revalidate: answer.revalidate,
            immutable: answer.immutable,
            expires: answer.expires,
            created: answer.created,
        });
        tracing::debug!(key = store, ms = started.elapsed().as_millis() as u64, bytes = made.bytes.len(), "image optimized");
        self.memory.lock().unwrap().put(store, made.clone(), made.bytes.len());
        Ok(made)
    }
}

/// The optimizer's memory, as `deploy/install/image.yaml` sets it (`FORKRAIL_IMAGE_MEMORY_MB`,
/// default 2048): what its invocations are billed by.
fn optimizer_memory_mb() -> u32 {
    static MB: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MB.get_or_init(|| std::env::var("FORKRAIL_IMAGE_MEMORY_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(2048))
}

fn unavailable() -> Failure {
    Failure::other(StatusCode::BAD_GATEWAY, "ROUTER_IMAGE_OPTIMIZER_FAILED", "The image couldn't be optimized.")
}

/// The optimizer's request, as JSON (see `deploy/image/index.mjs`).
fn event(job: &Job) -> Vec<u8> {
    let source = match &job.source {
        JobSource::Key(key) => serde_json::json!({ "key": key }),
        JobSource::Url(url) => serde_json::json!({ "url": url }),
        JobSource::Body(bytes) => serde_json::json!({ "body": base64::engine::general_purpose::STANDARD.encode(bytes) }),
    };
    serde_json::to_vec(&serde_json::json!({
        "source": source,
        "contentType": job.content_type,
        "cacheControl": job.cache_control,
        "width": job.width,
        "quality": job.quality,
        "format": job.format,
        "svg": job.svg,
        "minimumCacheTTL": job.minimum_cache_ttl,
        "immutable": job.immutable,
        "fetched": job.fetched,
        "store": job.store,
        "loop": job.loop_mark,
    }))
    .expect("the event serializes")
}

/// Where a result is kept, from everything that decides it: whose it is (`scope`: the project, or
/// for a function's image the deployment too), the source (a file's SHA-256 or a URL), width,
/// quality and output format.
pub fn result_key(project: &str, scope: &str, source: &str, width: u32, quality: u32, format: Option<&str>) -> String {
    let named = format!("{KEY_VERSION}\n{scope}\n{source}\n{width}\n{quality}\n{}", format.unwrap_or("auto"));
    image_result_key(project, &forkrail_common::hex(&Sha256::digest(named.as_bytes()))[..40])
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn body(bytes: Bytes) -> ResponseBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

fn empty() -> ResponseBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed()
}

fn set(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

/// One of Vercel's error answers: text with the code under the message, and the code in a header.
fn coded(status: StatusCode, code: &str, message: &str) -> Response<ResponseBody> {
    let mut response = Response::new(body(Bytes::from(format!("{message}\n\n{code}\n"))));
    *response.status_mut() = status;
    let h = response.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    h.insert(CACHE_CONTROL, HeaderValue::from_static("public, max-age=0, must-revalidate"));
    set(h, ERROR_HEADER, code);
    response
}

/// A query Vercel wouldn't take.
pub fn invalid() -> Response<ResponseBody> {
    coded(StatusCode::BAD_REQUEST, "INVALID_IMAGE_OPTIMIZE_REQUEST", "Bad request")
}

/// Anything but GET, HEAD and OPTIONS.
pub fn method_not_allowed() -> Response<ResponseBody> {
    coded(StatusCode::METHOD_NOT_ALLOWED, "INVALID_REQUEST_METHOD", "Method not allowed")
}

/// What a request whose source was looked at gets when it can't be served: as Vercel answers each,
/// with the source's own route headers (Vercel's answer for `/api/hello` carried `vercel.json`'s
/// header for `/api/(.*)`).
pub fn failed(failure: &Failure, config: &Compiled, stem: &str, source_headers: &[(HeaderName, HeaderValue)]) -> Response<ResponseBody> {
    let csp = config.config.content_security_policy.as_str();
    let mut response = match failure {
        Failure::NotAnImage => {
            let mut r = invalid();
            set(r.headers_mut(), HeaderName::from_static("content-security-policy"), csp);
            r
        }
        Failure::BadUpstream => {
            let mut r = coded(StatusCode::NOT_FOUND, "NOT_FOUND", "The page could not be found");
            let h = r.headers_mut();
            h.insert(VARY, HeaderValue::from_static("Accept"));
            set(h, HeaderName::from_static("content-disposition"), &config.disposition(stem, None));
            set(h, HeaderName::from_static("content-security-policy"), csp);
            h.insert(ERROR_MESSAGE_HEADER, HeaderValue::from_static("Bad status code from upstream image"));
            r
        }
        Failure::Other { status, code, message } => {
            let mut r = coded(StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY), code, message);
            r.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            r
        }
    };
    response.headers_mut().insert(CACHE_HEADER, HeaderValue::from_static("MISS"));
    for (name, value) in source_headers {
        if !response.headers().contains_key(name) {
            response.headers_mut().insert(name.clone(), value.clone());
        }
    }
    response
}

/// The answer with an image: Vercel's headers, `state` (`HIT`, `MISS`, `STALE`) in
/// `x-forkrail-cache`. `If-None-Match: *` gets 304, as Vercel's cache answers it; there's no
/// `ETag` to match otherwise, as on Vercel.
pub fn respond(image: &Optimized, state: &'static str, config: &Compiled, stem: &str, method: &Method, request: &HeaderMap) -> (Response<ResponseBody>, u64) {
    let mut headers = HeaderMap::new();
    let revalidate = match (image.immutable, image.revalidate) {
        (true, _) => ", immutable",
        (false, true) => ", must-revalidate",
        (false, false) => "",
    };
    set(&mut headers, CACHE_CONTROL, &format!("public, max-age={}{revalidate}", image.max_age));
    headers.insert(VARY, HeaderValue::from_static("Accept"));
    headers.insert(CACHE_HEADER, HeaderValue::from_static(state));
    let not_modified = request.get_all(IF_NONE_MATCH).iter().filter_map(|v| v.to_str().ok()).any(|v| v.split(',').any(|t| t.trim() == "*"));
    if not_modified {
        let mut response = Response::new(empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        *response.headers_mut() = headers;
        return (response, 0);
    }
    set(&mut headers, CONTENT_TYPE, &image.content_type);
    set(&mut headers, HeaderName::from_static("content-disposition"), &config.disposition(stem, Some(&image.content_type)));
    set(&mut headers, HeaderName::from_static("content-security-policy"), &config.config.content_security_policy);
    headers.insert(HeaderName::from_static("access-control-allow-origin"), HeaderValue::from_static("*"));
    set(&mut headers, LAST_MODIFIED, &http_date(image.created));
    let age = if state == "MISS" { 0 } else { now().saturating_sub(image.created) };
    headers.insert(AGE, HeaderValue::from(age));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(image.bytes.len()));
    let head = *method == Method::HEAD;
    let mut response = Response::new(if head { empty() } else { body(image.bytes.clone()) });
    *response.headers_mut() = headers;
    (response, if head { 0 } else { image.bytes.len() as u64 })
}

/// Unix seconds as an HTTP date (`Mon, 16 Nov 2026 05:37:41 GMT`).
pub fn http_date(secs: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        MONTHS[(month - 1) as usize],
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use forkrail_common::images::parse;

    fn config() -> Compiled {
        Compiled::new(parse(&serde_json::json!({"sizes": [640, 1080], "domains": [], "formats": ["image/webp"], "minimumCacheTTL": 60})).unwrap()).unwrap()
    }

    #[test]
    fn dates() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_794_807_461), "Mon, 16 Nov 2026 05:37:41 GMT");
        assert_eq!(http_date(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
    }

    #[test]
    fn keys_name_everything_that_decides_the_image() {
        let a = result_key("shop", "shop", "ab", 640, 75, Some("image/webp"));
        assert!(a.starts_with("images/shop/") && a.len() == "images/shop/".len() + 40);
        assert_ne!(a, result_key("shop", "shop", "ab", 640, 75, None));
        assert_ne!(a, result_key("shop", "shop", "ab", 1080, 75, Some("image/webp")));
        assert_ne!(a, result_key("shop", "shop", "ab", 640, 76, Some("image/webp")));
        assert_ne!(a, result_key("shop", "shop dpl1", "ab", 640, 75, Some("image/webp")));
        assert_eq!(a, result_key("shop", "shop", "ab", 640, 75, Some("image/webp")));
    }

    #[test]
    fn answers_as_vercel_words_them() {
        let c = config();
        let image = Optimized { bytes: Bytes::from_static(b"RIFF"), content_type: "image/webp".into(), max_age: 0, revalidate: true, immutable: false, expires: 0, created: 1_794_807_461 };
        let (r, sent) = respond(&image, "HIT", &c, "photo", &Method::GET, &HeaderMap::new());
        let h = r.headers();
        assert_eq!(r.status(), 200);
        assert_eq!(sent, 4);
        assert_eq!(h["cache-control"], "public, max-age=0, must-revalidate");
        assert_eq!(h["content-disposition"], "inline; filename=\"photo.webp\"");
        assert_eq!(h["content-security-policy"], "script-src 'none'; sandbox;");
        assert_eq!(h["vary"], "Accept");
        assert_eq!(h["last-modified"], "Mon, 16 Nov 2026 05:37:41 GMT");
        let mut ask = HeaderMap::new();
        ask.insert(IF_NONE_MATCH, HeaderValue::from_static("*"));
        let (r, _) = respond(&image, "HIT", &c, "photo", &Method::GET, &ask);
        assert_eq!(r.status(), 304);
        assert!(r.headers().get(CONTENT_TYPE).is_none());
        let (r, sent) = respond(&image, "MISS", &c, "photo", &Method::HEAD, &HeaderMap::new());
        assert_eq!((r.status().as_u16(), sent, &r.headers()["content-length"]), (200, 0, &HeaderValue::from(4)));
        let fetched = Optimized { max_age: 60, revalidate: false, ..image };
        // A framework's hashed file (Astro's `/_astro/*`): `immutable`, as Vercel answers it.
        let hashed = Optimized { bytes: Bytes::from_static(b"RIFF"), content_type: "image/webp".into(), max_age: 31_536_000, revalidate: true, immutable: true, expires: 0, created: 0 };
        assert_eq!(respond(&hashed, "HIT", &c, "photo", &Method::GET, &HeaderMap::new()).0.headers()["cache-control"], "public, max-age=31536000, immutable");
        assert_eq!(respond(&fetched, "HIT", &c, "photo", &Method::GET, &HeaderMap::new()).0.headers()["cache-control"], "public, max-age=60");

        let missing = failed(&Failure::BadUpstream, &c, "missing", &[]);
        assert_eq!(missing.status(), 404);
        assert_eq!(missing.headers()["content-disposition"], "inline; filename=\"missing.bin\"");
        assert_eq!(missing.headers()["x-forkrail-error"], "NOT_FOUND");
        let text = failed(&Failure::NotAnImage, &c, "robots", &[(HeaderName::from_static("x-from-vercel-json"), HeaderValue::from_static("1"))]);
        assert_eq!(text.status(), 400);
        assert!(text.headers().get(VARY).is_none() && text.headers().get("content-disposition").is_none());
        assert_eq!(text.headers()["x-from-vercel-json"], "1");
    }

    #[tokio::test]
    async fn one_call_for_requests_at_the_same_time() {
        let dir = std::env::temp_dir().join(format!("forkrail-images-{}", std::process::id()));
        let store = Arc::new(Statics::new(crate::statics::Source::Dir(dir), 1 << 20));
        let images = Arc::new(Images::new(
            store.clone(),
            store,
            Optimizer::Http { url: "http://127.0.0.1:9/".parse().unwrap(), client: hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http() },
            1 << 20,
        ));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let make = |calls: Arc<std::sync::atomic::AtomicUsize>| async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(Arc::new(Optimized { bytes: Bytes::new(), content_type: "image/png".into(), max_age: 0, revalidate: true, immutable: false, expires: 0, created: 0 }))
        };
        let (a, b) = tokio::join!(images.once("k", || make(calls.clone())), images.once("k", || make(calls.clone())));
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Done, so the next asks again.
        images.once("k", || make(calls.clone())).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
