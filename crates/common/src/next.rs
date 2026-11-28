//! A Next.js deployment as the router serves it (N3): what the router must know of the build to
//! put the right thing in front of Next, and the rules it serves by, as pure functions.
//!
//! The routing table says a deployment is Next's (`routes::NextRef`: its shape and the id of the
//! object below); the facts themselves are one object in the static store,
//! `static/next/<id>.json` (`NextConfig`), named by content as rules and image settings are, so a
//! site with thousands of prerenders doesn't grow `routes.json`. The CLI writes it at build from
//! N1's manifest (`crates/cli/src/next_router.rs`); the router reads it once per deployment and
//! compiles it (`Site`).
//!
//! What the router does with it (`crates/router/src/next.rs` has the request side):
//!
//! - **Middleware first.** A request the deployment's middleware (or 16's `proxy`) may match is
//!   never answered from the router's cache or store: under Next's own server the middleware runs
//!   inside the worker, so such a request goes there, and Next reads the shared cache after its
//!   middleware has run. "May match": the matcher's pattern matches and its `has`/`missing` hold;
//!   a pattern or condition the router can't read counts as matching.
//! - **Draft Mode and on-demand revalidation** (`__prerender_bypass` equal to the preview id, or
//!   `x-prerender-revalidate` equal to it) go to Next every time and are never kept.
//! - **Pages from N2's store.** Anything else that maps to an entry Next keeps (an App Router page,
//!   a route handler's answer, a Pages Router page or its `/_next/data` JSON) is answered by the
//!   router from that entry: the part the request asks for (`html`, `rsc`, a segment, `data`,
//!   `body`), with Vercel's headers. Next's own key for it is computed here (`Lookup::keys`).
//! - **Only where nothing else of Next's would answer first.** A path a config redirect or a
//!   `beforeFiles` rewrite may take (or an `afterFiles` one, for a dynamic route) is Next's; the
//!   config's `headers` are applied to what the router serves, and a rule whose value needs a
//!   parameter sends the request to Next instead. Sites with `i18n` are left to Next entirely.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::rules::engine::{Cond, Re};
use crate::rules::{Condition, Request};

pub const FORMAT: u32 = 1;

/// Where a deployment's config object is, among its static files.
pub fn config_key(id: &str) -> String {
    format!("static/next/{id}.json")
}

/// Draft Mode's cookie: its value is the build's preview id.
pub const BYPASS_COOKIE: &str = "__prerender_bypass";
/// On-demand revalidation: Next renders the page now and keeps it, when the value is the preview id.
pub const REVALIDATE_HEADER: &str = "x-prerender-revalidate";
/// Next's answer says how its cache answered (`HIT`, `STALE`, `MISS`, `REVALIDATED`). Vercel
/// never sends it: the router turns it into `x-vercel-cache`.
pub const CACHE_HEADER: &str = "x-nextjs-cache";
/// What Vercel sends on an App Router page, for its client and any cache in between.
pub const APP_VARY: &str = "rsc, next-router-state-tree, next-router-prefetch, next-router-segment-prefetch";
/// Next's `didPostponeHeader` (`routes-manifest.json`'s `rsc`): `2` on a segment prefetch's
/// answer says it came from the per-segment prerender; `1` on a page's, that it has a PPR shell.
pub const DID_POSTPONE_HEADER: &str = "x-nextjs-postponed";
/// On a Next.js worker's answer: "the answer is this static file of the deployment's", its path
/// as the router's static files name it (with the base path), percent-encoded. Next's routing
/// (a config rewrite: `/docs/:path*` to `/:path*`) can land on a file the router serves and the
/// package doesn't hold (`public/`, `/_next/static/`); the worker says so, with no body, and the
/// router answers with the file as it serves it to a request for its own path. Set by the H
/// server (`runtime/src/next/server.mjs`) and by forkrail's part of Next's own server
/// (`runtime/src/next-standalone.cjs`); honoured only on a worker's answer to a request for a Next
/// deployment, for that deployment's own files, and never passed on to anyone.
pub const STATIC_HEADER: &str = "x-forkrail-static";

/// The facts of one Next.js build the router serves by: `static/next/<id>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextConfig {
    pub format: u32,
    /// The Next version that built it.
    pub version: String,
    /// Next's `BUILD_ID`: `/_next/data/<build id>/…` is this build's data.
    pub build_id: String,
    /// `deploymentId` as the build had it, which Next's client sends back (`?dpl=`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    /// `previewModeId`: Draft Mode's cookie value and on-demand revalidation's header value. A
    /// secret of the build (also in the package's `prerender-manifest.json`).
    pub preview_id: String,
    /// `basePath` (`/docs`), or empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trailing_slash: bool,
    /// The build has `i18n`: locales change keys and paths in ways the router doesn't follow, so
    /// it serves nothing of the site from the store.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub i18n: bool,
    /// Cache Components / Partial Prerendering is on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_components: bool,
    /// Its middleware (`middleware.ts`) or proxy (`proxy.ts`), if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub middleware: Option<Middleware>,
    /// Every route that runs code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<Route>,
    /// Every path the build prerendered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prerenders: Vec<Prerender>,
    /// Patterns (Next's own, JavaScript syntax) of config redirects and `beforeFiles` rewrites:
    /// a path one may match is Next's to answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    /// Patterns of `afterFiles` rewrites, which come before dynamic routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// The config's `headers()`, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderRule>,
    /// An RSC request's answer depends on its `Next-Url` (intercepting routes): the router serves
    /// no RSC from the store.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub next_url_rsc: bool,
    /// `/_next/image` is the router's (the deployment's `images` holds the settings).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub images: bool,
    /// Patterns of `fallback` rewrites, which come after every route: a path no route takes may
    /// still be one of theirs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<String>,
    /// The path of `assetPrefix` (`/cdn` of `https://cdn.example.com/cdn`), when it has one other
    /// than the base path: Next's server answers `<it>/_next/…` as `<base path>/_next/…`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub asset_prefix: String,
    /// Public files only Next's server has (the middleware may match them): never a 404 of the
    /// router's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub server_files: Vec<String>,
    /// The build's not-found page, when it never goes stale: what a path no route takes answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_found: Option<NotFound>,
    /// Every route Next knows (`routes-manifest.json`'s `staticRoutes` pages and `dynamicRoutes`
    /// patterns), functions or not (a route handler built to a file, such as `robots.txt`): a
    /// path one of them takes is never the router's 404.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_dynamic: Vec<String>,
}

/// The build's not-found page: App Router's `/_not-found` or the Pages Router's `/404`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotFound {
    /// Its path as Next keys it (`/_not-found`, `/404`).
    pub path: String,
    /// Its page in the project (`/_not-found/page`, `/404`).
    pub page: String,
    pub kind: RouteKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Middleware {
    /// `edge` or `nodejs`.
    pub runtime: String,
    pub matchers: Vec<Matcher>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Matcher {
    /// Next's pattern for the request's path (with the base path, locale and `/_next/data`
    /// prefix inside it).
    pub regexp: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<Condition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    AppPage,
    AppRoute,
    /// A Pages Router page.
    Page,
    /// A Pages Router API route.
    Api,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// The path it answers, without the base path (`/blog/[slug]`).
    pub path: String,
    /// Its page in the project (`/(site)/blog/[slug]/page`; a Pages Router page's is its path):
    /// what Next names its cache entries after (15.5.27+, 16.3.8+).
    pub page: String,
    pub kind: RouteKind,
    /// Next's pattern for a dynamic route's paths; none for a static one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    /// Its own `maxDuration`, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prerender {
    /// The path (`/blog/first`), without the base path.
    pub path: String,
    /// The route it belongs to (`/blog/[slug]`).
    pub route: String,
    /// Seconds until stale; none: until revalidated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revalidate: Option<u64>,
    /// Seconds until it may no longer be served at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire: Option<u64>,
    /// The query parameters that make its pages apart, when Next says (`allowQuery`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_query: Option<Vec<String>>,
    /// A Partial Prerendering shell: only its static part is kept; the rest renders per request.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shell: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeaderRule {
    /// As written (`/blog/:slug`): its parameters' names.
    pub source: String,
    /// Next's pattern for it.
    pub regex: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<Condition>,
    pub headers: Vec<(String, String)>,
}

impl NextConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.format != FORMAT {
            return Err(format!("format {}, not {FORMAT}", self.format));
        }
        if self.preview_id.is_empty() {
            return Err("no preview id".into());
        }
        if self.build_id.is_empty() || self.build_id.contains('/') {
            return Err(format!("build id {:?}", self.build_id));
        }
        if !self.base_path.is_empty() && (!self.base_path.starts_with('/') || self.base_path.ends_with('/')) {
            return Err(format!("base path {:?}", self.base_path));
        }
        Ok(())
    }

    /// The object's id: the SHA-256 of its JSON.
    pub fn id(json: &[u8]) -> String {
        crate::hex(&Sha256::digest(json))
    }
}

// ---- Compiled ----

struct CompiledMatcher {
    /// `None`: a pattern the router can't read, which matches everything.
    re: Option<Re>,
    has: Vec<Option<Cond>>,
    missing: Vec<Option<Cond>>,
}

struct CompiledHeaders {
    re: Option<Re>,
    has: Vec<Option<Cond>>,
    missing: Vec<Option<Cond>>,
    headers: Vec<(String, String)>,
    /// A header's name or value names one of the source's parameters: Next fills it in.
    needs_params: bool,
}

/// A deployment's config, ready to answer requests by.
pub struct Site {
    pub config: NextConfig,
    matchers: Vec<CompiledMatcher>,
    static_routes: HashMap<String, usize>,
    dynamic: Vec<(Option<Re>, usize)>,
    /// Path → (its route, its prerender), by position.
    prerenders: HashMap<String, (usize, usize)>,
    before: Vec<Option<Re>>,
    after: Vec<Option<Re>>,
    headers: Vec<CompiledHeaders>,
    static_prefix: String,
    image_path: String,
    fallback: Vec<Option<Re>>,
    server_files: std::collections::HashSet<String>,
    known: std::collections::HashSet<String>,
    known_dynamic: Vec<Option<Re>>,
}

/// Which part of a Next entry a request wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Html,
    Rsc,
    /// A segment prefetch (`Next-Router-Segment-Prefetch: /_tree`).
    Segment(String),
    /// A Pages Router page's props (`/_next/data/…json`).
    Data,
    /// A route handler's answer.
    Body,
}

impl Lookup {
    /// It's the build's not-found page (`Site::not_found`): answered with 404.
    pub fn status_page(&self) -> bool {
        self.page == "/404" && self.fully_static && !self.app
    }
}

impl Part {
    /// Its name in the entry (`nextcache::Meta::parts`).
    pub fn name(&self) -> String {
        match self {
            Part::Html => "html".into(),
            Part::Rsc => "rsc".into(),
            Part::Segment(path) => format!("seg:{path}"),
            Part::Data => "data".into(),
            Part::Body => "body".into(),
        }
    }

    /// The type it's served with; a route handler's comes with its entry.
    pub fn content_type(&self) -> Option<&'static str> {
        match self {
            Part::Html => Some("text/html; charset=utf-8"),
            Part::Rsc | Part::Segment(_) => Some("text/x-component"),
            Part::Data => Some("application/json; charset=utf-8"),
            Part::Body => None,
        }
    }
}

/// What the store may answer a request with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lookup {
    /// Next's path for it (`/blog/a`), without the base path.
    pub page: String,
    pub part: Part,
    /// The kind of entry it must be (`nextcache::Meta::kind`).
    pub kind: &'static str,
    /// Next's own keys for it, in the order to try: scoped (15.5.27+, 16.3.8+, 16.4) and plain.
    /// Next names an entry one way or the other by version; trying both needs no version table.
    pub keys: [String; 2],
    /// An App Router page or route handler (its answers carry App Router headers).
    pub app: bool,
    /// A page the build made that never goes stale: Vercel serves it as a static file.
    pub fully_static: bool,
}

fn compile_re(pattern: &str) -> Option<Re> {
    Re::new(pattern, true).ok()
}

fn compile_conds(conds: &[Condition]) -> Vec<Option<Cond>> {
    conds.iter().map(|c| Cond::new(c).ok()).collect()
}

/// `has` all hold and `missing` none (a condition that couldn't be read holds, and is missing).
fn conditions(has: &[Option<Cond>], missing: &[Option<Cond>], req: &Request<'_>) -> bool {
    has.iter().all(|c| c.as_ref().is_none_or(|c| c.holds_for(req))) && missing.iter().all(|c| c.as_ref().is_none_or(|c| !c.holds_for(req)))
}

/// A pattern matches (one that can't be read always does).
fn matches(re: &Option<Re>, text: &str) -> bool {
    re.as_ref().is_none_or(|re| re.is_match(text))
}

/// The names of `:name` parameters in a source (`/blog/:slug*`).
fn params_of(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end > start {
                out.push(source[start..end].to_owned());
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// Next's `normalizePagePath`: `/` is `/index`, and a path under `/index` gets another in front.
pub fn normalize_page_path(page: &str) -> String {
    if page == "/" {
        return "/index".into();
    }
    if page == "/index" || page.starts_with("/index/") {
        return format!("/index{page}");
    }
    page.to_owned()
}

/// Next's scoped key (`server/lib/route-cache-key.js`): `/route-cache/<kind>/<sha256(source)>/$<path>`.
pub fn scoped_key(kind: &str, source: &str, page: &str) -> String {
    format!("/route-cache/{kind}/{}/${}", crate::hex(&Sha256::digest(source.as_bytes())), normalize_page_path(page))
}

/// The hex SHA-256 of Next's key: what the store names an entry by (`nextcache::hash_input`).
pub fn key_hash(key: &str) -> String {
    crate::hex(&Sha256::digest(key.as_bytes()))
}

fn header<'a>(headers: &'a http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// `a` and `b` are the same, compared in constant time for their length.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Site {
    pub fn compile(config: NextConfig) -> Result<Site, String> {
        config.validate()?;
        let matchers = config
            .middleware
            .iter()
            .flat_map(|m| &m.matchers)
            .map(|m| CompiledMatcher { re: compile_re(&m.regexp), has: compile_conds(&m.has), missing: compile_conds(&m.missing) })
            .collect();
        let mut static_routes = HashMap::new();
        let mut dynamic = Vec::new();
        for (i, route) in config.routes.iter().enumerate() {
            match &route.regex {
                Some(regex) => dynamic.push((compile_re(regex), i)),
                None => {
                    static_routes.entry(route.path.clone()).or_insert(i);
                }
            }
        }
        let by_path: HashMap<&str, usize> = config.routes.iter().enumerate().map(|(i, r)| (r.path.as_str(), i)).collect();
        let mut prerenders = HashMap::new();
        for (n, p) in config.prerenders.iter().enumerate() {
            if let Some(&i) = by_path.get(p.route.as_str()) {
                prerenders.insert(p.path.clone(), (i, n));
            }
        }
        let headers = config
            .headers
            .iter()
            .map(|h| {
                let params = params_of(&h.source);
                let needs_params = h.headers.iter().any(|(k, v)| params.iter().any(|p| k.contains(&format!(":{p}")) || v.contains(&format!(":{p}"))));
                CompiledHeaders { re: compile_re(&h.regex), has: compile_conds(&h.has), missing: compile_conds(&h.missing), headers: h.headers.clone(), needs_params }
            })
            .collect();
        let base = config.base_path.clone();
        Ok(Site {
            matchers,
            static_routes,
            dynamic,
            prerenders,
            before: config.before.iter().map(|r| compile_re(r)).collect(),
            after: config.after.iter().map(|r| compile_re(r)).collect(),
            fallback: config.fallback.iter().map(|r| compile_re(r)).collect(),
            server_files: config.server_files.iter().cloned().collect(),
            known: config.known.iter().cloned().collect(),
            known_dynamic: config.known_dynamic.iter().map(|r| compile_re(r)).collect(),
            headers,
            static_prefix: format!("{base}/_next/static/"),
            image_path: format!("{base}/_next/image"),
            config,
        })
    }

    /// Whether the deployment's middleware runs for `req` (its path as the client sent it).
    pub fn middleware_runs(&self, req: &Request<'_>) -> bool {
        if self.config.middleware.is_none() {
            return false;
        }
        // A middleware with no matcher Next could use runs on everything.
        if self.matchers.is_empty() {
            return true;
        }
        self.matchers.iter().any(|m| matches(&m.re, req.path) && conditions(&m.has, &m.missing, req))
    }

    /// Whether `value` is the build's preview id.
    pub fn is_preview_id(&self, value: &str) -> bool {
        same(value.trim(), &self.config.preview_id)
    }

    /// The request is in Draft Mode (or Pages Router's preview mode): its bypass cookie is the
    /// build's preview id.
    pub fn draft_mode(&self, headers: &http::HeaderMap) -> bool {
        headers
            .get_all(http::header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .any(|(name, value)| name.trim() == BYPASS_COOKIE && self.is_preview_id(value))
    }

    /// `Some(true)`: the request asks Next to make its page again now, with the build's id;
    /// `Some(false)`: it has the header with another value; `None`: it hasn't.
    pub fn revalidation(&self, headers: &http::HeaderMap) -> Option<bool> {
        header(headers, REVALIDATE_HEADER).map(|v| self.is_preview_id(v))
    }

    /// Whether `path` is under `/_next/static/` (a hashed asset of some build).
    pub fn static_asset(&self, path: &str) -> bool {
        path.starts_with(&self.static_prefix)
    }

    /// `<base>/_next/image`.
    pub fn image_path(&self) -> &str {
        &self.image_path
    }

    /// A request for `<asset prefix's path>/_next/…`, as Next's server takes it: the same path
    /// under the base path.
    pub fn unprefixed(&self, path: &str) -> Option<String> {
        if self.config.asset_prefix.is_empty() {
            return None;
        }
        let rest = path.strip_prefix(self.config.asset_prefix.as_str())?;
        rest.starts_with("/_next/").then(|| format!("{}{rest}", self.config.base_path))
    }

    /// Whether `path` is a public file only Next's server has.
    pub fn server_file(&self, path: &str) -> bool {
        self.server_files.contains(path)
    }

    /// The build's not-found page for a request no route, file or rewrite of Next's takes (what
    /// Next would answer with its 404 page): `None` when one may, or when Next must say.
    pub fn not_found(&self, req: &Request<'_>) -> Option<Lookup> {
        let found = self.config.not_found.as_ref()?;
        if self.config.i18n || !matches!(req.method, "GET" | "HEAD") || req.headers.contains_key("rsc") {
            return None;
        }
        let rest = self.without_base(req.path)?;
        if rest.starts_with("/_next/") || rest.contains('%') || rest.contains("//") || self.server_file(req.path) {
            return None;
        }
        let page = if rest.len() > 1 { rest.trim_end_matches('/') } else { rest };
        if self.route(page).is_some()
            || self.known.contains(page)
            || self.known_dynamic.iter().any(|re| matches(re, page))
            || self.taken(req.path, true)
            || self.fallback.iter().any(|re| matches(re, req.path))
        {
            return None;
        }
        let (kind, source) = match found.kind {
            RouteKind::AppPage => ("APP_PAGE", found.page.as_str()),
            RouteKind::Page => ("PAGES", found.path.as_str()),
            _ => return None,
        };
        Some(Lookup {
            keys: [scoped_key(kind, source, &found.path), normalize_page_path(&found.path)],
            page: "/404".into(),
            part: Part::Html,
            kind,
            app: false,
            fully_static: true,
        })
    }

    /// The path without the base path, if it's under it.
    fn without_base<'a>(&self, path: &'a str) -> Option<&'a str> {
        let rest = path.strip_prefix(self.config.base_path.as_str())?;
        match rest {
            "" => Some("/"),
            r if r.starts_with('/') => Some(r),
            _ => None,
        }
    }

    /// The build's prerender of a page path, if it made one.
    fn prerender_of(&self, page: &str) -> Option<&Prerender> {
        self.prerenders.get(page).map(|&(_, n)| &self.config.prerenders[n])
    }

    /// The route a page path (without the base path) belongs to, and whether it's dynamic.
    pub fn route(&self, page: &str) -> Option<(&Route, bool)> {
        if let Some(&(i, _)) = self.prerenders.get(page) {
            let route = &self.config.routes[i];
            return Some((route, route.regex.is_some()));
        }
        if let Some(&i) = self.static_routes.get(page) {
            return Some((&self.config.routes[i], false));
        }
        self.dynamic.iter().find(|(re, _)| re.as_ref().is_some_and(|re| re.is_match(page))).map(|(_, i)| (&self.config.routes[*i], true))
    }

    /// The route a request's path (with the base path, a `/_next/data` one too) belongs to.
    pub fn route_of_path(&self, path: &str) -> Option<&Route> {
        let rest = self.without_base(path)?;
        let page = match rest.strip_prefix("/_next/data/") {
            Some(data) => data_page(data.split_once('/')?.1)?,
            None => rest.to_owned(),
        };
        let page = if page.len() > 1 { page.trim_end_matches('/').to_owned() } else { page };
        Some(self.route(&page)?.0)
    }

    /// The route's own `maxDuration` for a request's path, if it has one.
    pub fn max_duration(&self, path: &str) -> Option<u64> {
        self.route_of_path(path)?.max_duration_s
    }

    /// Whether a rule of Next's config that comes before its pages (a redirect, a `beforeFiles`
    /// rewrite; an `afterFiles` one too before a dynamic route) may take the path.
    fn taken(&self, path: &str, dynamic: bool) -> bool {
        self.before.iter().any(|re| matches(re, path)) || (dynamic && self.after.iter().any(|re| matches(re, path)))
    }

    /// What the store may answer `req` with: `None` when Next must (another method, a path that
    /// isn't one of its pages' as Next spells it, one its config may redirect or rewrite, a site
    /// with `i18n`). Draft Mode, revalidation and middleware are the caller's to check first.
    pub fn lookup(&self, req: &Request<'_>) -> Option<Lookup> {
        if self.config.i18n || !matches!(req.method, "GET" | "HEAD") {
            return None;
        }
        let rest = self.without_base(req.path)?;
        // Paths Next would decode, or refuse, are its own to answer.
        if rest.contains('%') || rest.contains("//") || rest.contains("/.") {
            return None;
        }
        let (page, part) = match rest.strip_prefix("/_next/") {
            Some(next) => {
                let data = next.strip_prefix("data/")?;
                let (build, file) = data.split_once('/')?;
                if build != self.config.build_id {
                    return None;
                }
                (data_page(file)?, Part::Data)
            }
            None => {
                let page = if rest == "/" {
                    "/".to_owned()
                } else if self.config.trailing_slash {
                    rest.strip_suffix('/')?.to_owned()
                } else if rest.ends_with('/') {
                    return None;
                } else {
                    rest.to_owned()
                };
                let rsc = header(req.headers, "rsc") == Some("1");
                let part = if !rsc {
                    Part::Html
                } else if self.config.next_url_rsc {
                    return None;
                } else if let Some(segment) = header(req.headers, "next-router-segment-prefetch") {
                    if segment.is_empty() || segment.len() > 1024 {
                        return None;
                    }
                    Part::Segment(segment.to_owned())
                } else if self.config.cache_components && header(req.headers, "next-router-prefetch").is_some() {
                    return None;
                } else {
                    Part::Rsc
                };
                (page, part)
            }
        };
        let (route, dynamic) = self.route(&page)?;
        if self.taken(req.path, dynamic) {
            return None;
        }
        let (kind, source, app) = match (route.kind, &part) {
            (RouteKind::AppPage, Part::Html | Part::Rsc | Part::Segment(_)) => ("APP_PAGE", route.page.as_str(), true),
            (RouteKind::AppRoute, Part::Html) => ("APP_ROUTE", route.page.as_str(), true),
            (RouteKind::Page, Part::Html | Part::Data) => ("PAGES", route.path.as_str(), false),
            _ => return None,
        };
        let part = if kind == "APP_ROUTE" { Part::Body } else { part };
        let fully_static = self.prerender_of(&page).is_some_and(|p| p.revalidate.is_none() && !p.shell);
        Some(Lookup { keys: [scoped_key(kind, source, &page), normalize_page_path(&page)], page, part, kind, app, fully_static })
    }

    /// The config's `headers()` for a request the router answers, in order; `None` when one that
    /// applies needs Next (a value with one of its source's parameters).
    pub fn config_headers(&self, req: &Request<'_>) -> Option<Vec<(String, String)>> {
        let mut out = Vec::new();
        for rule in &self.headers {
            if !matches(&rule.re, req.path) || !conditions(&rule.has, &rule.missing, req) {
                continue;
            }
            if rule.needs_params || rule.re.is_none() {
                return None;
            }
            out.extend(rule.headers.iter().cloned());
        }
        Some(out)
    }
}

/// A `/_next/data/<build>/` file's page: `index.json` is `/`, `blog/a.json` is `/blog/a`.
fn data_page(file: &str) -> Option<String> {
    let name = file.strip_suffix(".json")?;
    if name.is_empty() || name.contains('%') || name.contains("//") {
        return None;
    }
    Some(if name == "index" { "/".into() } else { format!("/{name}") })
}

// ---- Headers between visitors and a Next deployment ----

/// Request headers the router takes off every visitor's request (`platform_only` in the
/// router) that a Next deployment's visitors may send all the same: Next's own client sends it
/// (`x-middleware-prefetch: 1` on a Pages Router prefetch, so the page's data isn't fetched for
/// nothing). Safe: it only makes Next skip a prefetch's data; middleware still runs.
pub const CLIENT_MAY_SEND: [&str; 1] = ["x-middleware-prefetch"];

/// A request header no visitor may send to a Next deployment: Next's internal invocation and
/// resume protocol, and its own markers (except the data-request marker its client sends). The
/// router takes `x-middleware-*` (but `CLIENT_MAY_SEND`), `x-matched-path`, `x-now-route-matches`
/// and `x-vercel-*` off every request already.
pub fn internal_request_header(name: &str) -> bool {
    matches!(name, "next-resume" | "x-next-resume-state-length" | "x-invocation-id" | "x-next-revalidate-tag-token" | "x-next-revalidated-tags")
        || name.starts_with("x-invoke-")
        || (name.starts_with("x-nextjs-") && name != "x-nextjs-data")
}

/// A response header of Next's that a visitor gets, where the router takes `x-middleware-*` off
/// other deployments' answers (C16's routing middleware protocol): Next's Pages Router client
/// reads `x-middleware-skip` and `-cache` (with `x-nextjs-rewrite`, `-redirect`, `-matched-path`,
/// which pass anyway), and an app may name its own header so (`x-middleware-ran`; Vercel passes
/// it). Never: the headers that carry the request on inside Next (`-override-headers`,
/// `-request-*`, which could hold a header the middleware added for the page, such as a token),
/// `-next`, `-set-cookie`, `-invoke`, `-subrequest`, and `-rewrite`/`-redirect`, which no client
/// of Next's reads (13.5 to 16), Vercel doesn't send, and which can carry an internal URL.
pub fn middleware_header_to_client(name: &str) -> bool {
    !(matches!(
        name,
        "x-middleware-next" | "x-middleware-override-headers" | "x-middleware-set-cookie" | "x-middleware-invoke" | "x-middleware-subrequest" | "x-middleware-rewrite" | "x-middleware-redirect"
    ) || name.starts_with("x-middleware-request-"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{ConditionKind, Matchable};

    fn cond(kind: ConditionKind, key: &str, value: Option<&str>) -> Condition {
        Condition { kind, key: Some(key.into()), value: value.map(|v| Matchable::Text(v.into())) }
    }

    /// next-app's build as N1's manifest has it (Next 16.4.6), in the router's terms.
    fn next_app() -> NextConfig {
        let route = |path: &str, page: &str, kind, regex: Option<&str>| Route { path: path.into(), page: page.into(), kind, regex: regex.map(Into::into), max_duration_s: None };
        NextConfig {
            format: FORMAT,
            version: "16.4.6".into(),
            build_id: "b1".into(),
            deployment_id: None,
            preview_id: "0123456789abcdef0123456789abcdef".into(),
            base_path: String::new(),
            trailing_slash: false,
            i18n: false,
            cache_components: false,
            middleware: Some(Middleware {
                runtime: "nodejs".into(),
                matchers: ["legacy", "rewritten", "ssr", "isr"]
                    .iter()
                    .map(|p| Matcher {
                        regexp: format!("^(?:\\/(_next\\/data\\/[^/]{{1,}}))?\\/{p}(\\.json)?[\\/#\\?]?$"),
                        has: vec![],
                        missing: vec![cond(ConditionKind::Header, "x-prerender-revalidate", Some("0123456789abcdef0123456789abcdef"))],
                    })
                    .collect(),
            }),
            routes: vec![
                route("/", "/page", RouteKind::AppPage, None),
                route("/isr", "/isr/page", RouteKind::AppPage, None),
                route("/ssr", "/ssr/page", RouteKind::AppPage, None),
                route("/blog/[slug]", "/blog/[slug]/page", RouteKind::AppPage, Some("^/blog/([^/]+?)(?:/)?$")),
                route("/api/hello", "/api/hello/route", RouteKind::AppRoute, None),
            ],
            prerenders: vec![
                Prerender { path: "/".into(), route: "/".into(), revalidate: None, expire: None, allow_query: None, shell: false },
                Prerender { path: "/isr".into(), route: "/isr".into(), revalidate: Some(30), expire: None, allow_query: None, shell: false },
                Prerender { path: "/blog/first".into(), route: "/blog/[slug]".into(), revalidate: Some(60), expire: None, allow_query: None, shell: false },
            ],
            before: vec!["^(?!/_next)/old-blog(?:/([^/]+?))(?:/)?$".into(), "^/hi(?:/)?$".into()],
            after: vec!["^/docs(?:/((?:[^/]+?)(?:/(?:[^/]+?))*))?(?:/)?$".into()],
            headers: vec![HeaderRule { source: "/ssr".into(), regex: "^/ssr(?:/)?$".into(), has: vec![], missing: vec![], headers: vec![("x-from-config".into(), "yes".into())] }],
            next_url_rsc: false,
            images: false,
            fallback: vec![],
            asset_prefix: String::new(),
            server_files: vec![],
            not_found: None,
            known: vec![],
            known_dynamic: vec![],
        }
    }

    fn req<'a>(method: &'a str, path: &'a str, headers: &'a http::HeaderMap) -> Request<'a> {
        Request { method, path, query: None, host: "app.test", headers }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn middleware_paths_are_nexts_alone() {
        let site = Site::compile(next_app()).unwrap();
        let none = http::HeaderMap::new();
        for path in ["/isr", "/isr/", "/ssr", "/_next/data/b1/isr.json", "/legacy"] {
            assert!(site.middleware_runs(&req("GET", path, &none)), "{path}");
        }
        for path in ["/", "/blog/first", "/api/hello", "/isrx"] {
            assert!(!site.middleware_runs(&req("GET", path, &none)), "{path}");
        }
        // 16.4's matchers skip on-demand revalidation with the build's id, not with another.
        let ours = headers(&[("x-prerender-revalidate", "0123456789abcdef0123456789abcdef")]);
        assert!(!site.middleware_runs(&req("GET", "/isr", &ours)));
        let theirs = headers(&[("x-prerender-revalidate", "guess")]);
        assert!(site.middleware_runs(&req("GET", "/isr", &theirs)));
        // A pattern the router can't read, or no matcher at all: every path.
        let mut odd = next_app();
        odd.middleware.as_mut().unwrap().matchers = vec![Matcher { regexp: "^(?<=x)[".into(), has: vec![], missing: vec![] }];
        assert!(Site::compile(odd.clone()).unwrap().middleware_runs(&req("GET", "/anything", &none)));
        odd.middleware.as_mut().unwrap().matchers.clear();
        assert!(Site::compile(odd).unwrap().middleware_runs(&req("GET", "/", &none)));
        let mut without = next_app();
        without.middleware = None;
        assert!(!Site::compile(without).unwrap().middleware_runs(&req("GET", "/isr", &none)));
    }

    #[test]
    fn has_and_missing_decide_a_matcher() {
        let mut config = next_app();
        config.middleware.as_mut().unwrap().matchers = vec![Matcher {
            regexp: "^/admin(?:/.*)?$".into(),
            has: vec![cond(ConditionKind::Cookie, "session", None)],
            missing: vec![cond(ConditionKind::Header, "x-skip", Some("1"))],
        }];
        let site = Site::compile(config).unwrap();
        let none = http::HeaderMap::new();
        assert!(!site.middleware_runs(&req("GET", "/admin", &none)), "no session cookie: has fails");
        let cookie = headers(&[("cookie", "session=abc")]);
        assert!(site.middleware_runs(&req("GET", "/admin/x", &cookie)));
        let skipped = headers(&[("cookie", "session=abc"), ("x-skip", "1")]);
        assert!(!site.middleware_runs(&req("GET", "/admin/x", &skipped)));
    }

    #[test]
    fn draft_mode_and_revalidation_need_the_builds_id() {
        let site = Site::compile(next_app()).unwrap();
        assert!(site.draft_mode(&headers(&[("cookie", "a=1; __prerender_bypass=0123456789abcdef0123456789abcdef; b=2")])));
        assert!(!site.draft_mode(&headers(&[("cookie", "__prerender_bypass=nope")])));
        assert!(!site.draft_mode(&http::HeaderMap::new()));
        assert_eq!(site.revalidation(&headers(&[("x-prerender-revalidate", "0123456789abcdef0123456789abcdef")])), Some(true));
        assert_eq!(site.revalidation(&headers(&[("x-prerender-revalidate", "x")])), Some(false));
        assert_eq!(site.revalidation(&http::HeaderMap::new()), None);
    }

    #[test]
    fn keys_are_nexts_own() {
        // The examples in N2's notes, from Next's `route-cache-key.js`.
        assert_eq!(scoped_key("APP_PAGE", "/blog/[slug]/page", "/blog/a"), "/route-cache/APP_PAGE/598124ea42a7fa20b1ca4e05fb72834de73e98d2ec5bfb5863d326020e1250db/$/blog/a");
        assert_eq!(scoped_key("APP_PAGE", "/page", "/"), "/route-cache/APP_PAGE/c6377dd9ce6a1c2cd100eb4344e843eb4639b9ed26f84643f77680e9eb454c4b/$/index");
        assert_eq!(scoped_key("PAGES", "/blog/[slug]", "/blog/post"), "/route-cache/PAGES/ceb8399bd1100580b7f43c3a39c05fd664d59a4db5ad3a1419f23db6790244b3/$/blog/post");
        assert_eq!(normalize_page_path("/index/a"), "/index/index/a");
        assert_eq!(normalize_page_path("/indexes"), "/indexes");
    }

    #[test]
    fn what_the_store_may_answer() {
        let site = Site::compile(next_app()).unwrap();
        let none = http::HeaderMap::new();
        let look = |path: &str, h: &http::HeaderMap| site.lookup(&req("GET", path, h));
        let home = look("/", &none).unwrap();
        assert_eq!((home.part.clone(), home.kind, home.keys[1].as_str()), (Part::Html, "APP_PAGE", "/index"));
        // A path of a dynamic route the build didn't make: the route's own key.
        let new = look("/blog/new-one", &none).unwrap();
        assert_eq!(new.keys[0], scoped_key("APP_PAGE", "/blog/[slug]/page", "/blog/new-one"));
        // RSC and segments by the request's headers, never by `_rsc` (Vercel doesn't check it).
        let rsc = headers(&[("rsc", "1"), ("next-router-state-tree", "%5B%5D")]);
        assert_eq!(look("/blog/first", &rsc).unwrap().part, Part::Rsc);
        let seg = headers(&[("rsc", "1"), ("next-router-prefetch", "1"), ("next-router-segment-prefetch", "/_tree")]);
        assert_eq!(look("/blog/first", &seg).unwrap().part, Part::Segment("/_tree".into()));
        // A route handler's answer.
        assert_eq!(look("/api/hello", &none).unwrap().part, Part::Body);
        assert!(look("/api/hello", &rsc).is_none());
        // Not as Next spells its pages: a trailing slash it would redirect, an encoded path.
        assert!(look("/blog/first/", &none).is_none());
        assert!(look("/blog/a%20b", &none).is_none());
        assert!(look("/nowhere", &none).is_none());
        // A config redirect or `beforeFiles` rewrite comes first; an `afterFiles` one only before
        // a dynamic route.
        assert!(look("/old-blog/x", &none).is_none());
        assert!(site.lookup(&req("POST", "/", &none)).is_none());
        assert!(look("/_next/data/b1/index.json", &none).is_none(), "an App Router page has no data");
        let mut i18n = next_app();
        i18n.i18n = true;
        assert!(Site::compile(i18n).unwrap().lookup(&req("GET", "/", &none)).is_none());
    }

    #[test]
    fn pages_router_data_and_trailing_slashes() {
        let mut config = next_app();
        config.routes.push(Route { path: "/posts/[id]".into(), page: "/posts/[id]".into(), kind: RouteKind::Page, regex: Some("^/posts/([^/]+?)(?:/)?$".into()), max_duration_s: Some(12) });
        config.trailing_slash = true;
        config.base_path = "/shop".into();
        let site = Site::compile(config).unwrap();
        let none = http::HeaderMap::new();
        let data = site.lookup(&req("GET", "/shop/_next/data/b1/posts/7.json", &none)).unwrap();
        assert_eq!((data.page.as_str(), data.part, data.kind), ("/posts/7", Part::Data, "PAGES"));
        assert_eq!(data.keys[0], scoped_key("PAGES", "/posts/[id]", "/posts/7"));
        assert!(site.lookup(&req("GET", "/shop/_next/data/old/posts/7.json", &none)).is_none(), "another build's data");
        assert_eq!(site.lookup(&req("GET", "/shop/posts/7/", &none)).unwrap().part, Part::Html);
        assert!(site.lookup(&req("GET", "/shop/posts/7", &none)).is_none(), "Next adds the slash");
        assert!(site.lookup(&req("GET", "/posts/7/", &none)).is_none(), "outside the base path");
        assert_eq!(site.max_duration("/shop/posts/7/"), Some(12));
        assert_eq!(site.max_duration("/shop/_next/data/b1/posts/7.json"), Some(12));
        assert_eq!(site.max_duration("/shop/api/hello"), None);
        assert!(site.static_asset("/shop/_next/static/chunks/a.js"));
        assert_eq!(site.image_path(), "/shop/_next/image");
    }

    #[test]
    fn unknown_paths_get_the_builds_not_found_page() {
        let mut config = next_app();
        config.not_found = Some(NotFound { path: "/_not-found".into(), page: "/_not-found/page".into(), kind: RouteKind::AppPage });
        config.fallback = vec!["^/legacy-site(?:/.*)?$".into()];
        config.server_files = vec!["/secret.txt".into()];
        config.asset_prefix = "/cdn".into();
        config.known = vec!["/robots.txt".into()];
        config.known_dynamic = vec!["^/shop/([^/]+?)(?:/)?$".into()];
        let site = Site::compile(config).unwrap();
        let none = http::HeaderMap::new();
        let nf = |path| site.not_found(&req("GET", path, &none));
        let found = nf("/nowhere").unwrap();
        assert_eq!((found.kind, found.keys[0].as_str(), found.status_page()), ("APP_PAGE", scoped_key("APP_PAGE", "/_not-found/page", "/_not-found").as_str(), true));
        for taken in ["/", "/blog/anything", "/api/hello", "/old-blog/x", "/docs/a", "/legacy-site/a", "/secret.txt", "/_next/static/x.js", "/a%20b", "/robots.txt", "/shop/x"] {
            assert!(nf(taken).is_none(), "{taken} is Next's");
        }
        assert!(site.not_found(&req("GET", "/nowhere", &headers(&[("rsc", "1")]))).is_none());
        assert!(site.not_found(&req("POST", "/nowhere", &none)).is_none());
        assert_eq!(site.unprefixed("/cdn/_next/static/chunks/a.js").as_deref(), Some("/_next/static/chunks/a.js"));
        assert_eq!(site.unprefixed("/cdn/other"), None);
        let mut without = next_app();
        without.not_found = None;
        assert!(Site::compile(without).unwrap().not_found(&req("GET", "/nowhere", &none)).is_none());
    }

    #[test]
    fn config_headers_go_on_what_the_router_answers() {
        let mut config = next_app();
        config.headers.push(HeaderRule { source: "/blog/:slug".into(), regex: "^/blog(?:/([^/]+?))(?:/)?$".into(), has: vec![], missing: vec![], headers: vec![("x-post".into(), "yes".into())] });
        let site = Site::compile(config.clone()).unwrap();
        let none = http::HeaderMap::new();
        assert_eq!(site.config_headers(&req("GET", "/blog/a", &none)), Some(vec![("x-post".into(), "yes".into())]));
        assert_eq!(site.config_headers(&req("GET", "/", &none)), Some(vec![]));
        config.headers.push(HeaderRule { source: "/blog/:slug".into(), regex: "^/blog(?:/([^/]+?))(?:/)?$".into(), has: vec![], missing: vec![], headers: vec![("x-slug".into(), ":slug".into())] });
        assert_eq!(Site::compile(config).unwrap().config_headers(&req("GET", "/blog/a", &none)), None, "a parameter is Next's to fill in");
    }

    #[test]
    fn headers_between_visitors_and_next() {
        for name in ["next-resume", "x-next-resume-state-length", "x-invoke-path", "x-invoke-query", "x-invocation-id", "x-nextjs-rewrite", "x-nextjs-internal"] {
            assert!(internal_request_header(name), "{name}");
        }
        for name in ["x-nextjs-data", "rsc", "next-router-state-tree", "next-url", "x-middleware-prefetch"] {
            assert!(!internal_request_header(name), "{name}");
        }
        for name in ["x-middleware-skip", "x-middleware-cache", "x-middleware-ran"] {
            assert!(middleware_header_to_client(name), "{name}");
        }
        for name in ["x-middleware-next", "x-middleware-override-headers", "x-middleware-request-authorization", "x-middleware-set-cookie", "x-middleware-subrequest", "x-middleware-rewrite", "x-middleware-redirect"] {
            assert!(!middleware_header_to_client(name), "{name}");
        }
    }
}
