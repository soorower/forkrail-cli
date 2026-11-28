//! The response cache's part of a request (`crate::cache` has Vercel's rules and the storage).
//! Before the function (`before`): a kept page or answer is served, a page that has never been
//! made is its build's fallback, a stale one is made again in the background; otherwise what's
//! to be kept of the function's answer goes in the request's `Effects`. After it (`after`): every
//! function answer gets the client's caching headers, and one that may be kept is kept as it
//! streams to the client.
//!
//! Pages are made by the home region's router (F4b, `cache/peers.rs`): an edge router asks it for
//! a page it hasn't got (and streams home's answer to its client as it comes), tells it of a stale
//! one, and sends it on-demand revalidations; home answers them here (`peer_renders`). While home
//! can't be reached, or doesn't know the deployment yet, an edge makes the page itself, as a
//! router alone does, and marks it its own (`local`).
//!
//! What's kept is the function's own answer; the deployment's routes add their headers each time
//! it's served, as they do to a function's answer.

use std::task::{Context, Poll, ready};

use hyper::header::{AUTHORIZATION, COOKIE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, RANGE};

mod nextjs;

use super::*;
use super::nextjs as nextjs_headers;
use crate::cache::peers::{Answer, Ask, Op, Reply};
use crate::cache::{self, Entry, Flight, FlightGuard, Freshness, Link, Meta, ResponseCache, Scope, Status};
use forkrail_common::rules::Prerender;
use forkrail_common::rules::prerender::{BYPASS_COOKIE, REVALIDATE_HEADER, REVALIDATE_IF_GENERATED_HEADER, ROUTE_MATCHES_HEADER};

/// How long an edge waits for home to say whether it has a page, when a fallback could be served
/// at once instead.
const QUICK_ASK: Duration = Duration::from_secs(3);

/// How a function's answer is kept, and what the client is told of the cache.
pub(super) struct Fill {
    label: Status,
    /// The request could be cached (GET or HEAD, no `Authorization`): the answer gets `age` and
    /// its CDN directives taken off, as Vercel's does; otherwise only the default headers.
    cacheable: bool,
    keep: Option<Keep>,
    /// The routes' status, headers and transforms, put on the answer once what to keep is taken.
    route: Box<Effects>,
    /// The caller reads the answer itself (`render`): passed on as the worker sent it.
    raw: bool,
    /// A Next.js deployment's answer (`nextjs.rs`): its `x-nextjs-cache` says what Next's cache
    /// did, and one of Next's cached pages is never kept as a CDN answer.
    next: bool,
    /// A public file Next's server answers (its middleware may match it): its `cache-control`
    /// goes out as the app set it, as Vercel serves a file.
    file: bool,
}

impl Fill {
    fn label(label: Status, cacheable: bool) -> Fill {
        Fill { label, cacheable, keep: None, route: Box::default(), raw: false, next: false, file: false }
    }
}

/// What an answer would be kept as.
struct Keep {
    deployment: String,
    what: What,
    /// The request making this page, which others wait on: let go once it's kept or not.
    guard: Option<FlightGuard>,
    /// The router whose request this page answers (home, for an edge's ask): not pushed to it.
    from: Option<String>,
    /// Made by an edge while home couldn't be reached.
    local: bool,
}

enum What {
    Page { hash: String, group: Option<u64>, expiration: Option<u64> },
    /// A CDN answer: its key without the headers its `Vary` will name, and the request's headers.
    Cdn { base: String, request: HeaderMap },
}

/// The routes' part of a request's effects, taken out of it: put on the answer later. A Routing
/// Middleware's own headers (`added`, a cookie it sets) go on every answer, a cached one too, as
/// the middleware runs on every request; they never decide whether the answer is kept.
fn route_of(effects: &mut Effects) -> Box<Effects> {
    Box::new(Effects {
        status: effects.status.take(),
        headers: std::mem::take(&mut effects.headers),
        transforms: std::mem::take(&mut effects.transforms),
        added: std::mem::take(&mut effects.added),
        ..Effects::default()
    })
}

/// The project and environment of a pool's deployment: what purges by tag are of.
fn scope_of(pool: &Pool) -> Scope<'_> {
    Scope { project: pool.project(), environment: &pool.config().environment }
}

/// `FORKRAIL_PAGES_VISITOR_HEADERS` (main.rs): `strip` (the default) makes pages without the
/// visitor's `Cookie` and `Authorization`, so a page that reads them can't keep one visitor's view
/// for all; `pass` sends them, as the request came.
pub(crate) fn pages_strip_visitor() -> bool {
    static STRIP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *STRIP.get_or_init(|| std::env::var("FORKRAIL_PAGES_VISITOR_HEADERS").map_or(true, |v| v != "pass"))
}

/// The request goes to the function as it is, its answer told `label`.
fn pass(effects: &mut Effects, log: &mut AccessLog, label: Status, cacheable: bool) -> Option<Response<ResponseBody>> {
    effects.cache = Some(Fill::label(label, cacheable));
    log.cache = Some(label.as_str());
    None
}

pub(super) async fn before(
    proxy: &Arc<Proxy>,
    cache: &Arc<ResponseCache>,
    pool: &Arc<Pool>,
    static_files: Option<&str>,
    head: &mut UpstreamHead,
    effects: &mut Effects,
    log: &mut AccessLog,
) -> Option<Response<ResponseBody>> {
    // The default pool's hosts aren't a deployment: nothing of theirs is kept.
    let deployment = log.deployment.clone()?;
    // A Next.js deployment's: its middleware, Draft Mode and N2's store decide (`nextjs.rs`).
    if let Some(next) = effects.next.clone() {
        return nextjs::before(proxy, cache, pool, &next, head, effects, log).await;
    }
    match effects.prerender.clone() {
        Some(prerender) => page(proxy, cache, pool, static_files, &deployment, &prerender, head, effects, log).await,
        None => cdn(proxy, cache, pool, &deployment, head, effects, log).await,
    }
}

/// A prerender's request (Vercel's ISR).
#[allow(clippy::too_many_arguments)]
async fn page(
    proxy: &Arc<Proxy>,
    cache: &Arc<ResponseCache>,
    pool: &Arc<Pool>,
    static_files: Option<&str>,
    deployment: &str,
    prerender: &Prerender,
    head: &mut UpstreamHead,
    effects: &mut Effects,
    log: &mut AccessLog,
) -> Option<Response<ResponseBody>> {
    let method = head.method.clone();
    let original = head.headers.clone();
    let token = prerender.bypass_hash.as_deref();
    // What the function gets: its query (`allowQuery`, with `passQuery`), the route's captures,
    // and nothing that would make it answer in part, compressed or "not modified": the page is
    // kept whole and served to everyone.
    let query = head.uri.query().unwrap_or("").to_owned();
    let key_query = prerender.key_query(&query);
    head.uri = with_query(&head.uri, &prerender.function_query(&query));
    if let Some(matches) = prerender.route_matches(effects.captures.as_deref())
        && let Ok(value) = HeaderValue::from_str(&matches)
    {
        head.headers.insert(HeaderName::from_static(ROUTE_MATCHES_HEADER), value);
    }
    for name in [ACCEPT_ENCODING, IF_NONE_MATCH, IF_MODIFIED_SINCE, RANGE] {
        head.headers.remove(name);
    }
    // Draft Mode: the function every time, nothing kept.
    let draft = token.is_some_and(|t| cache::cookie(&original, BYPASS_COOKIE).is_some_and(|c| cache::token_matches(t, c)));
    if draft {
        return pass(effects, log, Status::Bypass, true);
    }
    if !matches!(method, Method::GET | Method::HEAD) {
        return pass(effects, log, Status::Miss, false);
    }
    // A page is made once for everyone: unless the install says otherwise, without the cookies or
    // `Authorization` of whoever's request makes it (Draft Mode, above, still gets them).
    if pages_strip_visitor() {
        head.headers.remove(COOKIE);
        head.headers.remove(AUTHORIZATION);
    }
    let hash = cache::hash(&cache::page_key(deployment, &log.host, &prerender.path, &key_query));
    // A page this router makes itself is its own when home makes them (it only does while home
    // can't be reached).
    let local = cache.is_edge();
    let keep = |guard| Keep {
        deployment: deployment.to_owned(),
        what: What::Page { hash: hash.clone(), group: prerender.group, expiration: prerender.expiration },
        guard,
        from: None,
        local,
    };
    let template = Ask {
        op: Op::Fetch,
        host: head.host.clone(),
        deployment: deployment.to_owned(),
        function: prerender.path.clone(),
        max_duration_ms: effects.max_duration.map(|d| d.as_millis() as u64),
        hash: hash.clone(),
        have: None,
        wait: false,
        render: false,
        if_generated: false,
        uri: head.uri.to_string(),
        headers: head.headers.clone(),
        request_id: log.request_id.clone(),
        from: String::new(),
    };
    let ask = |link: &Link, op: Op| Ask { op, from: link.id.clone(), ..template.clone() };
    let limit = pool.max_duration() + Duration::from_secs(10);

    // On demand: made now, while the client waits, and its answer is the new page. Made by home
    // when there's one to ask.
    let revalidate = token.is_some_and(|t| original.get(REVALIDATE_HEADER).and_then(|v| v.to_str().ok()).is_some_and(|v| cache::token_matches(t, v)));
    if revalidate {
        let if_generated = original.contains_key(REVALIDATE_IF_GENERATED_HEADER);
        if let Some(link) = cache.home() {
            let mut asked = ask(link, Op::Revalidate);
            asked.if_generated = if_generated;
            match link.ask(&asked, limit).await {
                Ok(Reply::NotFound) => {
                    log.cache = Some(Status::Miss.as_str());
                    return Some(log_now(log, coded(StatusCode::NOT_FOUND, "NOT_FOUND", "The page could not be found."), "cache"));
                }
                Ok(Reply::Failed) => return Some(log_now(log, revalidation_failed(), "cache")),
                Ok(Reply::Page { kept, meta, headers, body, .. }) => {
                    let Some(entry) = collect_entry(&meta, headers, body).await else { return Some(log_now(log, revalidation_failed(), "cache")) };
                    if kept {
                        cache.receive(&hash, entry.clone());
                        cache.revalidated(deployment, entry.group, entry.stored_ms);
                    }
                    let label = if kept { Status::Revalidated } else { Status::Miss };
                    let route = route_of(effects);
                    log.function = None;
                    log.cache = Some(label.as_str());
                    let response = respond(cache, entry.status, entry.headers, entry.body, 0, label, &method, None, &route);
                    return Some(log_now(log, response, "cache"));
                }
                // Home can't do it: made here.
                Ok(Reply::None { .. } | Reply::Unknown) | Err(_) => {}
            }
        }
        if if_generated && cache.get(deployment, &hash, true).await.is_none() {
            log.cache = Some(Status::Miss.as_str());
            return Some(log_now(log, coded(StatusCode::NOT_FOUND, "NOT_FOUND", "The page could not be found."), "cache"));
        }
        let route = route_of(effects);
        let mut own = std::mem::replace(log, AccessLog::spent());
        own.cache = Some(Status::Revalidated.as_str());
        let rendered = render(proxy, cache, pool, copy(head, Method::GET), render_effects(effects), own, keep(None)).await;
        return Some(match rendered {
            Some(r) => {
                if let Some(kept) = &r.kept {
                    cache.revalidated(deployment, prerender.group, kept.stored_ms);
                }
                let label = if r.kept.is_some() { Status::Revalidated } else { Status::Miss };
                respond(cache, r.status, r.headers, r.body, 0, label, &method, None, &route)
            }
            None => revalidation_failed(),
        });
    }

    let scope = scope_of(pool);
    // A page a purge deleted is made again while the client waits, told `REVALIDATED`; never its
    // build's fallback, which is older still.
    let mut deleted = false;
    if let Some(entry) = cache.get(deployment, &hash, true).await {
        let label = match cache.freshness(&entry, cache::now_ms(), scope) {
            // A page made here while home was away: home's newer one asked for, if it's back.
            Freshness::Fresh => {
                if entry.local
                    && let Some(link) = cache.home()
                    && let Some(guard) = cache.lead(&hash)
                {
                    let mut asked = ask(link, Op::Refresh);
                    asked.have = Some(entry.stored_ms);
                    Remake::new(proxy, cache, pool, head, effects, log, keep(Some(guard)), None, hash.clone()).spawn(Some((link.clone(), asked)));
                }
                Status::Hit
            }
            // Served as it is while it's made again, once at a time: by home when there's one.
            Freshness::Stale | Freshness::Expired => {
                if let Some(guard) = cache.lead(&hash) {
                    let home = cache.home().map(|link| {
                        let mut asked = ask(link, Op::Refresh);
                        asked.have = Some(entry.stored_ms);
                        (link.clone(), asked)
                    });
                    Remake::new(proxy, cache, pool, head, effects, log, keep(Some(guard)), Some(entry.clone()), hash.clone()).spawn(home);
                }
                Status::Stale
            }
            Freshness::Deleted => {
                deleted = true;
                Status::Revalidated
            }
        };
        if !deleted {
            return Some(serve(cache, &entry, label, &method, &original, effects, log));
        }
    }
    let made = if deleted { Status::Revalidated } else { Status::Miss };

    // None here. An edge asks home: it has it, or makes it.
    if let Some(link) = cache.home() {
        let fallback = !deleted && prerender.fallback.is_some() && fallback_file(proxy, prerender, static_files).await;
        if fallback || method == Method::HEAD {
            // Only whether home has it: a fallback (or, for a HEAD, the function) answers at once
            // otherwise. Requests meanwhile wait for the one asking.
            let guard = match cache.flight(&hash) {
                Flight::Leader(guard) => Some(guard),
                Flight::Follower(mut done) => {
                    let _ = tokio::time::timeout(QUICK_ASK, done.wait_for(|d| *d)).await;
                    if let Some(entry) = cache.get(deployment, &hash, true).await
                        && let Some(label) = label_of(cache, &entry, scope)
                    {
                        return Some(serve(cache, &entry, label, &method, &original, effects, log));
                    }
                    // Home has none: the one that asked had it made there if it's to be.
                    if method == Method::HEAD {
                        return pass(effects, log, Status::Miss, true);
                    }
                    if let Some(response) = fallback_page(proxy, cache, prerender, static_files, &method, &original, effects, log, cache.lost(deployment)).await {
                        return Some(response);
                    }
                    None
                }
            };
            if guard.is_some() {
                let mut asked = ask(link, Op::Fetch);
                asked.render = fallback && (prerender.expiration.is_some() || cache.lost(deployment));
                match link.ask(&asked, QUICK_ASK).await {
                    Ok(Reply::Page { kept: true, meta, headers, body, .. }) => {
                        if let Some(entry) = collect_entry(&meta, headers, body).await {
                            cache.receive(&hash, entry);
                            drop(guard);
                            if let Some(entry) = cache.get(deployment, &hash, true).await
                                && let Some(label) = label_of(cache, &entry, scope)
                            {
                                return Some(serve(cache, &entry, label, &method, &original, effects, log));
                            }
                        }
                    }
                    // Home has none, and is making it (and will push it here) if it expires or
                    // may have been lost.
                    Ok(Reply::None { lost, .. }) if fallback => {
                        if let Some(response) = fallback_page(proxy, cache, prerender, static_files, &method, &original, effects, log, lost).await {
                            return Some(response);
                        }
                    }
                    Ok(Reply::None { .. }) if method == Method::HEAD => return pass(effects, log, Status::Miss, true),
                    _ => {}
                }
            }
        } else {
            // Made by home while the client waits, streamed here as it comes; others asking
            // meanwhile wait for it, as on home itself.
            match cache.flight(&hash) {
                Flight::Leader(guard) => {
                    let mut asked = ask(link, Op::Fetch);
                    asked.wait = true;
                    match link.ask(&asked, limit).await {
                        Ok(Reply::Page { kept, made: fresh, meta, headers, body }) => {
                            return Some(streamed(cache, &hash, kept, fresh, made, scope, meta, headers, body, guard, &method, effects, log));
                        }
                        // Made here instead, with this request's guard.
                        _ => return fill_here(keep(Some(guard)), made, effects, log),
                    }
                }
                Flight::Follower(mut done) => {
                    let _ = tokio::time::timeout(limit, done.wait_for(|d| *d)).await;
                    if let Some(entry) = cache.get(deployment, &hash, true).await
                        && cache.freshness(&entry, cache::now_ms(), scope) != Freshness::Deleted
                    {
                        return Some(serve(cache, &entry, Status::Hit, &method, &original, effects, log));
                    }
                    return fill_here(keep(None), made, effects, log);
                }
            }
        }
    }

    // Never made: the build's fallback page, while the function makes the page (only for a page
    // that expires: one with `expiration: false` keeps its fallback until it's revalidated, as
    // Vercel's on-demand ISR example expects, unless every copy of the pages was lost since).
    let lost = cache.lost(deployment);
    if prerender.fallback.is_some()
        && !deleted
        && let Some(response) = fallback_page(proxy, cache, prerender, static_files, &method, &original, effects, log, lost).await
    {
        if (prerender.expiration.is_some() || lost)
            && let Some(guard) = cache.lead(&hash)
        {
            Remake::new(proxy, cache, pool, head, effects, log, keep(Some(guard)), None, hash.clone()).spawn(None);
        }
        return Some(response);
    }
    // A HEAD has no page to keep.
    if method == Method::HEAD {
        return pass(effects, log, Status::Miss, true);
    }
    // Made while the client waits; others asking meanwhile wait for it (Vercel's request
    // collapsing).
    let guard = match cache.flight(&hash) {
        Flight::Leader(guard) => Some(guard),
        Flight::Follower(mut done) => {
            let _ = tokio::time::timeout(pool.max_duration(), done.wait_for(|d| *d)).await;
            if let Some(entry) = cache.get(deployment, &hash, true).await
                && cache.freshness(&entry, cache::now_ms(), scope) != Freshness::Deleted
            {
                return Some(serve(cache, &entry, Status::Hit, &method, &original, effects, log));
            }
            // What it waited for kept nothing: this one goes on its own.
            None
        }
    };
    fill_here(keep(guard), made, effects, log)
}

/// The function's answer to this request, kept as it streams to the client, told `label` (`MISS`,
/// or `REVALIDATED` for a page a purge deleted).
fn fill_here(keep: Keep, label: Status, effects: &mut Effects, log: &mut AccessLog) -> Option<Response<ResponseBody>> {
    let route = route_of(effects);
    effects.cache = Some(Fill { label, cacheable: true, keep: Some(keep), route, raw: false, next: false, file: false });
    log.cache = Some(label.as_str());
    None
}

fn revalidation_failed() -> Response<ResponseBody> {
    coded(StatusCode::BAD_GATEWAY, "ROUTER_REVALIDATION_FAILED", "The page couldn't be made again.")
}

/// `x-vercel-cache` for a kept page, served now; `None` for one a purge deleted, which isn't served.
fn label_of(cache: &ResponseCache, entry: &Entry, scope: Scope) -> Option<Status> {
    match cache.freshness(entry, cache::now_ms(), scope) {
        Freshness::Fresh => Some(Status::Hit),
        Freshness::Stale | Freshness::Expired => Some(Status::Stale),
        Freshness::Deleted => None,
    }
}

/// A page as it came from home: its meta, headers and body.
fn entry_of(meta: &Meta, headers: HeaderMap, body: Bytes) -> Option<Entry> {
    Some(Entry {
        status: StatusCode::from_u16(meta.status).ok()?,
        headers,
        body,
        stored_ms: meta.stored_ms,
        fresh_until: meta.fresh_until,
        stale_until: None,
        deployment: meta.deployment.clone(),
        group: meta.group,
        page: true,
        local: false,
    })
}

/// A page from home, read in full (up to the largest page).
async fn collect_entry(meta: &Meta, headers: HeaderMap, mut body: ResponseBody) -> Option<Entry> {
    let mut bytes = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame.ok()?.into_data() else { continue };
        bytes.extend_from_slice(&data);
        if bytes.len() > cache::MAX_PAGE_BYTES {
            return None;
        }
    }
    entry_of(meta, headers, bytes.freeze())
}

/// Home's page, made for this request (or kept from before), streamed to the client as it comes,
/// and kept here once it has all come.
#[allow(clippy::too_many_arguments)]
fn streamed(
    cache: &Arc<ResponseCache>,
    hash: &str,
    kept: bool,
    made: bool,
    made_label: Status,
    scope: Scope,
    meta: Meta,
    headers: HeaderMap,
    body: ResponseBody,
    guard: FlightGuard,
    method: &Method,
    effects: &mut Effects,
    log: &mut AccessLog,
) -> Response<ResponseBody> {
    let route = route_of(effects);
    let mut entry = entry_of(&meta, headers.clone(), Bytes::new());
    let label = match (&entry, made, kept) {
        (Some(e), false, true) => label_of(cache, e, scope).unwrap_or(made_label),
        _ => made_label,
    };
    let age = entry.as_ref().filter(|_| !made).map_or(0, |e| e.age(cache::now_ms()));
    let mut status = StatusCode::from_u16(meta.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut client = headers;
    route.apply(&mut status, &mut client);
    cache::client_headers(&mut client, cache.defaults);
    if cache.defaults || label != Status::Miss {
        cache::mark(&mut client, label, age);
    }
    let body = match entry.take().filter(|_| kept) {
        Some(entry) => {
            let expected = client.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok());
            Tee { inner: body, kept: Vec::new(), len: 0, limit: cache::MAX_PAGE_BYTES, expected, planned: Some((hash.to_owned(), entry)), cache: cache.clone(), guard: Some(guard), from: None }.boxed()
        }
        None => body,
    };
    let body = if *method == Method::HEAD { Full::new(Bytes::new()).map_err(|never| match never {}).boxed() } else { body };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = client;
    log.function = None;
    log.cache = Some(label.as_str());
    log_now(log, response, "cache")
}

/// Whether a prerender's fallback page is among the deployment's static files.
async fn fallback_file(proxy: &Proxy, prerender: &Prerender, static_files: Option<&str>) -> bool {
    let (Some(statics), Some(id), Some(name)) = (proxy.statics.as_ref(), static_files, prerender.fallback.as_deref()) else { return false };
    statics.manifest(id).await.is_some_and(|m| m.files.contains_key(name))
}

/// A page or answer made again in the background, by home (for an edge that can reach it) or
/// here: a page that fails stays as it was for 30 s more (Vercel keeps stale content and tries
/// again then).
struct Remake {
    proxy: Arc<Proxy>,
    cache: Arc<ResponseCache>,
    pool: Arc<Pool>,
    head: UpstreamHead,
    effects: Effects,
    log: AccessLog,
    keep: Keep,
    stale: Option<Arc<Entry>>,
    hash: String,
}

impl Remake {
    #[allow(clippy::too_many_arguments)]
    fn new(proxy: &Arc<Proxy>, cache: &Arc<ResponseCache>, pool: &Arc<Pool>, head: &UpstreamHead, effects: &Effects, log: &AccessLog, keep: Keep, stale: Option<Arc<Entry>>, hash: String) -> Remake {
        let mut head = copy(head, Method::GET);
        head.headers.remove(IF_NONE_MATCH);
        head.headers.remove(IF_MODIFIED_SINCE);
        Remake { proxy: proxy.clone(), cache: cache.clone(), pool: pool.clone(), head, effects: render_effects(effects), log: background(log, pool), keep, stale, hash }
    }

    /// In the background: asked of home first when `home` says how.
    fn spawn(self, home: Option<(Arc<Link>, Ask)>) {
        tokio::spawn(async move {
            match home {
                Some((link, ask)) => self.by_home(&link, &ask).await,
                None => self.here().await,
            }
        });
    }

    async fn by_home(mut self, link: &Link, ask: &Ask) {
        let limit = self.pool.max_duration() + Duration::from_secs(10);
        match link.ask(ask, limit).await {
            Ok(Reply::Page { kept: true, meta, headers, body, .. }) => {
                if let Some(entry) = collect_entry(&meta, headers, body).await {
                    self.cache.receive(&self.hash, entry);
                    self.log.written = true;
                    return;
                }
                self.failed(None);
            }
            Ok(Reply::Page { kept: false, meta, .. }) => self.failed(Some(meta.status)),
            Ok(Reply::Failed) => self.failed(None),
            // Being made there, and pushed here when it's done.
            Ok(Reply::None { .. }) => self.log.written = true,
            Ok(Reply::NotFound | Reply::Unknown) | Err(_) => self.here().await,
        }
    }

    fn failed(mut self, status: Option<u16>) {
        self.log.written = true;
        if let Some(stale) = &self.stale {
            warn!(deployment = %stale.deployment, status, "home couldn't make a page again; the stale one stays for 30 s");
            self.cache.retry_later(&self.hash, stale);
        }
    }

    async fn here(self) {
        let Remake { proxy, cache, pool, head, effects, log, keep, stale, hash } = self;
        let rendered = render(&proxy, &cache, &pool, head, effects, log, keep).await;
        if rendered.as_ref().is_none_or(|r| r.kept.is_none())
            && let Some(stale) = stale
        {
            warn!(deployment = %stale.deployment, status = rendered.as_ref().map(|r| r.status.as_u16()), "a page couldn't be made again; the stale one stays for 30 s");
            cache.retry_later(&hash, &stale);
        }
    }
}

// ---- Home, for the other routers ----

/// What home answers an edge's `Ask` with (`cache/peers.rs`), making pages with its own pool for
/// the deployment, as for its own clients.
pub(crate) fn peer_renders(proxy: Arc<Proxy>) -> crate::cache::peers::Renders {
    Arc::new(move |ask| {
        let proxy = proxy.clone();
        Box::pin(async move { answer_peer(&proxy, ask).await })
    })
}

async fn answer_peer(proxy: &Arc<Proxy>, ask: Ask) -> Answer {
    let Some(cache) = proxy.cache.clone() else { return Answer::Unknown };
    if ask.op == Op::Regenerate {
        return nextjs::answer_regenerate(proxy, &ask).await;
    }
    let host = normalize_host(&ask.host);
    let Some(target) = proxy.registry.deployment_target(&host, &ask.deployment) else { return Answer::Unknown };
    let Some(pool) = target.pool.clone() else { return Answer::Unknown };
    let Some(prerender) = prerender_of(proxy, &target, &ask.function).await else { return Answer::Unknown };
    let Ok(uri) = ask.uri.parse::<Uri>() else { return Answer::Unknown };
    let mut head = UpstreamHead { host: ask.host.clone(), method: Method::GET, uri, headers: ask.headers.clone(), payload: 0 };
    // As an edge strips them (`page`), here too: one from before the switch may still send them.
    if pages_strip_visitor() {
        head.headers.remove(COOKIE);
        head.headers.remove(AUTHORIZATION);
    }
    let effects = Effects { function: Some(ask.function.clone()), max_duration: ask.max_duration_ms.map(Duration::from_millis), ..Effects::default() };
    let log = peer_log(&ask, &host, &target, &pool, &head);
    let scope = scope_of(&pool);
    let keep = |guard, from: Option<String>| Keep {
        deployment: ask.deployment.clone(),
        what: What::Page { hash: ask.hash.clone(), group: prerender.group, expiration: prerender.expiration },
        guard,
        from,
        local: false,
    };
    let page = |entry: &Entry, made: bool| Answer::Page { kept: true, made, body: crate::cache::peers::page_body(Bytes::from(cache::encode(entry, &ask.hash))) };
    let wait_for = |mut done: tokio::sync::watch::Receiver<bool>| {
        let limit = pool.max_duration();
        async move {
            let _ = tokio::time::timeout(limit, done.wait_for(|d| *d)).await;
        }
    };
    match ask.op {
        Op::Fetch => {
            let mut deleted = false;
            if let Some(entry) = cache.get(&ask.deployment, &ask.hash, true).await {
                match cache.freshness(&entry, cache::now_ms(), scope) {
                    // Deleted by a purge here too: made again, as if there were none.
                    Freshness::Deleted => deleted = true,
                    freshness => {
                        // Stale here too: made again, as for a client of home's own.
                        if freshness != Freshness::Fresh
                            && let Some(guard) = cache.lead(&ask.hash)
                        {
                            Remake::new(proxy, &cache, &pool, &head, &effects, &log, keep(Some(guard), None), Some(entry.clone()), ask.hash.clone()).spawn(None);
                        }
                        discard(log);
                        return page(&entry, false);
                    }
                }
            }
            if ask.wait {
                return match cache.flight(&ask.hash) {
                    Flight::Leader(guard) => render_streamed(proxy, &cache, &pool, head, effects, log, keep(Some(guard), Some(ask.from.clone()))).await,
                    Flight::Follower(done) => {
                        wait_for(done).await;
                        match cache.get(&ask.deployment, &ask.hash, true).await {
                            Some(entry) => {
                                discard(log);
                                page(&entry, false)
                            }
                            None => render_streamed(proxy, &cache, &pool, head, effects, log, keep(None, Some(ask.from.clone()))).await,
                        }
                    }
                };
            }
            // Lost only matters to a page with a fallback, which the asker serves meanwhile.
            let lost = prerender.fallback.is_some() && cache.lost(&ask.deployment);
            if ask.render || lost || deleted {
                if let Some(guard) = cache.lead(&ask.hash) {
                    Remake::new(proxy, &cache, &pool, &head, &effects, &log, keep(Some(guard), None), None, ask.hash.clone()).spawn(None);
                }
                discard(log);
                return Answer::None { rendering: true, lost };
            }
            discard(log);
            Answer::None { rendering: false, lost: false }
        }
        Op::Refresh => {
            let have = ask.have.unwrap_or(0);
            let current = cache.get(&ask.deployment, &ask.hash, true).await;
            if let Some(entry) = &current
                && entry.stored_ms > have
                && cache.freshness(entry, cache::now_ms(), scope) == Freshness::Fresh
            {
                discard(log);
                return page(entry, false);
            }
            match cache.flight(&ask.hash) {
                Flight::Leader(guard) => {
                    let mut head = head;
                    head.headers.remove(IF_NONE_MATCH);
                    head.headers.remove(IF_MODIFIED_SINCE);
                    match render(proxy, &cache, &pool, head, effects, log, keep(Some(guard), Some(ask.from.clone()))).await {
                        Some(Rendered { kept: Some(entry), .. }) => page(&entry, true),
                        rendered => {
                            if let Some(stale) = current {
                                warn!(deployment = %ask.deployment, status = rendered.as_ref().map(|r| r.status.as_u16()), "a page couldn't be made again; the stale one stays for 30 s");
                                cache.retry_later(&ask.hash, &stale);
                            }
                            Answer::Failed
                        }
                    }
                }
                Flight::Follower(done) => {
                    discard(log);
                    wait_for(done).await;
                    match cache.get(&ask.deployment, &ask.hash, true).await {
                        Some(entry) if entry.stored_ms > have => page(&entry, false),
                        _ => Answer::Failed,
                    }
                }
            }
        }
        // Answered before the page's own work (`nextjs::answer_regenerate`).
        Op::Regenerate => {
            discard(log);
            Answer::Unknown
        }
        Op::Revalidate => {
            if ask.if_generated && cache.get(&ask.deployment, &ask.hash, true).await.is_none() {
                discard(log);
                return Answer::NotFound;
            }
            let mut log = log;
            log.cache = Some(Status::Revalidated.as_str());
            match render(proxy, &cache, &pool, head, effects, log, keep(None, Some(ask.from.clone()))).await {
                Some(r) => match &r.kept {
                    Some(kept) => {
                        cache.revalidated(&ask.deployment, kept.group, kept.stored_ms);
                        page(kept, true)
                    }
                    // An answer that isn't a page (a 500): the asker serves it once, as here.
                    None => {
                        let mut own = own_headers(&r.headers);
                        own.remove(CONTENT_LENGTH);
                        let meta = Meta { status: r.status.as_u16(), stored_ms: cache::now_ms(), deployment: ask.deployment.clone(), hash: ask.hash.clone(), ..Meta::default() };
                        let mut bytes = cache::encode_head(&meta, &own);
                        bytes.extend_from_slice(&r.body);
                        Answer::Page { kept: false, made: true, body: crate::cache::peers::page_body(Bytes::from(bytes)) }
                    }
                },
                None => Answer::Failed,
            }
        }
    }
}

/// A peer's log line that ended up not needed (the page was there): not written.
fn discard(mut log: AccessLog) {
    log.written = true;
}

/// The prerender `function` is in `target`'s routing program.
async fn prerender_of(proxy: &Proxy, target: &crate::registry::Target, function: &str) -> Option<Arc<Prerender>> {
    let statics = proxy.statics.as_ref()?;
    let engine = statics.program(target.rules.as_deref(), target.project_routing.routing.as_deref(), target.pool.is_some()).await.ok()?;
    engine.prerender(function).cloned()
}

/// The access log line of a render home makes for another router: the asker's request id, so the
/// two routers' lines can be matched.
fn peer_log(ask: &Ask, host: &str, target: &crate::registry::Target, pool: &Pool, head: &UpstreamHead) -> AccessLog {
    let mut log = AccessLog::spent();
    log.request_id = if ask.request_id.is_empty() { forkrail_common::quick_id() } else { ask.request_id.clone() };
    log.entry = super::Entry::Https;
    log.host = host.to_owned();
    log.deployment = Some(ask.deployment.clone());
    log.project = target.project.clone();
    log.region = target.region.clone();
    log.path = head.uri.path().to_owned();
    log.query = head.uri.query().filter(|q| !q.is_empty()).map(str::to_owned);
    log.function = Some(FunctionLog::of(pool.config()));
    log.cache = Some("peer");
    log.started = Instant::now();
    log.written = false;
    log
}

/// The function's answer to an edge's request, streamed to it as it comes (the page's head, then
/// its body), and kept here once it has all come, if it can be.
async fn render_streamed(proxy: &Arc<Proxy>, cache: &Arc<ResponseCache>, pool: &Arc<Pool>, mut head: UpstreamHead, effects: Effects, mut log: AccessLog, keep: Keep) -> Answer {
    if let Ok(id) = HeaderValue::from_str(&log.request_id) {
        head.headers.insert(REQUEST_ID, id);
    }
    pool.logs().begin(&log.request_id);
    log.app_logs = Some(pool.logs().clone());
    let (tx, rx) = oneshot::channel();
    let effects = Effects { cache: Some(Fill { label: Status::Miss, cacheable: true, keep: None, route: Box::default(), raw: true, next: false, file: false }), ..effects };
    let body = UpstreamBody::Buffered { bytes: Bytes::new(), _room: None };
    tokio::spawn(exchange(head, body, pool.clone(), Version::HTTP_11, proxy.clone(), log, effects, tx));
    let Ok(response) = rx.await else { return Answer::Failed };
    let (parts, body) = response.into_parts();
    let own = own_headers(&parts.headers);
    let now = cache::now_ms();
    let (hash, deployment) = match &keep.what {
        What::Page { hash, .. } => (hash.clone(), keep.deployment.clone()),
        What::Cdn { .. } => return Answer::Unknown,
    };
    match plan(cache, &keep, parts.status, own.clone(), &own, now) {
        Some((hash, entry, limit)) => {
            let meta = Meta { status: entry.status.as_u16(), stored_ms: entry.stored_ms, fresh_until: entry.fresh_until, group: entry.group, deployment, hash: hash.clone(), local: false };
            let head = Bytes::from(cache::encode_head(&meta, &entry.headers));
            let expected = parts.headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok());
            let Keep { guard, from, .. } = keep;
            let tee = Tee { inner: body, kept: Vec::new(), len: 0, limit, expected, planned: Some((hash, entry)), cache: cache.clone(), guard, from };
            Answer::Page { kept: true, made: true, body: Prefixed { head: Some(head), inner: tee.boxed() }.boxed() }
        }
        None => {
            let mut own = own;
            own.remove(CONTENT_LENGTH);
            let meta = Meta { status: parts.status.as_u16(), stored_ms: now, deployment, hash, ..Meta::default() };
            let head = Bytes::from(cache::encode_head(&meta, &own));
            drop(keep);
            Answer::Page { kept: false, made: true, body: Prefixed { head: Some(head), inner: body }.boxed() }
        }
    }
}

/// A body after bytes of its own: a page's head before the function's answer.
struct Prefixed {
    head: Option<Bytes>,
    inner: ResponseBody,
}

impl Body for Prefixed {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some(head) = self.head.take() {
            return Poll::Ready(Some(Ok(Frame::data(head))));
        }
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.head.is_none() && self.inner.is_end_stream()
    }
}

/// Any other function's request: its answer is kept if its headers ask for it (Vercel's CDN).
async fn cdn(
    proxy: &Arc<Proxy>,
    cache: &Arc<ResponseCache>,
    pool: &Arc<Pool>,
    deployment: &str,
    head: &mut UpstreamHead,
    effects: &mut Effects,
    log: &mut AccessLog,
) -> Option<Response<ResponseBody>> {
    let method = head.method.clone();
    if !matches!(method, Method::GET | Method::HEAD) {
        return pass(effects, log, Status::Miss, false);
    }
    if head.headers.contains_key(AUTHORIZATION) {
        return pass(effects, log, Status::Bypass, false);
    }
    let path_and_query = head.uri.path_and_query().map_or("/", |pq| pq.as_str()).to_owned();
    let base = cache::cdn_key(deployment, &method, &log.host, &path_and_query, &head.headers);
    let names = cache.vary_of(&base);
    let hash = cache::hash(&cache::with_vary(&base, names.as_deref().unwrap_or(&[]), &head.headers));
    let mut label = Status::Miss;
    if let Some(entry) = cache.get(deployment, &hash, false).await {
        let freshness = cache.freshness(&entry, cache::now_ms(), scope_of(pool));
        match freshness {
            Freshness::Fresh => return Some(serve(cache, &entry, Status::Hit, &method, &head.headers.clone(), effects, log)),
            Freshness::Stale => {
                if let Some(guard) = cache.lead(&hash) {
                    let keep = Keep { deployment: deployment.to_owned(), what: What::Cdn { base, request: head.headers.clone() }, guard: Some(guard), from: None, local: false };
                    Remake::new(proxy, cache, pool, head, effects, log, keep, None, hash).spawn(None);
                }
                return Some(serve(cache, &entry, Status::Stale, &method, &head.headers.clone(), effects, log));
            }
            // Made again while the client waits, whole: the function sees no validators. A purge's
            // delete is told `REVALIDATED` (Vercel's "Tag-based deletion"), with no request
            // collapsing, as on Vercel.
            Freshness::Expired | Freshness::Deleted => {
                head.headers.remove(IF_NONE_MATCH);
                head.headers.remove(IF_MODIFIED_SINCE);
                if freshness == Freshness::Deleted {
                    label = Status::Revalidated;
                }
            }
        }
    }
    let route = route_of(effects);
    let keep = Keep { deployment: deployment.to_owned(), what: What::Cdn { base, request: head.headers.clone() }, guard: None, from: None, local: false };
    effects.cache = Some(Fill { label, cacheable: true, keep: Some(keep), route, raw: false, next: false, file: false });
    log.cache = Some(label.as_str());
    None
}

/// A function's answer on its way to the client (from `exchange`).
pub(super) fn after(proxy: &Proxy, fill: Option<Fill>, response: Response<ResponseBody>) -> Response<ResponseBody> {
    let Some(cache) = proxy.cache.clone() else {
        let (mut parts, body) = response.into_parts();
        cache::platform_headers(&mut parts.headers);
        if fill.as_ref().is_some_and(|f| f.next && !f.raw)
            && let Some(label) = nextjs_headers::take_label(&mut parts.headers)
        {
            parts.headers.insert(cache::STATUS_HEADER, HeaderValue::from_static(label.as_str()));
        }
        return Response::from_parts(parts, body);
    };
    let mut fill = fill.unwrap_or_else(|| Fill::label(Status::Miss, false));
    if fill.raw {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    // Next's own word on its cache, as Vercel's header (a request the router let by unlooked-at,
    // Draft Mode's, stays `BYPASS`); one of its cached pages isn't kept here, and an answer it
    // says is stale never is (decided now: the label is taken off before `plan` looks).
    let nexts_page = (fill.next && nextjs_headers::nexts_own(&parts.headers)) || stale_by_next(&parts.headers);
    if fill.next
        && let Some(label) = nextjs_headers::take_label(&mut parts.headers)
        && fill.label == Status::Miss
    {
        fill.label = label;
    }
    if !fill.cacheable {
        cache::uncacheable_headers(&mut parts.headers, fill.label, cache.defaults);
        return Response::from_parts(parts, body);
    }
    let planned = fill.keep.as_ref().filter(|_| !nexts_page).and_then(|keep| {
        let raw = own_headers(&parts.headers);
        let view = with_route_headers(&raw, &fill.route);
        plan(&cache, keep, parts.status, raw, &view, cache::now_ms())
    });
    fill.route.apply(&mut parts.status, &mut parts.headers);
    if fill.file {
        cache::platform_headers(&mut parts.headers);
        if cache.defaults && !parts.headers.contains_key(hyper::header::CACHE_CONTROL) {
            parts.headers.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static(cache::DEFAULT_CACHE_CONTROL));
        }
    } else {
        cache::client_headers(&mut parts.headers, cache.defaults);
    }
    // Next's redirect: no cache of anyone's answered it (Vercel's carry neither), and a 304 only
    // what Vercel's does.
    let redirect = fill.next && parts.status.is_redirection() && parts.headers.contains_key(hyper::header::LOCATION);
    if (cache.defaults || fill.label != Status::Miss) && !redirect {
        cache::mark(&mut parts.headers, fill.label, 0);
    }
    if redirect && !parts.headers.contains_key(CONTENT_TYPE) {
        parts.headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    }
    if fill.next && parts.status == StatusCode::NOT_MODIFIED {
        nextjs_headers::not_modified_headers(&mut parts.headers);
    }
    let Some((hash, entry, limit)) = planned else { return Response::from_parts(parts, body) };
    let (guard, from) = fill.keep.map_or((None, None), |k| (k.guard, k.from));
    // Nothing more to come (a HEAD's answer): kept now, as it is.
    if body.is_end_stream() {
        cache.put(&hash, entry, from.as_deref());
        drop(guard);
        return Response::from_parts(parts, body);
    }
    let expected = parts.headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok());
    let tee = Tee { inner: body, kept: Vec::new(), len: 0, limit, expected, planned: Some((hash, entry)), cache, guard, from };
    Response::from_parts(parts, tee.boxed())
}

/// Whether Next says its answer is stale (`x-nextjs-cache: STALE`): the page is being made again
/// behind it, so the answer is never kept, on any deployment (one without N3's entry too: a Next
/// app published before its routers knew Next, or whose entry an older CLI dropped). Next 13.5 to
/// 14.2 give a stale page's answer the page's whole lifetime (`s-maxage=<revalidate>`: their
/// response cache resolves a stale entry with its `curRevalidate`), so a kept copy served the old
/// page as fresh for that long after the new one was made (seen on 14.2 without N3's entry: a
/// page 30 s old served `HIT` for 5 s more, the new one 5 s late). On Vercel the next request
/// after the regeneration gets the new page.
fn stale_by_next(headers: &HeaderMap) -> bool {
    nextjs_headers::label(headers) == Some(Status::Stale)
}

/// The function's own headers, without what the router adds to every answer.
fn own_headers(headers: &HeaderMap) -> HeaderMap {
    let mut own = headers.clone();
    own.remove(REQUEST_ID);
    own.remove(DEPLOYMENT_HEADER);
    strip_hop_by_hop(&mut own);
    own
}

/// The answer's headers with the routes' where it has none of its own: what Vercel's CDN reads
/// (a `vercel.json` header can ask for caching too; the function's own wins).
fn with_route_headers(own: &HeaderMap, route: &Effects) -> HeaderMap {
    let mut view = own.clone();
    for (name, value) in &route.headers {
        if !view.contains_key(name) {
            view.insert(name.clone(), value.clone());
        }
    }
    view
}

/// Whether an answer is kept, as what (its hash and entry, without the body yet), and how large
/// its body may be.
fn plan(cache: &ResponseCache, keep: &Keep, status: StatusCode, own: HeaderMap, view: &HeaderMap, now: u64) -> Option<(String, Entry, usize)> {
    // The function's own cookie only: a route's (skew protection's `__vdpl` on every document
    // request) goes on each answer as it's served, so the page is kept and the cookie set per
    // request, as on Vercel (seen: an ISR page asked for as a document, HIT with the cookie).
    // Nor an answer Next says is stale (a refresh's included: `render` plans the raw answer).
    if cache::sets_cookie(&own) || stale_by_next(&own) {
        return None;
    }
    let entry = |fresh_until, stale_until, group, page| Entry {
        status,
        headers: own.clone(),
        body: Bytes::new(),
        stored_ms: now,
        fresh_until,
        stale_until,
        deployment: keep.deployment.clone(),
        group,
        page,
        local: keep.local,
    };
    match &keep.what {
        What::Page { hash, group, expiration } => {
            if !cache::page_status(status) {
                return None;
            }
            let fresh_until = expiration.map(|s| now + s.saturating_mul(1000));
            Some((hash.clone(), entry(fresh_until, None, *group, true), cache::MAX_PAGE_BYTES))
        }
        What::Cdn { base, request } => {
            if !cache::cdn_status(status) {
                return None;
            }
            let ttl = cache::cdn_ttl(view)?;
            if view.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<usize>().ok()).is_some_and(|n| n > cache::MAX_CDN_BYTES) {
                return None;
            }
            let names = cache::vary(view)?;
            let hash = cache::hash(&cache::with_vary(base, &names, request));
            cache.set_vary(base, names);
            let (fresh, stale) = ttl.entry_times(now);
            Some((hash, entry(fresh, stale, None, false), cache::MAX_CDN_BYTES))
        }
    }
}

/// A function's answer passed on to the client and kept once it has all come, if it isn't too
/// large and the client took it all.
struct Tee {
    inner: ResponseBody,
    kept: Vec<Bytes>,
    len: usize,
    limit: usize,
    expected: Option<u64>,
    planned: Option<(String, Entry)>,
    cache: Arc<ResponseCache>,
    guard: Option<FlightGuard>,
    /// The router this page answers (`Keep::from`).
    from: Option<String>,
}

impl Tee {
    fn finish(&mut self) {
        if let Some((hash, mut entry)) = self.planned.take() {
            let mut body = BytesMut::with_capacity(self.len);
            for part in self.kept.drain(..) {
                body.extend_from_slice(&part);
            }
            entry.body = body.freeze();
            self.cache.put(&hash, entry, self.from.as_deref());
        }
        self.guard.take();
    }
}

impl Body for Tee {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    this.len += data.len();
                    if this.len > this.limit {
                        this.planned = None;
                        this.kept.clear();
                    } else if this.planned.is_some() {
                        this.kept.push(data.clone());
                    }
                }
                if this.inner.is_end_stream() {
                    this.finish();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Some(Err(e)) => {
                this.planned = None;
                Poll::Ready(Some(Err(e)))
            }
            None => {
                this.finish();
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        // HTTP/1.1 stops once Content-Length bytes are out, without asking for the end.
        if self.expected == Some(self.len as u64) {
            self.finish();
        }
    }
}

/// An answer the router made the function give, read in full.
struct Rendered {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    /// What was kept of it, if it could be.
    kept: Option<Arc<Entry>>,
}

/// Run the function for `head` with no client waiting on the stream, read its answer and keep it
/// if it can be: a page's regeneration, an on-demand revalidation, a CDN answer's refresh. Logged
/// as `log` when the answer has all come.
async fn render(proxy: &Arc<Proxy>, cache: &Arc<ResponseCache>, pool: &Arc<Pool>, mut head: UpstreamHead, effects: Effects, mut log: AccessLog, keep: Keep) -> Option<Rendered> {
    if let Ok(id) = HeaderValue::from_str(&log.request_id) {
        head.headers.insert(REQUEST_ID, id);
    }
    pool.logs().begin(&log.request_id);
    log.app_logs = Some(pool.logs().clone());
    let (tx, rx) = oneshot::channel();
    let effects = Effects { cache: Some(Fill { label: Status::Miss, cacheable: true, keep: None, route: Box::default(), raw: true, next: false, file: false }), ..effects };
    let body = UpstreamBody::Buffered { bytes: Bytes::new(), _room: None };
    tokio::spawn(exchange(head, body, pool.clone(), Version::HTTP_11, proxy.clone(), log, effects, tx));
    let response = rx.await.ok()?;
    let (parts, mut body) = response.into_parts();
    let mut bytes = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame.ok()?.into_data() else { continue };
        bytes.extend_from_slice(&data);
        if bytes.len() > cache::MAX_PAGE_BYTES {
            return None;
        }
    }
    let own = own_headers(&parts.headers);
    let body = bytes.freeze();
    let kept = plan(cache, &keep, parts.status, own.clone(), &own, cache::now_ms()).and_then(|(hash, mut entry, limit)| {
        (body.len() <= limit).then(|| {
            entry.body = body.clone();
            cache.put(&hash, entry, keep.from.as_deref())
        })
    });
    drop(keep);
    Some(Rendered { status: parts.status, headers: parts.headers, body, kept })
}

/// What a render needs of the request's effects: which function, and its time limit (its query
/// is in the request already).
fn render_effects(effects: &Effects) -> Effects {
    Effects { function: effects.function.clone(), max_duration: effects.max_duration, ..Effects::default() }
}

fn copy(head: &UpstreamHead, method: Method) -> UpstreamHead {
    let mut headers = head.headers.clone();
    if method != head.method {
        headers.remove(CONTENT_LENGTH);
    }
    UpstreamHead { host: head.host.clone(), method, uri: head.uri.clone(), headers, payload: head.payload }
}

/// The access log line of a request the router makes itself, from the one that set it off.
fn background(log: &AccessLog, pool: &Pool) -> AccessLog {
    let mut own = AccessLog::spent();
    own.request_id = forkrail_common::quick_id();
    own.entry = log.entry;
    own.host = log.host.clone();
    own.deployment = log.deployment.clone();
    own.project = log.project.clone();
    own.region = log.region.clone();
    own.path = log.path.clone();
    own.query = log.query.clone();
    own.proto = log.proto;
    own.agent = log.agent.clone();
    own.location = log.location.clone();
    own.function = Some(FunctionLog::of(pool.config()));
    own.cache = Some("background");
    own.started = Instant::now();
    own.written = false;
    own
}

/// A kept page or answer, served.
fn serve(cache: &ResponseCache, entry: &Entry, label: Status, method: &Method, request: &HeaderMap, route: &Effects, log: &mut AccessLog) -> Response<ResponseBody> {
    let response = respond(cache, entry.status, entry.headers.clone(), entry.body.clone(), entry.age(cache::now_ms()), label, method, Some(request), route);
    log.function = None;
    log.cache = Some(label.as_str());
    log.bytes_out = if *method == Method::HEAD || response.status() == StatusCode::NOT_MODIFIED { 0 } else { entry.body.len() as u64 };
    log_now(log, response, "cache")
}

/// The client's answer from a kept or just-made one: the routes' headers on it, the client's
/// caching headers, `age` and `x-vercel-cache`; a 304 for a client that has it already.
#[allow(clippy::too_many_arguments)]
fn respond(cache: &ResponseCache, mut status: StatusCode, mut headers: HeaderMap, body: Bytes, age: u64, label: Status, method: &Method, request: Option<&HeaderMap>, route: &Effects) -> Response<ResponseBody> {
    route.apply(&mut status, &mut headers);
    cache::client_headers(&mut headers, cache.defaults);
    cache::mark(&mut headers, label, age);
    let not_modified = status == StatusCode::OK && request.is_some_and(|r| matches_etag(r, headers.get(ETAG)));
    let body = if not_modified {
        status = StatusCode::NOT_MODIFIED;
        headers.remove(CONTENT_LENGTH);
        Bytes::new()
    } else {
        // A HEAD's own answer was kept without a body: its length is the one it said.
        if !(body.is_empty() && *method == Method::HEAD && headers.contains_key(CONTENT_LENGTH)) {
            headers.insert(CONTENT_LENGTH, HeaderValue::from(body.len()));
        }
        if *method == Method::HEAD { Bytes::new() } else { body }
    };
    let mut response = Response::new(Full::new(body).map_err(|never| match never {}).boxed());
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Whether the client's `If-None-Match` names the answer's ETag (weakly, as for a GET).
fn matches_etag(request: &HeaderMap, etag: Option<&HeaderValue>) -> bool {
    let Some(etag) = etag.and_then(|e| e.to_str().ok()) else { return false };
    let etag = etag.strip_prefix("W/").unwrap_or(etag);
    request.get_all(IF_NONE_MATCH).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).any(|tag| {
        let tag = tag.trim();
        tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag
    })
}

/// A prerender's fallback page from the deployment's static files, with its status and headers,
/// told `PRERENDER`; `STALE` when every copy of the pages was lost since (`ResponseCache::lost`),
/// as the page it stands for may have been newer.
#[allow(clippy::too_many_arguments)]
async fn fallback_page(proxy: &Proxy, cache: &ResponseCache, prerender: &Prerender, static_files: Option<&str>, method: &Method, request: &HeaderMap, route: &Effects, log: &mut AccessLog, lost: bool) -> Option<Response<ResponseBody>> {
    let statics = proxy.statics.as_ref()?;
    let manifest = statics.manifest(static_files?).await?;
    let file = manifest.files.get(prerender.fallback.as_deref()?)?.clone();
    let status = prerender.initial_status.and_then(|s| StatusCode::from_u16(s).ok());
    let served = statics.respond(&file, method, request, status, crate::statics::Serve::Page, |len| proxy.room(len)).await?;
    let (mut parts, body) = served.response.into_parts();
    for (name, value) in &prerender.initial_headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            parts.headers.insert(name, value);
        }
    }
    parts.headers.remove("x-forkrail-cache");
    route.apply(&mut parts.status, &mut parts.headers);
    let label = if lost { Status::Stale } else { Status::Prerender };
    cache::client_headers(&mut parts.headers, cache.defaults);
    cache::mark(&mut parts.headers, label, 0);
    log.function = None;
    log.cache = Some(label.as_str());
    log.encoding = served.encoding.map(str::to_owned);
    log.bytes_out = served.bytes;
    Some(log_now(log, Response::from_parts(parts, body), "cache"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the cache adds to a request, on this machine:
    /// `cargo test --release -p forkrail-router cost_per_request -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn cost_per_request() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let cache = ResponseCache::new(64 << 20, 32 << 20, None, None, true);
            let mut request = HeaderMap::new();
            for (k, v) in [("accept", "text/html,application/xhtml+xml"), ("accept-encoding", "gzip, deflate, br"), ("user-agent", "Mozilla/5.0 (Macintosh) Chrome/131"), ("cookie", "a=1; b=2"), ("x-forwarded-for", "1.2.3.4")] {
                request.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
            }
            let mut answer = HeaderMap::new();
            for (k, v) in [("content-type", "text/html"), ("cache-control", "public, max-age=0, s-maxage=60"), ("etag", "\"abc\""), ("x-hook", "sveltekit"), ("date", "Mon, 16 Nov 2026 06:12:33 GMT")] {
                answer.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
            }
            let body = Bytes::from(vec![b'x'; 20_000]);
            let route = Effects::default();
            let n = 200_000u32;
            // A miss: the key, a lookup that finds nothing, then the answer planned, its headers
            // for the client, and kept.
            let started = std::time::Instant::now();
            for i in 0..n {
                let base = cache::cdn_key("dep-1", &Method::GET, "app.forkrail.com", &format!("/page?i={i}"), &request);
                let names = cache.vary_of(&base);
                let hash = cache::hash(&cache::with_vary(&base, names.as_deref().unwrap_or(&[]), &request));
                assert!(cache.get("dep-1", &hash, false).await.is_none());
                let keep = Keep { deployment: "dep-1".into(), what: What::Cdn { base, request: request.clone() }, guard: None, from: None, local: false };
                let own = own_headers(&answer);
                let view = with_route_headers(&own, &route);
                let (hash, mut entry, _) = plan(&cache, &keep, StatusCode::OK, own, &view, cache::now_ms()).unwrap();
                let mut client = answer.clone();
                cache::client_headers(&mut client, true);
                cache::mark(&mut client, Status::Miss, 0);
                entry.body = body.clone();
                cache.put(&hash, entry, None);
            }
            let miss = started.elapsed() / n;
            let last = format!("/page?i={}", n - 1);
            // A hit: the key, the lookup, and the answer made from what's kept.
            let base = cache::cdn_key("dep-1", &Method::GET, "app.forkrail.com", &last, &request);
            let started = std::time::Instant::now();
            let mut bytes = 0usize;
            for _ in 0..n {
                let base = cache::cdn_key("dep-1", &Method::GET, "app.forkrail.com", &last, &request);
                let names = cache.vary_of(&base);
                let hash = cache::hash(&cache::with_vary(&base, names.as_deref().unwrap_or(&[]), &request));
                let entry = cache.get("dep-1", &hash, false).await.unwrap();
                let response = respond(&cache, entry.status, entry.headers.clone(), entry.body.clone(), entry.age(cache::now_ms()), Status::Hit, &Method::GET, Some(&request), &route);
                bytes += response.headers().len();
            }
            let hit = started.elapsed() / n;
            // An answer that isn't kept: only the client's headers.
            let started = std::time::Instant::now();
            for _ in 0..n {
                let mut client = answer.clone();
                cache::client_headers(&mut client, true);
                cache::mark(&mut client, Status::Miss, 0);
                bytes += client.len();
            }
            let plain = started.elapsed() / n;
            let (entries, used) = cache.memory_use();
            // A page served from memory: the lookup (which also marks it used on disk) and the
            // answer made from it.
            let dir = std::env::temp_dir().join(format!("forkrail-bench-{}", forkrail_common::random_hex(6)));
            let pages = ResponseCache::new(1 << 20, 32 << 20, Some(cache::Disk::open(dir.clone(), 1 << 30, 0).unwrap()), None, true);
            let entry = Entry { status: StatusCode::OK, headers: answer.clone(), body: body.clone(), stored_ms: cache::now_ms(), fresh_until: None, stale_until: None, deployment: "dep-1".into(), group: None, page: true, local: false };
            let page_hash = cache::hash(&cache::page_key("dep-1", "app.forkrail.com", "/blog/[slug]", "slug=a"));
            pages.put(&page_hash, entry, None);
            let started = std::time::Instant::now();
            for _ in 0..n {
                let hash = cache::hash(&cache::page_key("dep-1", "app.forkrail.com", "/blog/[slug]", "slug=a"));
                let entry = pages.get("dep-1", &hash, true).await.unwrap();
                let label = label_of(&pages, &entry, Scope { project: "shop", environment: "production" }).unwrap();
                let response = respond(&pages, entry.status, entry.headers.clone(), entry.body.clone(), entry.age(cache::now_ms()), label, &Method::GET, Some(&request), &route);
                bytes += response.headers().len();
            }
            let page_hit = started.elapsed() / n;
            let _ = std::fs::remove_dir_all(dir);
            println!("miss (lookup + keep a 20 KB answer): {miss:?}; hit: {hit:?}; page hit: {page_hit:?}; headers only: {plain:?}; memory {entries} entries, {} bytes each as counted ({bytes})", used / entries.max(1));
            let _ = base;

            // Cache tags (F4c): a CDN hit as `cdn()` serves it, freshness (with the purges' marks)
            // included, for an answer with three tags: no purge anywhere, purges of other projects
            // only, and purges of this project (a mark on one of its tags, and 2,000 others).
            let tagged = ResponseCache::new(64 << 20, 1 << 20, None, None, true);
            let mut with_tags = answer.clone();
            with_tags.insert(cache::CACHE_TAG, HeaderValue::from_static("product-123,products,astro-path:/p"));
            let base = cache::cdn_key("dep-1", &Method::GET, "app.forkrail.com", "/p", &request);
            let keep = Keep { deployment: "dep-1".into(), what: What::Cdn { base: base.clone(), request: request.clone() }, guard: None, from: None, local: false };
            let (hash, mut entry, _) = plan(&tagged, &keep, StatusCode::OK, with_tags.clone(), &with_tags, cache::now_ms()).unwrap();
            entry.body = body.clone();
            tagged.put(&hash, entry, None);
            let scope = Scope { project: "shop", environment: "production" };
            let hit_with_marks = |label: &str| {
                let started = std::time::Instant::now();
                let mut fresh = 0u32;
                for _ in 0..n {
                    let names = tagged.vary_of(&base);
                    let hash = cache::hash(&cache::with_vary(&base, names.as_deref().unwrap_or(&[]), &request));
                    let entry = futures_lite_get(&tagged, &hash);
                    if tagged.freshness(&entry, cache::now_ms(), scope) == Freshness::Fresh {
                        fresh += 1;
                    }
                    let response = respond(&tagged, entry.status, entry.headers.clone(), entry.body.clone(), entry.age(cache::now_ms()), Status::Hit, &Method::GET, Some(&request), &route);
                    std::hint::black_box(response);
                }
                println!("CDN hit with 3 tags, {label}: {:?} (fresh {fresh} of {n})", started.elapsed() / n);
            };
            hit_with_marks("no purge anywhere");
            let purge = |project: &str, tags: Vec<String>| {
                let p = forkrail_common::purge::Purge { op: forkrail_common::purge::PurgeOp::Invalidate, tags, src_images: false, target: None, deadline_s: None, at_ms: 1, by: None };
                tagged.take_marks(&cache::TagMarks::marks_of(project, &p), None);
            };
            for i in 0..100 {
                purge(&format!("other-{i}"), (0..10).map(|t| format!("t{t}")).collect());
            }
            hit_with_marks("purges in 100 other projects");
            for chunk in (0..2_000).collect::<Vec<_>>().chunks(16) {
                purge("shop", chunk.iter().map(|t| format!("tag-{t}")).collect());
            }
            purge("shop", vec!["products".into()]);
            hit_with_marks("2,001 marks in its own project, one on its tags");
        });
    }

    /// A CDN answer from memory, without awaiting (`get` finds CDN answers in memory at once).
    fn futures_lite_get(cache: &ResponseCache, hash: &str) -> Arc<Entry> {
        let mut future = std::pin::pin!(cache.get("dep-1", hash, false));
        let waker = std::task::Waker::noop();
        match future.as_mut().poll(&mut std::task::Context::from_waker(waker)) {
            Poll::Ready(found) => found.expect("kept"),
            Poll::Pending => panic!("a CDN answer is in memory"),
        }
    }

    #[test]
    fn etags_match_weakly() {
        let mut request = HeaderMap::new();
        request.insert(IF_NONE_MATCH, HeaderValue::from_static("\"x\", W/\"abc\""));
        assert!(matches_etag(&request, Some(&HeaderValue::from_static("\"abc\""))));
        assert!(matches_etag(&request, Some(&HeaderValue::from_static("W/\"x\""))));
        assert!(!matches_etag(&request, Some(&HeaderValue::from_static("\"y\""))));
        assert!(!matches_etag(&request, None));
        assert!(!matches_etag(&HeaderMap::new(), Some(&HeaderValue::from_static("\"abc\""))));
    }

    #[test]
    fn what_is_kept_of_an_answer() {
        let cache = ResponseCache::new(1 << 20, 1 << 20, None, None, true);
        let page = Keep { deployment: "d".into(), what: What::Page { hash: "h".into(), group: Some(1), expiration: Some(30) }, guard: None, from: None, local: false };
        let mut own = HeaderMap::new();
        own.insert(CONTENT_TYPE, HeaderValue::from_static("text/html"));
        // A page whatever its caching headers say, for its expiration.
        let (hash, entry, limit) = plan(&cache, &page, StatusCode::OK, own.clone(), &own, 1_000).unwrap();
        assert_eq!((hash.as_str(), entry.fresh_until, entry.stale_until, entry.group, limit), ("h", Some(31_000), None, Some(1), cache::MAX_PAGE_BYTES));
        assert!(plan(&cache, &page, StatusCode::NOT_FOUND, own.clone(), &own, 0).is_some(), "a 404 page is kept");
        assert!(plan(&cache, &page, StatusCode::INTERNAL_SERVER_ERROR, own.clone(), &own, 0).is_none());
        let mut cookie = own.clone();
        cookie.insert(SET_COOKIE, HeaderValue::from_static("s=1"));
        assert!(plan(&cache, &page, StatusCode::OK, cookie.clone(), &cookie, 0).is_none(), "one visitor's cookie for everyone");
        // A route's cookie (skew protection's `__vdpl` on a document request) goes on each answer
        // as it's served: the page is kept without it.
        let skew = Effects { headers: vec![(SET_COOKIE, HeaderValue::from_static("__vdpl=dpl_1; Path=/"))], ..Effects::default() };
        let (_, kept, _) = plan(&cache, &page, StatusCode::OK, own.clone(), &with_route_headers(&own, &skew), 0).unwrap();
        assert!(!kept.headers.contains_key(SET_COOKIE));
        // A CDN answer only when it asks, under its Vary.
        let cdn = Keep { deployment: "d".into(), what: What::Cdn { base: "b".into(), request: HeaderMap::new() }, guard: None, from: None, local: false };
        assert!(plan(&cache, &cdn, StatusCode::OK, own.clone(), &own, 0).is_none());
        let mut asks = own.clone();
        asks.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=0, s-maxage=60, stale-while-revalidate=30"));
        asks.insert(hyper::header::VARY, HeaderValue::from_static("Origin"));
        let (_, entry, _) = plan(&cache, &cdn, StatusCode::OK, asks.clone(), &asks, 0).unwrap();
        assert_eq!((entry.fresh_until, entry.stale_until), (Some(60_000), Some(90_000)));
        assert_eq!(cache.vary_of("b").as_deref(), Some(&[HeaderName::from_static("origin")][..]));
        assert!(plan(&cache, &cdn, StatusCode::CREATED, asks.clone(), &asks, 0).is_none());
        // A route's header asks for it too, and the function's own wins.
        let route = Effects { headers: vec![(hyper::header::CACHE_CONTROL, HeaderValue::from_static("s-maxage=10"))], ..Effects::default() };
        assert!(plan(&cache, &cdn, StatusCode::OK, own.clone(), &with_route_headers(&own, &route), 0).is_some());
        let mut no = own.clone();
        no.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        assert!(plan(&cache, &cdn, StatusCode::OK, no.clone(), &with_route_headers(&no, &route), 0).is_none());
    }

    #[test]
    fn an_answer_next_says_is_stale_is_never_kept() {
        let cache = ResponseCache::new(1 << 20, 1 << 20, None, None, true);
        let cdn = Keep { deployment: "d".into(), what: What::Cdn { base: "b".into(), request: HeaderMap::new() }, guard: None, from: None, local: false };
        let page = Keep { deployment: "d".into(), what: What::Page { hash: "h".into(), group: None, expiration: Some(30) }, guard: None, from: None, local: false };
        // Next 14.2's answers for an ISR page with `revalidate: 5`, as its standalone server sends
        // them: the page's whole lifetime for the CDN, whether the page is fresh or past it.
        let answer = |label: &'static str| {
            let mut h = HeaderMap::new();
            h.insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
            h.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("s-maxage=5, stale-while-revalidate"));
            h.insert(HeaderName::from_static(forkrail_common::next::CACHE_HEADER), HeaderValue::from_static(label));
            h
        };
        let stale = answer("STALE");
        assert!(stale_by_next(&stale));
        assert!(plan(&cache, &cdn, StatusCode::OK, stale.clone(), &stale, 0).is_none(), "the old page would be served as fresh for 5 s after the new one was made");
        assert!(plan(&cache, &page, StatusCode::OK, stale.clone(), &stale, 0).is_none());
        // A deployment without N3's entry keeps Next's other answers as any function's that asks
        // (N3's own rule keeps none of them for a Next deployment).
        for label in ["HIT", "MISS", "REVALIDATED"] {
            let fresh = answer(label);
            assert!(!stale_by_next(&fresh));
            let (_, entry, _) = plan(&cache, &cdn, StatusCode::OK, fresh.clone(), &fresh, 0).unwrap();
            assert_eq!(entry.fresh_until, Some(5_000), "{label}");
        }
        // Another app's answer is unchanged.
        let mut other = answer("STALE");
        other.remove(forkrail_common::next::CACHE_HEADER);
        assert!(!stale_by_next(&other) && plan(&cache, &cdn, StatusCode::OK, other.clone(), &other, 0).is_some());
    }
}
