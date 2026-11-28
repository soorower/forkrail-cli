//! A Next.js deployment's requests at the router (N3): its config, read once from the static
//! store, and the headers between visitors and Next. What the router serves of it from N2's
//! store is in `cached/nextjs.rs`; the rules themselves in `forkrail_common::next`.
//!
//! **Headers.** The router takes `x-middleware-*` off every visitor's request and every answer
//! (`platform_only`, `middleware::strip_protocol`: the header-forgery class of Next's
//! CVE-2025-29927, and C16's routing middleware protocol). For a Next deployment:
//! - in: `x-middleware-prefetch` is let through (Next's Pages Router client sends it on a
//!   prefetch; it only makes Next skip the prefetch's data, middleware still runs), and Next's
//!   own internal headers are taken off as well (`next-resume`, `x-invoke-*`, `x-nextjs-*` but
//!   `x-nextjs-data`, …: `forkrail_common::next::internal_request_header`); `x-middleware-subrequest`
//!   stays off with every other `x-middleware-*`.
//! - out: Next's answers keep the `x-middleware-*` its client reads (`-rewrite`, `-skip`,
//!   `-cache`, `-redirect`) and any an app names its own; the ones that carry the request on inside
//!   Next (`-override-headers`, `-request-*`, `-next`, `-set-cookie`) never reach a visitor.
//!   `x-nextjs-cache` becomes `x-vercel-cache` (Vercel never sends Next's).
//!
//! **A config that can't be read** fails closed: every request goes to Next, nothing is served or
//! kept by the router, the headers are still Next's.

use std::collections::HashMap;
use std::sync::Mutex;

use forkrail_common::next::{self as rules_next, Site};
use forkrail_common::routes::{NextRef, NextShape};

use super::*;
use crate::cache::Status;

/// While a config failed to read lately, it isn't asked for again.
const RETRY_FAILED: Duration = Duration::from_secs(30);
/// Configs kept; past it, all are forgotten (they're read again as needed).
const MAX_SITES: usize = 1024;

/// Deployments' Next.js configs, compiled once each (they never change: they're named by content).
#[derive(Default)]
pub struct Sites {
    slots: Mutex<HashMap<String, Slot>>,
}

#[derive(Clone)]
enum Slot {
    Loaded(Arc<Site>),
    Failed(Instant),
}

impl Sites {
    pub async fn get(&self, statics: Option<&Statics>, id: &str) -> Option<Arc<Site>> {
        match self.slots.lock().unwrap().get(id).cloned() {
            Some(Slot::Loaded(site)) => return Some(site),
            Some(Slot::Failed(at)) if at.elapsed() < RETRY_FAILED => return None,
            _ => {}
        }
        let key = rules_next::config_key(id);
        let loaded = match statics {
            Some(statics) => match statics.object(&key).await {
                // Its patterns are the build's (thousands, maybe): compiled off the request threads.
                Ok(bytes) if rules_next::NextConfig::id(&bytes) == id => tokio::task::spawn_blocking(move || serde_json::from_slice(&bytes).map_err(|e| e.to_string()).and_then(Site::compile))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string())),
                Ok(_) => Err("its content isn't what its name says".to_owned()),
                Err(e) => Err(e),
            },
            None => Err("no static store".to_owned()),
        };
        let mut slots = self.slots.lock().unwrap();
        if slots.len() >= MAX_SITES {
            slots.clear();
        }
        match loaded {
            Ok(site) => {
                let site = Arc::new(site);
                slots.insert(id.to_owned(), Slot::Loaded(site.clone()));
                Some(site)
            }
            Err(e) => {
                warn!(key, error = %e, "reading a Next.js deployment's config failed; Next answers all its requests");
                slots.insert(id.to_owned(), Slot::Failed(Instant::now()));
                None
            }
        }
    }
}

/// What a request to a Next.js deployment carries along (`Effects::next`).
pub(super) struct Ctx {
    #[allow(dead_code)]
    pub shape: NextShape,
    /// `None`: the config couldn't be read, and Next answers everything.
    pub site: Option<Arc<Site>>,
    /// The request is for a route handler or an API route: its answer has no page's `vary`.
    pub handler: bool,
    /// The request is for a public file only Next's server has: its `cache-control` is the app's.
    pub file: bool,
    /// The deployment's static files (their manifest's id): what its worker may answer with
    /// (`STATIC_HEADER`).
    pub files: Option<String>,
}

pub(super) async fn ctx(proxy: &Proxy, next: &NextRef, files: Option<String>) -> Ctx {
    Ctx { shape: next.shape, site: proxy.next_sites.get(proxy.statics.as_deref(), &next.config).await, handler: false, file: false, files }
}

/// A worker's word that its answer is one of the deployment's static files
/// (`forkrail_common::next::STATIC_HEADER`).
pub(super) const STATIC_HEADER: HeaderName = HeaderName::from_static(rules_next::STATIC_HEADER);

/// The file a worker's answer names (`STATIC_HEADER`), as the static files' manifest names it:
/// percent-decoded, a path. `None` for anything else.
pub(super) fn named_file(value: &HeaderValue) -> Option<String> {
    let path = forkrail_common::rules::convert::percent_decode(value.to_str().ok()?);
    (path.starts_with('/') && !path.contains('\u{fffd}')).then_some(path)
}

/// The worker's headers that go on the file it named, where the file's answer has none of the
/// name: what Next's routing and the app's middleware set on the way there (a cookie, a header of
/// the config's `headers()` whose value takes a parameter). Never what described the worker's
/// own empty answer or its connection: the file's answer says those for itself.
pub(super) fn carry_over(worker: &HeaderMap, answer: &mut HeaderMap) {
    const OWN: [&str; 17] = [
        "content-type",
        "content-length",
        "content-encoding",
        "content-range",
        "content-disposition",
        "accept-ranges",
        "etag",
        "last-modified",
        "age",
        "date",
        "vary",
        "connection",
        "keep-alive",
        "transfer-encoding",
        "trailer",
        "upgrade",
        "x-vercel-cache",
    ];
    for name in worker.keys() {
        if answer.contains_key(name) || OWN.contains(&name.as_str()) || name == STATIC_HEADER {
            continue;
        }
        for value in worker.get_all(name) {
            answer.append(name.clone(), value.clone());
        }
    }
}

/// The visitor's request headers as a Next deployment gets them: Next's internal ones off,
/// `x-middleware-prefetch` back on if the visitor sent it (it was taken off with every
/// `x-middleware-*` when the request came in).
pub(super) fn request_headers(headers: &mut HeaderMap, prefetch: Option<HeaderValue>) {
    let internal: Vec<HeaderName> = headers.keys().filter(|k| rules_next::internal_request_header(k.as_str())).cloned().collect();
    for name in internal {
        headers.remove(name);
    }
    if let Some(value) = prefetch {
        headers.insert(HeaderName::from_static(rules_next::CLIENT_MAY_SEND[0]), value);
    }
}

/// Next's answer's headers as the visitor gets them (in place of `middleware::strip_protocol`),
/// and as Vercel sends them: its `vary` without `Accept-Encoding` (the platform's to deal with),
/// none of a page's on a route handler's answer, and a page's ETag weak (JSON keeps Next's strong
/// one, as Vercel's does).
pub(super) fn answer_headers(headers: &mut HeaderMap, handler: bool) {
    let internal: Vec<HeaderName> =
        headers.keys().filter(|k| k.as_str().starts_with(forkrail_common::rules::middleware::PREFIX) && !rules_next::middleware_header_to_client(k.as_str())).cloned().collect();
    for name in internal {
        headers.remove(name);
    }
    if headers.contains_key(hyper::header::VARY) {
        const PAGE: [&str; 4] = ["rsc", "next-router-state-tree", "next-router-prefetch", "next-router-segment-prefetch"];
        let kept: Vec<String> = headers
            .get_all(hyper::header::VARY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case("accept-encoding") && !(handler && PAGE.iter().any(|p| n.eq_ignore_ascii_case(p))))
            .map(str::to_owned)
            .collect();
        headers.remove(hyper::header::VARY);
        if let Ok(value) = HeaderValue::from_str(&kept.join(", "))
            && !kept.is_empty()
        {
            headers.insert(hyper::header::VARY, value);
        }
    }
    // Next's standalone server sends some of its markers twice; Vercel once.
    for name in ["x-nextjs-prerender", "x-nextjs-stale-time"] {
        if headers.get_all(name).iter().count() > 1
            && let Some(first) = headers.get(name).cloned()
        {
            headers.insert(name, first);
        }
    }
    let html = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/html"));
    if let Some(etag) = headers.get(hyper::header::ETAG).and_then(|v| v.to_str().ok()).filter(|e| html && e.starts_with('"'))
        && let Ok(weak) = HeaderValue::from_str(&format!("W/{etag}"))
    {
        headers.insert(hyper::header::ETAG, weak);
    }
}

/// What a 304 to a Next deployment's visitor carries: Vercel's sends its `cache-control` and
/// `x-vercel-cache` alone (N6, 2026-11-17); the page's own headers don't go with it.
pub(super) fn not_modified_headers(headers: &mut HeaderMap) {
    let keep = [hyper::header::CACHE_CONTROL, crate::cache::STATUS_HEADER, REQUEST_ID, DEPLOYMENT_HEADER];
    let names: Vec<HeaderName> = headers.keys().filter(|k| !keep.contains(k)).cloned().collect();
    for name in names {
        headers.remove(name);
    }
}

/// `x-nextjs-cache` as `x-vercel-cache`'s value.
pub(super) fn label(headers: &HeaderMap) -> Option<Status> {
    match headers.get(rules_next::CACHE_HEADER)?.as_bytes() {
        b"HIT" => Some(Status::Hit),
        b"STALE" => Some(Status::Stale),
        b"MISS" => Some(Status::Miss),
        b"REVALIDATED" => Some(Status::Revalidated),
        _ => None,
    }
}

/// `x-nextjs-cache` taken off, and what it said.
pub(super) fn take_label(headers: &mut HeaderMap) -> Option<Status> {
    let found = label(headers);
    headers.remove(rules_next::CACHE_HEADER);
    found
}

/// Whether an answer is one of Next's cached pages (ISR, a prerender, a PPR shell): Next's
/// shared cache keeps it, never the router's CDN cache in front.
pub(super) fn nexts_own(headers: &HeaderMap) -> bool {
    ["x-nextjs-cache", "x-nextjs-prerender", "x-nextjs-postponed", "x-next-cache-tags"].iter().any(|n| headers.contains_key(*n))
}

/// A request for `/_next/static/…` the deployment hasn't got, and its middleware doesn't take:
/// Vercel's own answer, without a worker (`/_next/static/not-found.txt`, seen 2026-11-17).
pub(super) fn static_missing(site: &Site, head: &UpstreamHead, host: &str) -> Option<Response<ResponseBody>> {
    if !matches!(head.method, Method::GET | Method::HEAD) || !site.static_asset(head.uri.path()) {
        return None;
    }
    let req = forkrail_common::rules::Request { method: head.method.as_str(), path: head.uri.path(), query: head.uri.query(), host, headers: &head.headers };
    if site.middleware_runs(&req) {
        return None;
    }
    let body = if head.method == Method::HEAD { Bytes::new() } else { Bytes::from_static(b"Not Found") };
    let mut response = Response::new(Full::new(body).map_err(|never| match never {}).boxed());
    *response.status_mut() = StatusCode::NOT_FOUND;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    headers.insert(CONTENT_LENGTH, HeaderValue::from(9));
    headers.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static(crate::cache::DEFAULT_CACHE_CONTROL));
    headers.insert(hyper::header::CONTENT_DISPOSITION, HeaderValue::from_static("inline; filename=\"not-found.txt\""));
    headers.insert(HeaderName::from_static("access-control-allow-origin"), HeaderValue::from_static("*"));
    crate::cache::mark(headers, Status::Hit, 0);
    Some(response)
}

/// Whether the deployment's middleware may run on this request.
pub(super) fn middleware_takes(site: &Site, head: &UpstreamHead, host: &str) -> bool {
    let req = forkrail_common::rules::Request { method: head.method.as_str(), path: head.uri.path(), query: head.uri.query(), host, headers: &head.headers };
    site.middleware_runs(&req)
}

/// Whether `/_next/image` under the base path is the router's to answer for this request: the
/// deployment has Next's image settings, and its middleware doesn't run on the request (if it
/// does, Next's own optimizer answers, after it).
pub(super) fn image_is_routers(site: &Site, head: &UpstreamHead, host: &str) -> bool {
    if !site.config.images || head.uri.path() != site.image_path() {
        return false;
    }
    let req = forkrail_common::rules::Request { method: head.method.as_str(), path: head.uri.path(), query: head.uri.query(), host, headers: &head.headers };
    !site.middleware_runs(&req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_next_deployments_request_headers() {
        let mut h = HeaderMap::new();
        for (k, v) in [("next-resume", "1"), ("x-invoke-path", "/admin"), ("x-nextjs-rewrite", "/x"), ("x-nextjs-data", "1"), ("rsc", "1"), ("next-router-state-tree", "%5B%5D")] {
            h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        request_headers(&mut h, Some(HeaderValue::from_static("1")));
        let mut names: Vec<&str> = h.keys().map(HeaderName::as_str).collect();
        names.sort();
        assert_eq!(names, ["next-router-state-tree", "rsc", "x-middleware-prefetch", "x-nextjs-data"]);
    }

    #[test]
    fn a_next_deployments_answer_headers() {
        let mut h = HeaderMap::new();
        for k in ["x-middleware-rewrite", "x-middleware-skip", "x-middleware-cache", "x-middleware-ran", "x-middleware-next", "x-middleware-override-headers", "x-middleware-request-authorization", "x-middleware-set-cookie", "x-nextjs-matched-path"] {
            h.insert(HeaderName::from_static(k), HeaderValue::from_static("v"));
        }
        answer_headers(&mut h, false);
        let mut names: Vec<&str> = h.keys().map(HeaderName::as_str).collect();
        names.sort();
        assert_eq!(names, ["x-middleware-cache", "x-middleware-ran", "x-middleware-skip", "x-nextjs-matched-path"]);
        h.insert("x-nextjs-cache", HeaderValue::from_static("STALE"));
        assert_eq!(take_label(&mut h), Some(Status::Stale));
        assert!(!h.contains_key("x-nextjs-cache"));
        // Vercel's vary and a weak ETag.
        let mut page = HeaderMap::new();
        page.insert(hyper::header::VARY, HeaderValue::from_static("rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch, Accept-Encoding"));
        page.insert(hyper::header::ETAG, HeaderValue::from_static("\"abc\""));
        page.insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        let mut handler = page.clone();
        answer_headers(&mut page, false);
        assert_eq!((page["vary"].to_str().unwrap(), page["etag"].to_str().unwrap()), (forkrail_common::next::APP_VARY, "W/\"abc\""));
        answer_headers(&mut handler, true);
        assert!(!handler.contains_key("vary"));
    }

    #[test]
    fn a_workers_word_names_a_path_and_brings_routings_headers() {
        assert_eq!(named_file(&HeaderValue::from_static("/another.txt")).as_deref(), Some("/another.txt"));
        assert_eq!(named_file(&HeaderValue::from_static("/docs/_next/static/b1/_buildManifest.js")).as_deref(), Some("/docs/_next/static/b1/_buildManifest.js"));
        assert_eq!(named_file(&HeaderValue::from_static("/caf%C3%A9%20au%20lait.txt")).as_deref(), Some("/café au lait.txt"));
        assert_eq!(named_file(&HeaderValue::from_static("/100%25.txt")).as_deref(), Some("/100%.txt"));
        for odd in ["another.txt", "", "https://elsewhere.example/x", "/%FF.txt"] {
            assert_eq!(named_file(&HeaderValue::from_static(odd)), None, "{odd}");
        }
        let mut worker = HeaderMap::new();
        for (k, v) in [
            ("set-cookie", "a=1"),
            ("set-cookie", "b=2"),
            ("x-from-config", "yes"),
            ("cache-control", "private"),
            ("content-type", "text/html"),
            ("content-length", "0"),
            ("etag", "\"w\""),
            ("vary", "rsc"),
            ("date", "Mon, 16 Nov 2026 05:37:41 GMT"),
            ("connection", "keep-alive"),
            ("x-forkrail-static", "/a.txt"),
        ] {
            worker.append(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        let mut file = HeaderMap::new();
        file.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
        file.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
        carry_over(&worker, &mut file);
        let mut names: Vec<&str> = file.keys().map(HeaderName::as_str).collect();
        names.sort();
        assert_eq!(names, ["cache-control", "content-type", "set-cookie", "x-from-config"]);
        assert_eq!(file.get_all("set-cookie").iter().count(), 2);
        assert_eq!(file["cache-control"], "public, max-age=31536000, immutable", "the file's own wins");
    }

    #[test]
    fn markers_once_and_a_304_as_vercels() {
        let mut h = HeaderMap::new();
        h.append("x-nextjs-prerender", HeaderValue::from_static("1"));
        h.append("x-nextjs-prerender", HeaderValue::from_static("1"));
        answer_headers(&mut h, false);
        assert_eq!(h.get_all("x-nextjs-prerender").iter().count(), 1);
        let mut not_modified = HeaderMap::new();
        for (k, v) in [("cache-control", "public, max-age=0, must-revalidate"), ("x-vercel-cache", "HIT"), ("etag", "W/\"a\""), ("vary", "rsc"), ("x-nextjs-stale-time", "300"), ("age", "4")] {
            not_modified.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        not_modified_headers(&mut not_modified);
        let mut names: Vec<&str> = not_modified.keys().map(HeaderName::as_str).collect();
        names.sort();
        assert_eq!(names, ["cache-control", "x-vercel-cache"]);
    }
}
