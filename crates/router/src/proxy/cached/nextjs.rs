//! The response cache's part of a Next.js deployment's request (N3): one freshness authority,
//! N2's store, and nothing served past the middleware or Draft Mode.
//!
//! In order, for every request to a Next deployment:
//! 1. **Draft Mode** (`__prerender_bypass` equal to the build's preview id): Next answers, nothing
//!    is kept (`BYPASS`).
//! 2. **On-demand revalidation** (`x-prerender-revalidate` equal to it): Next makes the page now
//!    and keeps it in the store itself; nothing kept here. With another value the header is taken
//!    off (Next would ignore it).
//! 3. **Middleware**: a request the deployment's middleware may match goes to Next, which runs it
//!    and then reads the shared cache; the router neither serves nor keeps anything for it.
//! 4. **The store**: a request for one of Next's kept entries (an App Router page and its RSC or
//!    segments, a route handler's answer, a Pages Router page and its data) is answered from it:
//!    `HIT` while fresh by its time and N2's tag marks; `STALE` once its time is up or a tag was
//!    invalidated with a profile, while Next makes it again once (N2's lease, shared with the
//!    workers' readers, so neither starts a second); a deleted or expired one, or one with a PPR
//!    shell, is Next's to make while the client waits. A regeneration is a plain request in the
//!    home region, whose Next reads the stale entry first, gets N2's lease and makes the page
//!    again behind its answer (`revalidateReason: 'stale'`, as on Vercel); the new copy is pushed
//!    to the other regions.
//! 5. **Anything else** is Next's answer, kept as a CDN answer only when it asks for that itself
//!    (a route handler's `s-maxage`) and isn't one of Next's cached pages.
//!
//! Headers are Vercel's: `x-vercel-cache`, `age`, `public, max-age=0, must-revalidate` to the
//! browser, `vary` and `x-nextjs-prerender` on App Router pages, a weak ETag, the config's
//! `headers()`; Next's own `x-nextjs-cache` becomes `x-vercel-cache` (`nextjs::take_label`).

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

use forkrail_common::next::{Lookup, Part, Site};
use forkrail_common::nextcache::{self as wire};

use super::*;
use crate::proxy::nextjs::Ctx;

/// Smaller bodies go as they are (the threshold of the `compression` package Next uses).
const GZIP_FROM: usize = 1024;
/// Larger ones go as they are too: compressing them takes a request thread's time (Next's
/// pages are far smaller; a worker may keep up to 20 MB).
const GZIP_UP_TO: usize = 2 << 20;
/// Compressed parts kept, in bytes; past it, all are forgotten.
const GZIP_MEMORY: usize = 16 << 20;

/// A Next deployment's request: what the router answers itself, or how Next's answer is kept.
pub(super) async fn before(
    proxy: &Arc<Proxy>,
    cache: &Arc<ResponseCache>,
    pool: &Arc<Pool>,
    next: &Arc<Ctx>,
    head: &mut UpstreamHead,
    effects: &mut Effects,
    log: &mut AccessLog,
) -> Option<Response<ResponseBody>> {
    let get = matches!(head.method, Method::GET | Method::HEAD);
    let Some(site) = next.site.clone() else { return pass(effects, log, Status::Miss, get) };
    if site.draft_mode(&head.headers) {
        return pass(effects, log, Status::Bypass, get);
    }
    match site.revalidation(&head.headers) {
        Some(true) => return pass(effects, log, Status::Revalidated, get),
        Some(false) => {
            head.headers.remove(forkrail_common::next::REVALIDATE_HEADER);
        }
        None => {}
    }
    let found = {
        let req = forkrail_common::rules::Request { method: head.method.as_str(), path: head.uri.path(), query: head.uri.query(), host: &log.host, headers: &head.headers };
        if site.middleware_runs(&req) {
            return pass(effects, log, Status::Miss, get);
        }
        site.lookup(&req).and_then(|lookup| site.config_headers(&req).map(|own| (lookup, own)))
    };
    if let Some((lookup, own)) = found
        && let Some(response) = from_store(proxy, cache, pool, &site, &lookup, own, head, effects, log).await
    {
        return Some(response);
    }
    // A path nothing of Next's takes: the build's not-found page, as Vercel serves it.
    let not_found = {
        let req = forkrail_common::rules::Request { method: head.method.as_str(), path: head.uri.path(), query: head.uri.query(), host: &log.host, headers: &head.headers };
        site.not_found(&req).and_then(|lookup| site.config_headers(&req).map(|own| (lookup, own)))
    };
    if let Some((lookup, own)) = not_found
        && let Some(response) = from_store(proxy, cache, pool, &site, &lookup, own, head, effects, log).await
    {
        return Some(response);
    }
    if !get || head.headers.contains_key(AUTHORIZATION) {
        return pass(effects, log, Status::Miss, false);
    }
    let deployment = log.deployment.clone()?;
    let file = next.file;
    let answered = cdn(proxy, cache, pool, &deployment, head, effects, log).await;
    if let Some(fill) = effects.cache.as_mut() {
        fill.next = true;
        fill.file = file;
    }
    answered
}

/// Next answers, its answer told `label` unless it says otherwise; nothing kept.
fn pass(effects: &mut Effects, log: &mut AccessLog, label: Status, cacheable: bool) -> Option<Response<ResponseBody>> {
    let mut fill = Fill::label(label, cacheable);
    fill.next = true;
    fill.file = effects.next.as_ref().is_some_and(|n| n.file);
    effects.cache = Some(fill);
    log.cache = Some(label.as_str());
    None
}

/// An entry of Next's in the store for one of `lookup`'s keys: in this router's store, else the
/// build's seed.
/// An entry of Next's in the store for one of `lookup`'s keys: in this router's store, else the
/// build's seed (`true`: brought from the seed for this request).
async fn entry_for(cache: &ResponseCache, deployment: &str, lookup: &Lookup) -> Option<(String, Arc<Entry>, bool)> {
    let next = crate::cache::next::get();
    for key in &lookup.keys {
        let key_hash = forkrail_common::next::key_hash(key);
        let hash = cache::hash(&wire::hash_input(deployment, &key_hash));
        if let Some(entry) = cache.get(deployment, &hash, true).await {
            return Some((hash, entry, false));
        }
        if let Some(next) = next
            && let Some(entry) = next.seeded(deployment, &key_hash, &hash).await
        {
            return Some((hash, entry, true));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn from_store(
    proxy: &Arc<Proxy>,
    cache: &Arc<ResponseCache>,
    pool: &Arc<Pool>,
    site: &Arc<Site>,
    lookup: &Lookup,
    own: Vec<(String, String)>,
    head: &UpstreamHead,
    effects: &mut Effects,
    log: &mut AccessLog,
) -> Option<Response<ResponseBody>> {
    let deployment = log.deployment.clone()?;
    let (hash, entry, seeded) = entry_for(cache, &deployment, lookup).await?;
    if !entry.headers.contains_key(wire::MARK_HEADER) {
        return None;
    }
    let (meta, at) = wire::Meta::of(&entry.body)?;
    // A PPR shell is answered whole by Next (it resumes the rest in the same answer); serving
    // the shell here and only the rest from a worker needs direct handler calls (N8).
    if meta.kind != lookup.kind || meta.parts.iter().any(|(name, _)| name == "postponed") {
        return None;
    }
    let range = meta.part(&lookup.part.name(), at)?;
    let now = cache::now_ms();
    if meta.exp.filter(|e| e.is_finite() && *e > 0.0).is_some_and(|exp| now > entry.stored_ms.saturating_add((exp * 1000.0) as u64)) {
        return None;
    }
    let label = match cache.freshness(&entry, now, scope_of(pool)) {
        Freshness::Fresh => Status::Hit,
        Freshness::Stale | Freshness::Expired => {
            if !lookup.status_page() {
                regenerate(proxy, cache, pool, site, lookup, head, log, &hash);
            }
            Status::Stale
        }
        // Deleted (`revalidateTag` without a profile, `updateTag`, a purge): Next makes it now.
        Freshness::Deleted => return None,
    };
    // The not-found page is served only as the build made it; anything else is Next's to say.
    if lookup.status_page() && label != Status::Hit {
        return None;
    }
    // The build's own page, served for the first time here: Vercel's `PRERENDER` (Vercel's 404
    // page is a file: `HIT`).
    let label = if seeded && !lookup.status_page() && !lookup.fully_static { Status::Prerender } else { label };
    let status = if lookup.status_page() { StatusCode::NOT_FOUND } else { entry.status };
    let mut headers = answer_headers(&entry.headers, lookup, site.config.deployment_id.as_deref(), own);
    // A page that never goes stale, and the build's own page served for the first time, Vercel
    // serves as a file of the build (its `vary` too, on a Pages Router page's).
    if (lookup.fully_static || seeded) && matches!(lookup.part, Part::Html | Part::Data) {
        // Named only on the 404 page (Vercel's `/404` file); a prerender is plain `inline`.
        let disposition = if lookup.status_page() { inline_disposition(&lookup.page) } else { HeaderValue::from_static("inline") };
        headers.insert(hyper::header::CONTENT_DISPOSITION, disposition);
        if seeded && lookup.part == Part::Data {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        headers.insert(HeaderName::from_static("access-control-allow-origin"), HeaderValue::from_static("*"));
        if seeded && !lookup.status_page() {
            headers.insert(hyper::header::VARY, HeaderValue::from_static(forkrail_common::next::APP_VARY));
        }
    }
    let part = lookup.part.name();
    // Vercel's ETags: weak on a page, strong on a Pages Router page's data, none on an RSC
    // payload or segment.
    let tag = &cache::hash(&format!("{hash}\n{}\n{part}", entry.stored_ms))[..24];
    let etag = match lookup.part {
        Part::Html => Some(format!("W/\"{tag}\"")),
        Part::Data => Some(format!("\"{tag}\"")),
        _ => None,
    };
    if let Some(etag) = etag {
        headers.insert(ETAG, HeaderValue::from_str(&etag).expect("hex"));
    }
    let mut body = entry.body.slice(range);
    if body.len() >= GZIP_FROM
        && body.len() <= GZIP_UP_TO
        && accepts_gzip(&head.headers)
        && compressible(headers.get(CONTENT_TYPE))
        && let Some(gz) = gzipped(&format!("{hash}\n{}\n{part}", entry.stored_ms), &body)
    {
        body = gz;
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    let route = route_of(effects);
    let method = head.method.clone();
    let mut response = respond(cache, status, headers, body.clone(), entry.age(now), label, &method, Some(&head.headers), &route);
    if response.status() == StatusCode::NOT_MODIFIED {
        crate::proxy::nextjs::not_modified_headers(response.headers_mut());
    }
    log.function = None;
    log.cache = Some(label.as_str());
    log.encoding = response.headers().get(CONTENT_ENCODING).and_then(|v| v.to_str().ok()).map(str::to_owned);
    log.bytes_out = if method == Method::HEAD || response.status() == StatusCode::NOT_MODIFIED { 0 } else { body.len() as u64 };
    Some(log_now(log, response, "cache"))
}

/// Next's page made again, once: a plain request for it to a worker in the home region (an edge
/// asks home, and sends it itself only while home can't be reached), whose Next reads the stale
/// entry, is given N2's lease as its first reader, answers with the stale copy and makes the page
/// again behind it (N4's `waitUntil` holds the worker), with `revalidateReason: 'stale'` as on
/// Vercel. (`x-prerender-revalidate` would make Next render at once, but it reports that as
/// `on-demand` to the app; Next's own `revalidate-reason` test caught it.) None is sent while
/// anyone holds the lease, and one at a time from a router; other workers' readers are served the
/// stale copy until the new one is written. What Next keeps reaches every router as any write does.
#[allow(clippy::too_many_arguments)]
fn regenerate(proxy: &Arc<Proxy>, cache: &Arc<ResponseCache>, pool: &Arc<Pool>, site: &Site, lookup: &Lookup, head: &UpstreamHead, log: &AccessLog, hash: &str) {
    let mut path = format!("{}{}", site.config.base_path, lookup.page);
    if site.config.trailing_slash && lookup.page != "/" {
        path.push('/');
    }
    let Ok(uri) = format!("http://{}{path}", head.host).parse::<Uri>() else { return };
    let mut headers = HeaderMap::new();
    // The visitor's forwarding and location headers, so the page sees the same origin; none of
    // what is theirs alone, and nothing that makes Next answer in part.
    for (name, value) in &head.headers {
        let n = name.as_str();
        if n.starts_with("x-forwarded-") || n.starts_with("x-vercel-ip-") || matches!(n, "x-vercel-id" | "x-real-ip" | "user-agent" | "accept") {
            headers.append(name.clone(), value.clone());
        }
    }
    let Some(next) = crate::cache::next::get() else { return };
    // Being made (someone holds N2's lease, here or elsewhere), or asked for from here already.
    if next.leased(hash) {
        return;
    }
    let Some(guard) = cache.lead(hash) else { return };
    let home = cache.home().cloned();
    let regen = UpstreamHead { host: head.host.clone(), method: Method::GET, uri, headers, payload: 0 };
    let log = background(log, pool);
    let (proxy, pool) = (proxy.clone(), pool.clone());
    let (hash, deployment) = (hash.to_owned(), log.deployment.clone().unwrap_or_default());
    tokio::spawn(async move {
        let _guard = guard;
        if let Some(link) = home {
            let ask = crate::cache::peers::Ask {
                op: crate::cache::peers::Op::Regenerate,
                host: regen.host.clone(),
                deployment,
                function: regen.uri.path().to_owned(),
                max_duration_ms: None,
                hash: hash.clone(),
                have: None,
                wait: false,
                render: false,
                if_generated: false,
                uri: regen.uri.to_string(),
                headers: regen.headers.clone(),
                request_id: log.request_id.clone(),
                from: link.id.clone(),
            };
            if let Ok(crate::cache::peers::Reply::None { .. }) = link.ask(&ask, QUICK_ASK).await {
                discard(log);
                return;
            }
            // Home can't: asked for here.
        }
        here(&proxy, &pool, regen, log).await;
    });
}

/// The regeneration request through this router's own pool, its answer read and dropped (Next
/// keeps what it made through its cache handler).
async fn here(proxy: &Arc<Proxy>, pool: &Arc<Pool>, mut head: UpstreamHead, mut log: AccessLog) {
    if let Ok(id) = HeaderValue::from_str(&log.request_id) {
        head.headers.insert(REQUEST_ID, id);
    }
    pool.logs().begin(&log.request_id);
    log.app_logs = Some(pool.logs().clone());
    log.cache = Some(Status::Revalidated.as_str());
    let (tx, rx) = oneshot::channel();
    let effects = Effects { cache: Some(Fill { label: Status::Revalidated, cacheable: true, keep: None, route: Box::default(), raw: true, next: true, file: false }), ..Effects::default() };
    let body = UpstreamBody::Buffered { bytes: Bytes::new(), _room: None };
    tokio::spawn(exchange(head, body, pool.clone(), Version::HTTP_11, proxy.clone(), log, effects, tx));
    let Ok(response) = rx.await else { return };
    let status = response.status();
    let mut body = response.into_body();
    while let Some(frame) = body.frame().await {
        if frame.is_err() {
            break;
        }
    }
    if !status.is_success() {
        tracing::debug!(%status, "a Next.js page's regeneration wasn't made; its stale copy is served until its lease ends");
    }
}

/// Home's part of an edge's `Regenerate` ask: made here, at once, in the background.
pub(super) async fn answer_regenerate(proxy: &Arc<Proxy>, ask: &crate::cache::peers::Ask) -> crate::cache::peers::Answer {
    use crate::cache::peers::Answer;
    let host = normalize_host(&ask.host);
    let Some(target) = proxy.registry.deployment_target(&host, &ask.deployment) else { return Answer::Unknown };
    let (Some(pool), Some(next)) = (target.pool.clone(), target.next.as_ref()) else { return Answer::Unknown };
    let ctx = crate::proxy::nextjs::ctx(proxy, next, target.static_files.clone()).await;
    let (Some(_), Ok(uri), Some(cache)) = (ctx.site.as_ref(), ask.uri.parse::<Uri>(), proxy.cache.clone()) else { return Answer::Unknown };
    // A page of the deployment's, asked for with nothing of a visitor's that's theirs alone.
    if ask.headers.contains_key(COOKIE) || ask.headers.contains_key(AUTHORIZATION) || ask.headers.contains_key(forkrail_common::next::REVALIDATE_HEADER) {
        return Answer::Unknown;
    }
    // Someone (a request here, a worker's reader, another edge) is making it already.
    if crate::cache::next::get().is_none_or(|next| next.leased(&ask.hash)) {
        return Answer::None { rendering: true, lost: false };
    }
    let Some(guard) = cache.lead(&ask.hash) else { return Answer::None { rendering: true, lost: false } };
    let head = UpstreamHead { host: ask.host.clone(), method: Method::GET, uri, headers: ask.headers.clone(), payload: 0 };
    let log = peer_log(ask, &host, &target, &pool, &head);
    let (proxy, pool) = (proxy.clone(), pool.clone());
    tokio::spawn(async move {
        here(&proxy, &pool, head, log).await;
        drop(guard);
    });
    Answer::None { rendering: true, lost: false }
}

/// The headers a store answer goes out with (before the routes', the browser's cache-control,
/// `age` and `x-vercel-cache`): the page's own as Next kept them, without what's the platform's
/// or Next's server's alone, with its part's type and, for an App Router page, Vercel's `vary`
/// and `x-nextjs-prerender`, then the config's `headers()`.
fn answer_headers(kept: &HeaderMap, lookup: &Lookup, deployment_id: Option<&str>, own: Vec<(String, String)>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in kept {
        let skip = name == wire::MARK_HEADER
            || name == cache::CACHE_TAG
            || name == CONTENT_LENGTH
            || name == CONTENT_ENCODING
            || name == SET_COOKIE
            || name == hyper::header::TRANSFER_ENCODING
            || name.as_str() == "x-next-cache-tags"
            || name.as_str() == forkrail_common::next::CACHE_HEADER
            // Next writes its own (`s-maxage`) for its server; Vercel's browser one goes out.
            || (name == hyper::header::CACHE_CONTROL && lookup.part != Part::Body);
        if !skip {
            headers.append(name.clone(), value.clone());
        }
    }
    if let Some(kind) = lookup.part.content_type() {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(kind));
    }
    if lookup.app && lookup.part != Part::Body {
        headers.insert(hyper::header::VARY, HeaderValue::from_static(forkrail_common::next::APP_VARY));
        headers.entry("x-nextjs-prerender").or_insert(HeaderValue::from_static("1"));
    }
    // Next sends its deployment id with RSC payloads and Pages Router data, for skew protection.
    if matches!(lookup.part, Part::Rsc | Part::Segment(_) | Part::Data)
        && let Some(id) = deployment_id.and_then(|id| HeaderValue::from_str(id).ok())
    {
        headers.insert("x-nextjs-deployment-id", id);
    }
    for (name, value) in own {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
            headers.insert(name, value);
        }
    }
    // Next's client keeps a segment (the route tree `/_tree` too) only when the answer says it
    // came from the per-segment prerender, `x-nextjs-postponed: 2`: Next's server sets it on every
    // segment it serves from its cache, and Vercel's segment outputs carry it. Without it the
    // client throws the segment away, asks for it again and then has a worker render the page
    // for a runtime prefetch. Set after the config's headers, as Next's handler sets it after
    // routing's.
    if matches!(lookup.part, Part::Segment(_)) {
        headers.insert(HeaderName::from_static(forkrail_common::next::DID_POSTPONE_HEADER), HeaderValue::from_static("2"));
    }
    headers
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    cache::encodings(headers).split(',').any(|c| c == "gzip")
}

fn compressible(kind: Option<&HeaderValue>) -> bool {
    let Some(kind) = kind.and_then(|v| v.to_str().ok()) else { return false };
    let kind = kind.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    kind.starts_with("text/") || kind.ends_with("+json") || kind.ends_with("+xml") || matches!(kind.as_str(), "application/json" | "application/javascript" | "application/xml" | "image/svg+xml")
}

static GZIPPED: Mutex<Option<(HashMap<String, Bytes>, usize)>> = Mutex::new(None);

/// A part, gzipped once per version of its entry on this router.
fn gzipped(key: &str, body: &Bytes) -> Option<Bytes> {
    if let Some((kept, _)) = GZIPPED.lock().unwrap().as_ref()
        && let Some(found) = kept.get(key)
    {
        return Some(found.clone());
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::with_capacity(body.len() / 3), flate2::Compression::new(6));
    encoder.write_all(body).ok()?;
    let gz = Bytes::from(encoder.finish().ok()?);
    // One that would take more than a quarter of the memo isn't kept: it would push out the rest.
    if gz.len() > GZIP_MEMORY / 4 {
        return Some(gz);
    }
    let mut kept = GZIPPED.lock().unwrap();
    let (map, used) = kept.get_or_insert_with(Default::default);
    if *used + gz.len() > GZIP_MEMORY {
        map.clear();
        *used = 0;
    }
    *used += gz.len();
    map.insert(key.to_owned(), gz.clone());
    Some(gz)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(part: Part, app: bool) -> Lookup {
        Lookup { page: "/blog/a".into(), part, kind: if app { "APP_PAGE" } else { "PAGES" }, keys: [String::new(), String::new()], app, fully_static: false }
    }

    #[test]
    fn a_store_answers_headers_are_vercels() {
        let mut kept = HeaderMap::new();
        for (k, v) in [
            ("x-forkrail-next", "1"),
            ("vercel-cache-tag", "_N_T_/blog/a,posts"),
            ("x-next-cache-tags", "_N_T_/blog/a,posts"),
            ("cache-control", "s-maxage=60, stale-while-revalidate=31535940"),
            ("set-cookie", "s=1"),
            ("content-length", "10"),
            ("x-nextjs-stale-time", "300"),
            ("x-powered-by", "Next.js"),
        ] {
            kept.append(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        let own = vec![("x-from-config".to_owned(), "yes".to_owned())];
        let html = answer_headers(&kept, &lookup(Part::Html, true), Some("dpl_1"), own.clone());
        let mut names: Vec<&str> = html.keys().map(HeaderName::as_str).collect();
        names.sort();
        assert_eq!(names, ["content-type", "vary", "x-from-config", "x-nextjs-prerender", "x-nextjs-stale-time", "x-powered-by"]);
        assert_eq!(html["content-type"], "text/html; charset=utf-8");
        assert_eq!(html["vary"], forkrail_common::next::APP_VARY);
        let rsc = answer_headers(&kept, &lookup(Part::Rsc, true), Some("dpl_1"), Vec::new());
        assert_eq!((rsc["content-type"].to_str().unwrap(), rsc["x-nextjs-deployment-id"].to_str().unwrap()), ("text/x-component", "dpl_1"));
        let data = answer_headers(&kept, &lookup(Part::Data, false), None, Vec::new());
        assert_eq!(data["content-type"], "application/json; charset=utf-8");
        assert!(!data.contains_key("vary") && !data.contains_key("x-nextjs-prerender"), "a Pages Router page has no App Router headers");
        // A route handler keeps its own type and caching.
        let mut route = HeaderMap::new();
        route.insert(CONTENT_TYPE, HeaderValue::from_static("application/xml"));
        route.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=600"));
        let body = answer_headers(&route, &lookup(Part::Body, true), None, Vec::new());
        assert_eq!((body["content-type"].to_str().unwrap(), body["cache-control"].to_str().unwrap()), ("application/xml", "public, max-age=600"));
    }

    /// A segment prefetch (the route tree too) answered from the store says it came from the
    /// per-segment prerender, as Next's server and Vercel say; nothing else does.
    #[test]
    fn segments_say_they_are_per_segment_prerenders() {
        let mut kept = HeaderMap::new();
        kept.insert(HeaderName::from_static("x-nextjs-stale-time"), HeaderValue::from_static("300"));
        for segment in ["/_tree", "/!KGRlZmF1bHQp/fully-static/__PAGE__"] {
            let own = vec![("x-nextjs-postponed".to_owned(), "1".to_owned())];
            let answer = answer_headers(&kept, &lookup(Part::Segment(segment.into()), true), Some("dpl_1"), own);
            assert_eq!(answer["x-nextjs-postponed"], "2", "{segment}");
            assert_eq!(answer.get_all("x-nextjs-postponed").iter().count(), 1);
            assert_eq!(answer["content-type"], "text/x-component");
        }
        for part in [Part::Html, Part::Rsc] {
            assert!(!answer_headers(&kept, &lookup(part.clone(), true), Some("dpl_1"), Vec::new()).contains_key("x-nextjs-postponed"), "{part:?}");
        }
    }

    /// What N3 adds to a request, on this machine:
    /// `cargo test --release -p forkrail-router next_cost_per_request -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn next_cost_per_request() {
        use forkrail_common::next::{Matcher, Middleware, NextConfig, Route, RouteKind, Site};
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let n = 200_000u32;
            let mut visitor = HeaderMap::new();
            for (k, v) in [("accept", "text/html,application/xhtml+xml"), ("accept-encoding", "gzip, deflate, br"), ("user-agent", "Mozilla/5.0 (Macintosh) Chrome/142"), ("cookie", "a=1; b=2"), ("x-forwarded-for", "1.2.3.4")] {
                visitor.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
            }
            // A non-Next deployment's request: the one header looked for before routing.
            let started = std::time::Instant::now();
            let mut found = 0;
            for _ in 0..n {
                found += usize::from(std::hint::black_box(&visitor).get(crate::proxy::nextjs_prefetch()).is_some());
            }
            println!("non-Next request: {:?} (x-middleware-prefetch looked for; found {found})", started.elapsed() / n);

            // next-app's config: 5 middleware matchers, 12 routes, a config header rule.
            let preview = "d23be5563d67de649c8cc28f3cce48ea";
            let matcher = |p: &str| Matcher {
                regexp: format!("^(?:\\/(_next\\/data\\/[^/]{{1,}}))?\\/{p}(\\.json|\\.rsc|\\.segments\\/.+\\.segment\\.rsc)?[\\/#\\?]?$"),
                has: vec![],
                missing: vec![serde_json::from_value(serde_json::json!({"type": "header", "key": "x-prerender-revalidate", "value": preview})).unwrap()],
            };
            let route = |path: &str, page: &str, regex: Option<&str>| Route { path: path.into(), page: page.into(), kind: RouteKind::AppPage, regex: regex.map(Into::into), max_duration_s: None };
            let config = NextConfig {
                format: forkrail_common::next::FORMAT,
                version: "16.4.6".into(),
                build_id: "b1".into(),
                deployment_id: Some("dpl_1".into()),
                preview_id: preview.into(),
                base_path: String::new(),
                trailing_slash: false,
                i18n: false,
                cache_components: false,
                middleware: Some(Middleware { runtime: "nodejs".into(), matchers: ["legacy", "rewritten", "ssr", "isr", "secret"].iter().map(|p| matcher(p)).collect() }),
                routes: vec![route("/", "/page", None), route("/isr", "/isr/page", None), route("/ssr", "/ssr/page", None), route("/action", "/action/page", None), route("/blog/[slug]", "/blog/[slug]/page", Some("^/blog/([^/]+?)(?:/)?$"))],
                prerenders: vec![],
                before: vec!["^(?!/_next)/old-blog(?:/([^/]+?))(?:/)?$".into(), "^/hi(?:/)?$".into()],
                after: vec!["^/docs(?:/((?:[^/]+?)(?:/(?:[^/]+?))*))?(?:/)?$".into()],
                headers: vec![forkrail_common::next::HeaderRule { source: "/ssr".into(), regex: "^/ssr(?:/)?$".into(), has: vec![], missing: vec![], headers: vec![("x-from-config".into(), "yes".into())] }],
                next_url_rsc: false,
                images: true,
                fallback: vec![],
                asset_prefix: String::new(),
                server_files: vec![],
                not_found: None,
                known: vec![],
                known_dynamic: vec![],
            };
            let site = Site::compile(config).unwrap();
            let view = |path: &'static str| forkrail_common::rules::Request { method: "GET", path, query: None, host: "next.test", headers: &visitor };

            // A request the middleware takes: to the worker, after these checks.
            let started = std::time::Instant::now();
            let mut taken = 0;
            for _ in 0..n {
                let mut headers = visitor.clone();
                crate::proxy::nextjs::request_headers(&mut headers, None);
                let req = forkrail_common::rules::Request { method: "GET", path: "/ssr", query: None, host: "next.test", headers: &headers };
                if !site.draft_mode(&headers) && site.revalidation(&headers).is_none() && site.middleware_runs(&req) {
                    taken += 1;
                }
            }
            println!("Next request the middleware takes (headers, Draft Mode, revalidation, 5 matchers): {:?} ({taken})", started.elapsed() / n);

            // A page from the store: the same checks, the lookup, the entry, its headers and the answer.
            let cache = ResponseCache::new(64 << 20, 32 << 20, None, None, true);
            let meta = wire::Meta { kind: "APP_PAGE".into(), key_hash: "k".into(), parts: vec![("html".into(), 20_000), ("rsc".into(), 8_000)], rev: Some(60.0), ..wire::Meta::default() };
            let body = Bytes::from(meta.body(&[&vec![b'h'; 20_000], &vec![b'r'; 8_000]]));
            let lookup = site.lookup(&view("/blog/first")).unwrap();
            let hash = cache::hash(&wire::hash_input("dep-1", &forkrail_common::next::key_hash(&lookup.keys[0])));
            let mut kept = HeaderMap::new();
            kept.insert(HeaderName::from_static(wire::MARK_HEADER), HeaderValue::from_static("1"));
            kept.insert(cache::CACHE_TAG, HeaderValue::from_static("_N_T_/layout,_N_T_/blog/layout,_N_T_/blog/[slug]/page,_N_T_/blog/first"));
            kept.insert(HeaderName::from_static("x-nextjs-stale-time"), HeaderValue::from_static("300"));
            let entry = Entry { status: StatusCode::OK, headers: kept, body, stored_ms: cache::now_ms(), fresh_until: Some(cache::now_ms() + 3_600_000), stale_until: None, deployment: "dep-1".into(), group: None, page: true, local: false };
            cache.put(&hash, entry, None);
            let route = Effects::default();
            let scope = Scope { project: "shop", environment: "production" };
            let started = std::time::Instant::now();
            let mut bytes = 0;
            for _ in 0..n {
                let mut headers = visitor.clone();
                crate::proxy::nextjs::request_headers(&mut headers, None);
                let req = forkrail_common::rules::Request { method: "GET", path: "/blog/first", query: None, host: "next.test", headers: &headers };
                assert!(!site.draft_mode(&headers) && site.revalidation(&headers).is_none() && !site.middleware_runs(&req));
                let lookup = site.lookup(&req).unwrap();
                let own = site.config_headers(&req).unwrap();
                let (hash, entry, _) = entry_for(&cache, "dep-1", &lookup).await.unwrap();
                let (meta, at) = wire::Meta::of(&entry.body).unwrap();
                let range = meta.part(&lookup.part.name(), at).unwrap();
                assert_eq!(cache.freshness(&entry, cache::now_ms(), scope), Freshness::Fresh);
                let mut out = answer_headers(&entry.headers, &lookup, Some("dpl_1"), own);
                let etag = format!("W/\"{}\"", &cache::hash(&format!("{hash}\n{}\nhtml", entry.stored_ms))[..24]);
                out.insert(ETAG, HeaderValue::from_str(&etag).unwrap());
                let part = entry.body.slice(range);
                let gz = gzipped(&format!("{hash}\n{}\nhtml", entry.stored_ms), &part).unwrap();
                out.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
                let response = respond(&cache, entry.status, out, gz, entry.age(cache::now_ms()), Status::Hit, &Method::GET, Some(&headers), &route);
                bytes += response.headers().len();
            }
            println!("Next page from the store (checks, lookup, entry, headers, gzip from memory, answer): {:?} ({bytes})", started.elapsed() / n);
            let started = std::time::Instant::now();
            let part = Bytes::from(vec![b'x'; 20_000]);
            for i in 0..2_000 {
                gzipped(&format!("cold-{i}"), &part).unwrap();
            }
            println!("gzip of a 20 KB part, once per version of an entry: {:?}", started.elapsed() / 2_000);
        });
    }

    #[test]
    fn parts_are_gzipped_once() {
        let body = Bytes::from(vec![b'a'; 4096]);
        let a = gzipped("k1", &body).unwrap();
        assert!(a.len() < 100);
        assert_eq!(gzipped("k1", &Bytes::from_static(b"other")).unwrap(), a, "kept by the entry's version");
        assert!(compressible(Some(&HeaderValue::from_static("text/html; charset=utf-8"))));
        assert!(compressible(Some(&HeaderValue::from_static("text/x-component"))));
        assert!(!compressible(Some(&HeaderValue::from_static("image/png"))));
        let mut h = HeaderMap::new();
        h.insert(ACCEPT_ENCODING, HeaderValue::from_static("br, gzip;q=0.5"));
        assert!(accepts_gzip(&h));
        h.insert(ACCEPT_ENCODING, HeaderValue::from_static("br, gzip;q=0"));
        assert!(!accepts_gzip(&h));
    }
}
