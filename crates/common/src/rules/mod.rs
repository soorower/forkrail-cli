//! A deployment's routing rules: `vercel.json`'s `redirects`, `rewrites`, `headers`, `cleanUrls`,
//! `trailingSlash` and `routes`, turned into one ordered list of Vercel's routes (`Program`) the
//! way Vercel's routing-utils does (`convert`), and run against each request by the router
//! (`engine`). The CLI builds and checks the program before anything is uploaded, so a mistake is
//! refused at deploy, not found by a visitor; the router reads the same program from S3.
//!
//! Matching follows Vercel: `source` patterns are path-to-regexp 6.1.0 (`p2r`), `src` patterns
//! JavaScript regexes, run with the `regex` crate, or `fancy-regex` for the lookarounds and
//! backreferences it lacks (`/((?!uk/).*)`).

pub mod bulk;
pub mod convert;
pub mod engine;
pub mod middleware;
pub mod p2r;
pub mod prerender;
pub mod project;
pub mod transform;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use engine::{Decision, Engine, Files, NoFiles, Outcome, Paused, Request, Resume, Step, normalize_query};
pub use prerender::Prerender;

/// Vercel's limits, from its `vercel.json` schema: items in each of `routes`, `redirects`,
/// `rewrites` and `headers`; characters in a `source`, `src`, `destination` or `dest`.
pub const MAX_RULES: usize = 2048;
pub const MAX_PATTERN: usize = 4096;
/// Header values may be longer, and a `headers` item has at most this many.
const MAX_HEADER_VALUE: usize = 32768;
const MAX_HEADERS_PER_RULE: usize = 1024;
const MAX_CONDITIONS: usize = 16;

/// Where a route's handling stands, as Vercel names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Check the path against the deployment's files and function.
    Filesystem,
    /// Run when the filesystem had the path: headers only.
    Hit,
    /// Run when it didn't.
    Miss,
    /// Run when nothing served the request.
    Error,
}

/// One of Vercel's routes: a phase marker, or a pattern and what to do on a match.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Route {
    Handle { handle: Phase },
    Source(SourceRoute),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceRoute {
    /// An anchored JavaScript regex for the path.
    pub src: String,
    /// Where to send the request: a path in the deployment, or a URL elsewhere. `$1` and `$name`
    /// are the pattern's captures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    /// Response headers. `Location` with a 3xx `status` is a redirect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<String>>,
    /// Keep matching the routes after this one.
    #[serde(default, rename = "continue", skip_serializing_if = "std::ops::Not::not")]
    pub cont: bool,
    /// Go on past this route when `dest` isn't one of the deployment's files.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub check: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, rename = "caseSensitive", skip_serializing_if = "Option::is_none")]
    pub case_sensitive: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<Condition>,
    /// Changes to the request's headers, query or path, and to the answer's headers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transforms: Vec<Transform>,
    /// Which part of `vercel.json` it came from (`redirects[3]`, `cleanUrls`), for the logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Routing Middleware (Vercel's `middlewarePath`): the function a match runs first, by its path
    /// (`/_middleware`); its answer says whether routing goes on (`middleware.rs`). Routers from
    /// before it read the route as a plain `continue` and skip the middleware, so a deployment with
    /// one needs routers that have it.
    #[serde(default, rename = "middlewarePath", skip_serializing_if = "Option::is_none")]
    pub middleware: Option<String>,
    /// `override`: a builder's route that stays out of the leading group of its `continue` and
    /// `check` routes when merged with the project's (Vercel's `mergeRoutes`).
    #[serde(default, rename = "override", skip_serializing_if = "std::ops::Not::not")]
    pub override_: bool,
    /// Vercel's route `mitigate`: a match is challenged or denied as the firewall does (C5's
    /// challenge page and deny answer). A challenge passed goes on through the routes. Routers from
    /// before it ignore it: a deployment with one needs routers that have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitigate: Option<Mitigate>,
}

/// A route's `mitigate`: `{"action": "challenge" | "deny"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mitigate {
    pub action: MitigateAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MitigateAction {
    /// The firewall's challenge page, unless the visitor has passed it.
    Challenge,
    /// 403, as the firewall's deny.
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub key: String,
    pub value: String,
}

/// A `has` or `missing` item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: ConditionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Matchable>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConditionKind {
    Header,
    Cookie,
    Host,
    Query,
}

/// A condition's value: a regex the whole value must match, or comparisons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Matchable {
    Text(String),
    Ops(Ops),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ops {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neq: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inc: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ninc: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<f64>,
}

/// One of a route's `transforms`: a change to the request's headers, query or path, or to the
/// answer's headers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    #[serde(rename = "type")]
    pub kind: TransformKind,
    pub op: TransformOp,
    /// The header or parameter: a name, or comparisons its name must pass. None for
    /// `request.path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TransformTarget>,
    /// The values (`$1` and `$name` are the route's captures).
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "one_or_many")]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransformKind {
    #[serde(rename = "request.headers")]
    RequestHeaders,
    #[serde(rename = "request.query")]
    RequestQuery,
    #[serde(rename = "response.headers")]
    ResponseHeaders,
    #[serde(rename = "request.path")]
    RequestPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransformOp {
    Set,
    Append,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransformTarget {
    pub key: KeyMatch,
}

/// Which keys a transform touches: one by name, or every one whose name passes the comparisons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeyMatch {
    Name(String),
    Ops(Ops),
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

/// A deployment's routing: its routes, and what its "filesystem" holds besides static files.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub routes: Vec<Route>,
    /// Whether patterns match case-sensitively unless a route says otherwise: true when the
    /// project uses `redirects`, `rewrites`, `headers`, `cleanUrls` or `trailingSlash`, false for
    /// `routes` alone, as on Vercel.
    #[serde(default, rename = "caseSensitive")]
    pub case_sensitive: bool,
    /// `cleanUrls`: `/about` serves `/about.html`.
    #[serde(default, rename = "cleanUrls", skip_serializing_if = "std::ops::Not::not")]
    pub clean_urls: bool,
    #[serde(default, rename = "trailingSlash", skip_serializing_if = "Option::is_none")]
    pub trailing_slash: Option<bool>,
    /// The paths that are the deployment's function (`/api/main.py`, `/api/main`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub function: Vec<String>,
    /// Requests nothing else answers go to the function: a framework's server, or an API whose
    /// `vercel.json` routes nothing to it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    /// The deployment's bulk redirects (`bulkRedirectsPath`): the id of their table,
    /// `static/redirects/<id>` (see `bulk`), looked up before any route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bulk: Option<String>,
    /// Per-file functions' own maxDuration in seconds, by each of their paths (`function_paths`):
    /// the router's limit for a request the routes send to one, as Vercel limits each function by
    /// its own. Others have the deployment's. Routers from before it read every function's as the
    /// deployment's, which is the longest of them.
    #[serde(default, rename = "maxDuration", skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub max_duration: std::collections::BTreeMap<String, u64>,
    /// A Build Output's `wildcard`: a value per domain (Vercel's internationalization), which the
    /// routes use as `$wildcard`; a domain not listed gives the empty string. Routers from before it
    /// leave `$wildcard` as it is.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub wildcard: std::collections::BTreeMap<String, String>,
    /// A Build Output's prerenders (Vercel's ISR), by each path their function answers at: the
    /// router keeps those functions' answers as pages (`prerender`). Routers from before it run
    /// them as plain functions, rendering every request.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub prerenders: std::collections::BTreeMap<String, Prerender>,
}

/// What `vercel.json` keys build a program.
pub const ROUTING_KEYS: [&str; 6] = ["routes", "redirects", "rewrites", "headers", "cleanUrls", "trailingSlash"];

/// The deployment's function, as far as routing cares.
#[derive(Debug, Clone, Default)]
pub struct Function {
    /// Its entry, relative to the project (`api/main.py`), when a path can name it.
    pub entry: Option<String>,
    /// Whether it serves what nothing else does, whatever the routes say (a framework's server).
    pub catch_all: bool,
    /// Per-file functions (Vercel's `api/*.go`, `api/*.rs`), relative to the project, all served by
    /// the one deployment's function: each at its paths (`function_paths`), a `[param]` file by a
    /// route that puts the segment in the query, as Vercel's zero-config routes do
    /// (`file_routes`), and any other `/api` path not found.
    pub files: Vec<String>,
}

/// Vercel's zero-config routes for per-file functions (`@vercel/fs-detectors`' `getRouteResult`):
/// one for each file with a dynamic segment (`api/users/[id].go`: `^/api/users/([^/]+)$` to
/// `/api/users/[id]?id=$1`, checked), most segments first, then a 404 for the rest of `/api`.
/// As `routes` items, to go in the filesystem phase.
pub fn file_routes(files: &[String]) -> Vec<Value> {
    let mut dynamic: Vec<&String> = files.iter().filter(|f| f.split('/').any(|s| segment_name(s).is_some())).collect();
    // Vercel's order (`sortFilesBySegmentCount`): more segments first; then fewer dynamic ones at
    // the end of the path; then by name.
    dynamic.sort_by(|a, b| {
        let (sa, sb) = (a.split('/').count(), b.split('/').count());
        sb.cmp(&sa).then(trailing_dynamic(a).cmp(&trailing_dynamic(b))).then(a.cmp(b))
    });
    let mut routes: Vec<Value> = Vec::new();
    for file in dynamic {
        let parts: Vec<&str> = file.split('/').collect();
        let mut query: Vec<String> = Vec::new();
        let mut src: Vec<String> = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let last = i == parts.len() - 1;
            if let Some(name) = segment_name(part) {
                query.push(format!("{name}=${}", query.len() + 1));
                src.push("([^/]+)".to_owned());
            } else if last {
                let (stem, ext) = part.rsplit_once('.').map_or((*part, ""), |(s, e)| (s, e));
                let names = if stem == "index" { vec!["/".to_owned(), format!("/{}", escape(stem)), format!("/{}\\.{}", escape(stem), escape(ext))] } else { vec![format!("{}/", escape(stem)), escape(stem), format!("{}\\.{}", escape(stem), escape(ext))] };
                src.push(format!("({}){}", names.join("|"), if stem == "index" { "?" } else { "" }));
            } else {
                src.push(escape(part));
            }
        }
        let index = parts.last().is_some_and(|l| l.rsplit_once('.').map_or(*l, |(s, _)| s) == "index");
        let src = if index && src.len() > 1 { format!("^/{}{}$", src[..src.len() - 1].join("/"), src[src.len() - 1]) } else { format!("^/{}$", src.join("/")) };
        let extensionless = file.rsplit_once('.').map_or(file.as_str(), |(s, _)| s);
        let qs = if query.is_empty() { String::new() } else { format!("?{}", query.join("&")) };
        routes.push(serde_json::json!({ "src": src, "dest": format!("/{extensionless}{qs}"), "check": true }));
    }
    routes.push(serde_json::json!({ "src": "^/api(/.*)?$", "status": 404 }));
    routes
}

/// How many dynamic segments a file's path ends with (`api/[a]/b/[c]/[d].ts`: 2).
pub fn trailing_dynamic(file: &str) -> usize {
    file.split('/').fold(0, |n, s| if segment_name(s).is_some() { n + 1 } else { 0 })
}

/// A path segment's dynamic name: `id` for `[id]` or `[id].go`.
fn segment_name(segment: &str) -> Option<&str> {
    let stem = segment.rsplit_once('.').map_or(segment, |(s, _)| s);
    stem.strip_prefix('[').and_then(|s| s.strip_suffix(']')).filter(|s| !s.is_empty())
}

/// A file name as a regex matches it literally (Vercel's `escapeName`).
fn escape(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if "[]^$.|?*+()".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The URL paths of a function whose entry is `entry`: `/api/main.py` and `/api/main`, and for an
/// `index` file its folder, `/api`.
pub fn function_paths(entry: &str) -> Vec<String> {
    let entry = entry.trim_start_matches("./").trim_start_matches('/');
    let mut out = vec![format!("/{entry}")];
    if let Some((stem, _)) = entry.rsplit_once('.')
        && !stem.is_empty()
        && !stem.ends_with('/')
    {
        out.push(format!("/{stem}"));
        if let Some(dir) = stem.strip_suffix("/index") {
            out.push(format!("/{dir}"));
        } else if stem == "index" {
            out.push("/".to_owned());
        }
    }
    out
}

/// The URL paths of a per-file function (`api/users.ts`): `function_paths`, and for an `index`
/// file also its folder with the slash (`/api/`), as a folder's `index.html` is served.
pub fn file_paths(file: &str) -> Vec<String> {
    let mut out = function_paths(file);
    if let Some((stem, _)) = file.trim_start_matches("./").trim_start_matches('/').rsplit_once('.')
        && let Some(dir) = stem.strip_suffix("/index")
    {
        out.push(format!("/{dir}/"));
    }
    out
}

impl Program {
    /// The program for a project's `vercel.json` (`config`), or `None` when it has no routing keys
    /// and so routes as a deployment always has: static files, then the function.
    /// `preset`: the framework preset's own routes (its name, and the routes as `routes` JSON),
    /// which go around the project's as Vercel's `mergeRoutes` puts a builder's.
    pub fn from_vercel(config: &Map<String, Value>, function: &Function, preset: Option<(&str, &Value)>) -> Result<Option<Program>, String> {
        Program::with_middleware(config, function, preset, &[])
    }

    /// `from_vercel`, with the project's Routing Middleware (`middleware.rs`): its routes go after
    /// the project's own in each phase and before the preset's checked ones, where Vercel's
    /// `mergeRoutes` puts a build marked `projectMiddleware`.
    pub fn with_middleware(config: &Map<String, Value>, function: &Function, preset: Option<(&str, &Value)>, middleware: &[SourceRoute]) -> Result<Option<Program>, String> {
        if !ROUTING_KEYS.iter().any(|k| config.contains_key(*k)) && preset.is_none() && function.files.is_empty() && middleware.is_empty() {
            return Ok(None);
        }
        let new_keys = ["redirects", "rewrites", "headers", "cleanUrls", "trailingSlash"];
        if config.contains_key("routes")
            && let Some(key) = new_keys.iter().find(|k| config.contains_key(**k))
        {
            return Err(format!(
                "`routes` can't be used with `{key}`: use `rewrites`, `redirects` and `headers` instead (Vercel refuses the mix too)"
            ));
        }
        let clean_urls = boolean(config, "cleanUrls")?;
        let trailing_slash = boolean(config, "trailingSlash")?;
        let mut routes: Vec<Route> = Vec::new();
        if let Some(clean) = clean_urls {
            routes.extend(convert::clean_urls(clean, trailing_slash).into_iter().map(Route::Source));
        }
        if let Some(enable) = trailing_slash {
            routes.extend(convert::trailing_slash(enable).into_iter().map(Route::Source));
        }
        if let Some(list) = config.get("routes") {
            routes.extend(legacy_routes(list)?);
        }
        if let Some(list) = config.get("redirects") {
            for (i, item) in items(list, "redirects")?.iter().enumerate() {
                routes.push(Route::Source(redirect(item, i)?));
            }
        }
        if let Some(list) = config.get("headers") {
            for (i, item) in items(list, "headers")?.iter().enumerate() {
                routes.push(Route::Source(headers(item, i)?));
            }
        }
        if let Some(list) = config.get("rewrites") {
            let list = items(list, "rewrites")?;
            routes.push(convert::filesystem());
            for (i, item) in list.iter().enumerate() {
                routes.push(Route::Source(rewrite(item, i)?));
            }
        }
        // Per-file functions' routes go with a preset's, ahead of them in the filesystem phase,
        // so that a site's catch-all (a single-page app's `index.html`) can't take `/api`.
        let with_files;
        let preset = if function.files.is_empty() {
            preset
        } else {
            let mut list = vec![serde_json::json!({ "handle": "filesystem" })];
            list.extend(file_routes(&function.files));
            let name = preset.map_or("api", |(name, _)| name);
            if let Some((_, own)) = preset {
                let own = own.as_array().cloned().unwrap_or_default();
                // The preset's routes before its filesystem phase come first; from it, after ours.
                match own.iter().position(|r| r.get("handle").and_then(Value::as_str) == Some("filesystem")) {
                    Some(at) => {
                        let mut merged: Vec<Value> = own[..at].to_vec();
                        merged.extend(list);
                        merged.extend(own[at + 1..].iter().cloned());
                        list = merged;
                    }
                    None => {
                        let mut merged = own.clone();
                        merged.extend(list);
                        list = merged;
                    }
                }
            }
            with_files = Value::Array(list);
            Some((name, &with_files))
        };
        if let Some((name, list)) = preset {
            let mut own = legacy_routes(list).map_err(|e| format!("{name}'s own routes: {e}"))?;
            for route in &mut own {
                if let Route::Source(r) = route {
                    // `routes`-style, so matched without regard to case whatever the project's
                    // are, unless the route says (a Build Output's may).
                    r.case_sensitive.get_or_insert(false);
                    r.from = Some(format!("{name}'s {}", r.from.as_deref().unwrap_or("route")));
                }
            }
            routes = merge_routes(routes, middleware.iter().cloned().map(Route::Source).collect(), own);
        } else if !middleware.is_empty() {
            routes = merge_routes(routes, middleware.iter().cloned().map(Route::Source).collect(), Vec::new());
        }
        let mut paths = function.entry.as_deref().map(function_paths).unwrap_or_default();
        for file in &function.files {
            for path in file_paths(file) {
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        let mut program = Program {
            routes,
            case_sensitive: new_keys.iter().any(|k| config.contains_key(*k)),
            clean_urls: clean_urls == Some(true),
            trailing_slash,
            fallback: function.catch_all || (function.entry.is_some() && !config.contains_key("routes") && !config.contains_key("rewrites")),
            function: paths,
            bulk: None,
            max_duration: Default::default(),
            wildcard: Default::default(),
            prerenders: Default::default(),
        };
        // Every pattern must compile, here rather than in the router.
        Engine::new(&program)?;
        // Routes that only send everything to the function, as most APIs from Vercel have, route
        // exactly as having none does.
        if function.files.is_empty() && program.only_reaches_function() {
            program.routes.clear();
            program.fallback = true;
        }
        if program.routes.is_empty() && !program.clean_urls && program.trailing_slash.is_none() && program.fallback == function.entry.is_some() {
            return Ok(None);
        }
        Ok(Some(program))
    }

    /// The program for a framework's Build Output (Vercel's Build Output API, `config.json`'s
    /// `routes`), with the project's own `vercel.json` routing keys around it as `vercel build`
    /// merges them (the framework's routes are the builder's). `functions` are the paths its
    /// functions answer at (each `.func` name, with the aliases the filesystem gives it), and
    /// `max_duration` each one's own limit. Nothing reaches a function but its routes: there's
    /// no catch-all of forkrail's own.
    pub fn from_build_output(config: &Map<String, Value>, framework: &str, routes: &Value, functions: Vec<String>, max_duration: std::collections::BTreeMap<String, u64>, wildcard: std::collections::BTreeMap<String, String>) -> Result<Program, String> {
        let mut program = Program::from_vercel(config, &Function::default(), Some((framework, routes)))?.unwrap_or_default();
        program.function = functions;
        program.fallback = false;
        program.max_duration = max_duration;
        program.wildcard = wildcard.into_iter().map(|(domain, value)| (domain.to_ascii_lowercase(), value)).collect();
        Engine::new(&program)?;
        Ok(program)
    }

    /// Add a project's Routing Middleware route to a program `from_vercel` made, where
    /// `with_middleware` would have put it: in the first phase, after the project's own routes
    /// and the preset's leading `continue` ones, before the preset's others. A preset's routes are
    /// the ones `from_vercel` names `"<preset>'s ..."`.
    pub fn insert_middleware(&mut self, route: SourceRoute) {
        let preset = |r: &SourceRoute| r.from.as_deref().is_some_and(|f| f.contains("'s "));
        let first_phase = self.routes.iter().position(|r| matches!(r, Route::Handle { .. })).unwrap_or(self.routes.len());
        let mut at = 0;
        for (i, r) in self.routes[..first_phase].iter().enumerate() {
            if let Route::Source(s) = r
                && (!preset(s) || (s.cont && !s.check && !s.override_))
            {
                at = i + 1;
            }
        }
        self.routes.insert(at, Route::Source(route));
    }

    /// Whether every route is a plain catch-all whose destination is the function.
    fn only_reaches_function(&self) -> bool {
        const CATCH_ALL: [&str; 6] = ["^/(.*)$", "^/.*$", "^(.*)$", "^.*$", "^(?:/(.*))$", "^/(.+)?$"];
        !self.function.is_empty()
            && !self.routes.is_empty()
            && self.routes.iter().all(|r| match r {
                Route::Handle { handle } => *handle == Phase::Filesystem,
                Route::Source(s) => {
                    let dest_path = s.dest.as_deref().map(|d| d.split(['?', '#']).next().unwrap_or(""));
                    let dest_path = dest_path.map(|d| if d.starts_with('/') { d.to_owned() } else { format!("/{d}") });
                    CATCH_ALL.contains(&s.src.as_str())
                        && dest_path.is_some_and(|d| self.function.contains(&d))
                        && s.dest.as_deref().is_some_and(|d| !d.contains('?'))
                        && s.headers.is_empty()
                        && s.methods.is_none()
                        && !s.cont
                        && s.status.is_none()
                        && s.has.is_empty()
                        && s.missing.is_empty()
                        && s.transforms.is_empty()
                }
            })
    }

    /// How many routes match a pattern (not phase markers).
    pub fn rule_count(&self) -> usize {
        self.routes.iter().filter(|r| matches!(r, Route::Source(_))).count()
    }
}

fn boolean(config: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match config.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("`{key}` must be true or false")),
    }
}

fn items<'a>(list: &'a Value, what: &str) -> Result<&'a Vec<Value>, String> {
    let Some(list) = list.as_array() else { return Err(format!("`{what}` must be an array")) };
    if list.len() > MAX_RULES {
        return Err(format!("`{what}` has {} items; Vercel's limit is {MAX_RULES}", list.len()));
    }
    Ok(list)
}

fn object<'a>(item: &'a Value, what: &str, allowed: &[&str], later: &[(&str, &str)]) -> Result<&'a Map<String, Value>, String> {
    let Some(item) = item.as_object() else { return Err(format!("{what} must be an object")) };
    for key in item.keys() {
        if let Some((_, when)) = later.iter().find(|(k, _)| k == key) {
            return Err(format!("{what}.{key} isn't supported yet ({when})"));
        }
        if !allowed.contains(&key.as_str()) {
            return Err(format!("{what} has an unknown property `{key}`"));
        }
    }
    Ok(item)
}

fn text<'a>(item: &'a Map<String, Value>, key: &str, what: &str, max: usize) -> Result<Option<&'a str>, String> {
    match item.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if s.chars().count() <= max => Ok(Some(s)),
        Some(Value::String(_)) => Err(format!("{what}.{key} is longer than {max} characters, Vercel's limit")),
        Some(_) => Err(format!("{what}.{key} must be text")),
    }
}

fn required<'a>(item: &'a Map<String, Value>, key: &str, what: &str) -> Result<&'a str, String> {
    text(item, key, what, MAX_PATTERN)?.ok_or_else(|| format!("{what} needs `{key}`"))
}

fn status(item: &Map<String, Value>, key: &str, what: &str) -> Result<Option<u16>, String> {
    match item.get(key) {
        None => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if (100..=999).contains(&n) => Ok(Some(n as u16)),
            _ => Err(format!("{what}.{key} must be a status code from 100 to 999")),
        },
    }
}

/// What `later` tells the user about keys a later phase brings.
const LATER_ENV: (&str, &str) = ("env", "environment variables in routes come later");

/// A route's `transforms`, checked as Vercel's schema checks them. In `rewrites` (`rewrite`) only
/// `request.path` is allowed, its `args` a path pattern whose `:name` parameters come from `source`
/// (`segments`) or `has` (`has_segments`); it's compiled here to `$1`-style references.
pub fn transforms(item: &Map<String, Value>, what: &str, rewrite: Option<(&[String], &[String])>) -> Result<Vec<Transform>, String> {
    let Some(list) = item.get("transforms") else { return Ok(Vec::new()) };
    let Some(list) = list.as_array().filter(|l| !l.is_empty()) else { return Err(format!("{what}.transforms must be a non-empty array")) };
    let mut out = Vec::new();
    for (i, t) in list.iter().enumerate() {
        let at = format!("{what}.transforms[{i}]");
        let obj = object(t, &at, &["type", "op", "target", "args"], &[("env", "environment variables in transforms need the routers to read secrets, which they can't"), ("fallback", "it isn't documented")])?;
        let mut transform: Transform = serde_json::from_value(t.clone()).map_err(|e| format!("{at}: {e}"))?;
        if let Some(target) = obj.get("target") {
            let ok = target.as_object().is_some_and(|o| o.len() == 1 && o.contains_key("key"));
            if !ok {
                return Err(format!("{at}.target must be {{\"key\": ...}}"));
            }
        }
        for arg in &transform.args {
            if arg.chars().count() > MAX_PATTERN {
                return Err(format!("{at}.args is longer than {MAX_PATTERN} characters"));
            }
        }
        if matches!(transform.op, TransformOp::Set | TransformOp::Append) && transform.args.is_empty() {
            return Err(format!("{at}: `{}` needs `args`", if transform.op == TransformOp::Set { "set" } else { "append" }));
        }
        if rewrite.is_some() && transform.kind != TransformKind::RequestPath {
            return Err(format!("{at}: in `rewrites` only `request.path` transforms are allowed; use `routes` for the others"));
        }
        match transform.kind {
            TransformKind::RequestPath => {
                if transform.op != TransformOp::Set {
                    return Err(format!("{at}: a `request.path` transform can only `set`"));
                }
                if transform.target.is_some() {
                    return Err(format!("{at}: a `request.path` transform has no `target`"));
                }
                if !obj.get("args").is_some_and(Value::is_string) {
                    return Err(format!("{at}.args must be one path"));
                }
                let path = &transform.args[0];
                let ok = path.starts_with('/')
                    && !path.starts_with("//")
                    && path.len() <= 2048
                    && !path.chars().any(|c| c == '?' || c == '#' || c.is_whitespace() || c.is_control());
                if !ok {
                    return Err(format!("{at}.args {path:?} must be a path starting with one `/`, with no query, `#` or spaces"));
                }
                if let Some((segments, has_segments)) = rewrite {
                    let mut indexes: Vec<(String, String)> = Vec::new();
                    for (i, name) in segments.iter().enumerate() {
                        indexes.push((name.clone(), format!("${}", i + 1)));
                    }
                    for name in has_segments {
                        indexes.push((name.clone(), format!("${name}")));
                    }
                    for name in p2r::param_names(path).unwrap_or_default() {
                        if !indexes.iter().any(|(n, _)| *n == name) {
                            return Err(format!("{at}: the path uses \":{name}\", which neither `source` nor `has` has"));
                        }
                    }
                    transform.args = vec![convert::safely_compile(path, &indexes, true).map_err(|e| format!("{at}: {e}"))?];
                }
            }
            kind => {
                let Some(target) = &transform.target else { return Err(format!("{at} needs `target`")) };
                match &target.key {
                    KeyMatch::Name(name) => {
                        let header = matches!(kind, TransformKind::RequestHeaders | TransformKind::ResponseHeaders);
                        if name.is_empty() || name.len() > MAX_PATTERN {
                            return Err(format!("{at}.target.key must be a name"));
                        }
                        if header && transform.op != TransformOp::Delete && !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                            return Err(format!("{at}.target.key {name:?} must be letters, digits, `-` and `_`"));
                        }
                    }
                    KeyMatch::Ops(ops) => {
                        if *ops == Ops::default() {
                            return Err(format!("{at}.target.key has no comparison"));
                        }
                        if ops.re.is_some() {
                            return Err(format!("{at}.target.key can't use `re` (Vercel doesn't allow it in transforms)"));
                        }
                        if transform.op == TransformOp::Set {
                            return Err(format!("{at}: `set` needs `target.key` to be a name"));
                        }
                    }
                }
                if matches!(kind, TransformKind::RequestHeaders | TransformKind::ResponseHeaders)
                    && let Some(bad) = transform.args.iter().find(|a| a.is_empty() || a.chars().any(|c| !(c == ' ' || c.is_ascii_graphic())))
                {
                    return Err(format!("{at}.args {bad:?} isn't a header value"));
                }
            }
        }
        out.push(transform);
    }
    Ok(out)
}

fn conditions(item: &Map<String, Value>, key: &str, what: &str) -> Result<Vec<Condition>, String> {
    let Some(list) = item.get(key) else { return Ok(Vec::new()) };
    let Some(list) = list.as_array() else { return Err(format!("{what}.{key} must be an array")) };
    if list.len() > MAX_CONDITIONS {
        return Err(format!("{what}.{key} has {} items; Vercel's limit is {MAX_CONDITIONS}", list.len()));
    }
    let mut out = Vec::new();
    for (i, c) in list.iter().enumerate() {
        let at = format!("{what}.{key}[{i}]");
        let condition: Condition = serde_json::from_value(c.clone()).map_err(|e| format!("{at}: {e}"))?;
        if let Some(obj) = c.as_object()
            && let Some(extra) = obj.keys().find(|k| !["type", "key", "value"].contains(&k.as_str()))
        {
            return Err(format!("{at} has an unknown property `{extra}`"));
        }
        match (condition.kind, &condition.key, &condition.value) {
            (ConditionKind::Host, _, None) => return Err(format!("{at}: a host condition needs `value`")),
            (ConditionKind::Host, Some(_), _) => return Err(format!("{at}: a host condition has no `key`")),
            (ConditionKind::Header | ConditionKind::Cookie | ConditionKind::Query, None, _) => return Err(format!("{at} needs `key`")),
            _ => {}
        }
        if let Some(Matchable::Ops(ops)) = &condition.value {
            if *ops == Ops::default() {
                return Err(format!("{at}.value has no comparison"));
            }
            if let Some(eq) = &ops.eq
                && !eq.is_string()
                && !eq.is_number()
            {
                return Err(format!("{at}.value.eq must be text or a number"));
            }
        }
        out.push(condition);
    }
    Ok(out)
}

/// Every named parameter a destination uses must come from the source or a `has` condition.
fn check_destination(source: &str, destination: &str, has: &[Condition], what: &str) -> Result<(), String> {
    let (_, segments) = convert::source_to_regex(source).map_err(|e| format!("{what}.source {source:?} isn't a valid pattern: {e}"))?;
    let from_has = convert::has_segments(has);
    let url = convert::NodeUrl::parse(destination);
    let mut used = Vec::new();
    for part in [url.hostname.as_deref().unwrap_or(""), url.pathname.as_deref().unwrap_or("")] {
        used.extend(p2r::param_names(part).unwrap_or_default());
    }
    for (_, values) in &url.query {
        if let Some(first) = values.first() {
            used.extend(p2r::param_names(first).unwrap_or_default());
        }
    }
    if let Some(name) = used.iter().find(|n| !segments.contains(n) && !from_has.contains(n)) {
        return Err(format!("{what} uses \":{name}\" in `destination`, but neither `source` nor `has` has it"));
    }
    Ok(())
}

fn redirect(item: &Value, i: usize) -> Result<SourceRoute, String> {
    let what = format!("redirects[{i}]");
    let item = object(item, &what, &["source", "destination", "permanent", "statusCode", "has", "missing"], &[LATER_ENV])?;
    let source = required(item, "source", &what)?;
    let destination = required(item, "destination", &what)?;
    let permanent = match item.get("permanent") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(format!("{what}.permanent must be true or false")),
    };
    let code = status(item, "statusCode", &what)?;
    if permanent.is_some() && code.is_some() {
        return Err(format!("{what} can't have both `permanent` and `statusCode`"));
    }
    if let Some(code) = code
        && ![301, 302, 303, 307, 308].contains(&code)
    {
        return Err(format!("{what}.statusCode must be 301, 302, 303, 307 or 308"));
    }
    let has = conditions(item, "has", &what)?;
    let missing = conditions(item, "missing", &what)?;
    check_destination(source, destination, &has, &what)?;
    convert::redirect(convert::Redirect { source, destination, permanent, status: code, has, missing }, what.clone())
        .map_err(|e| format!("{what}: {e}"))
}

fn rewrite(item: &Value, i: usize) -> Result<SourceRoute, String> {
    let what = format!("rewrites[{i}]");
    let item = object(
        item,
        &what,
        &["source", "destination", "statusCode", "has", "missing", "transforms"],
        &[LATER_ENV, ("respectOriginCacheControl", "caching comes with the cache work")],
    )?;
    let source = required(item, "source", &what)?;
    let destination = match item.get("destination") {
        Some(Value::Object(_)) => return Err(format!("{what}.destination: services aren't supported")),
        _ => required(item, "destination", &what)?,
    };
    let code = status(item, "statusCode", &what)?;
    let has = conditions(item, "has", &what)?;
    let missing = conditions(item, "missing", &what)?;
    check_destination(source, destination, &has, &what)?;
    let (_, segments) = convert::source_to_regex(source).map_err(|e| format!("{what}.source {source:?} isn't a valid pattern: {e}"))?;
    let transforms = transforms(item, &what, Some((&segments, &convert::has_segments(&has))))?;
    let mut route = convert::rewrite(convert::Rewrite { source, destination, status: code, has, missing }, what.clone()).map_err(|e| format!("{what}: {e}"))?;
    route.transforms = transforms;
    Ok(route)
}

fn headers(item: &Value, i: usize) -> Result<SourceRoute, String> {
    let what = format!("headers[{i}]");
    let item = object(item, &what, &["source", "headers", "has", "missing"], &[])?;
    let source = required(item, "source", &what)?;
    let Some(list) = item.get("headers").and_then(Value::as_array) else { return Err(format!("{what}.headers must be an array of {{key, value}}")) };
    if list.len() > MAX_HEADERS_PER_RULE {
        return Err(format!("{what}.headers has {} items; Vercel's limit is {MAX_HEADERS_PER_RULE}", list.len()));
    }
    let mut pairs = Vec::new();
    for (j, h) in list.iter().enumerate() {
        let at = format!("{what}.headers[{j}]");
        let h = object(h, &at, &["key", "value"], &[])?;
        let key = required(h, "key", &at)?;
        let value = text(h, "value", &at, MAX_HEADER_VALUE)?.ok_or_else(|| format!("{at} needs `value`"))?;
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(format!("{at}.key {key:?} isn't a header name"));
        }
        pairs.push((key.to_owned(), value.to_owned()));
    }
    let has = conditions(item, "has", &what)?;
    let missing = conditions(item, "missing", &what)?;
    convert::headers(convert::Headers { source, headers: pairs, has, missing }, what.clone()).map_err(|e| format!("{what}: {e}"))
}

/// Vercel's `mergeRoutes` (`@vercel/routing-utils`), for a project's routes, its Routing
/// Middleware's and a builder's (here a framework preset's): phase by phase, the builder's routes
/// that continue, then the project's, then the middleware's, then the builder's that `check`, then
/// the rest of the builder's. A builder's route with `override` is one of the rest.
fn merge_routes(project: Vec<Route>, middleware: Vec<Route>, builder: Vec<Route>) -> Vec<Route> {
    type Phased = Vec<(Option<Phase>, Vec<SourceRoute>)>;
    fn by_phase(routes: Vec<Route>) -> Phased {
        let mut out: Phased = Vec::new();
        let mut phase = None;
        for route in routes {
            match route {
                Route::Handle { handle } => phase = Some(handle),
                Route::Source(r) => match out.iter_mut().find(|(p, _)| *p == phase) {
                    Some((_, list)) => list.push(r),
                    None => out.push((phase, vec![r])),
                },
            }
        }
        out
    }
    let (project, middleware, builder) = (by_phase(project), by_phase(middleware), by_phase(builder));
    let mut phases: Vec<Option<Phase>> = vec![None];
    for (phase, _) in project.iter().chain(&middleware).chain(&builder) {
        if !phases.contains(phase) {
            phases.push(*phase);
        }
    }
    let take = |list: &Phased, phase: Option<Phase>| list.iter().find(|(p, _)| *p == phase).map(|(_, l)| l.clone()).unwrap_or_default();
    let mut out = Vec::new();
    for phase in phases {
        let (mine, between, theirs) = (take(&project, phase), take(&middleware, phase), take(&builder, phase));
        if let Some(handle) = phase
            && !(mine.is_empty() && between.is_empty() && theirs.is_empty())
        {
            out.push(Route::Handle { handle });
        }
        let (checks, rest): (Vec<SourceRoute>, Vec<SourceRoute>) = theirs.into_iter().partition(|r| r.check && !r.override_);
        let (continues, others): (Vec<SourceRoute>, Vec<SourceRoute>) = rest.into_iter().partition(|r| r.cont && !r.override_);
        out.extend(continues.into_iter().map(Route::Source));
        out.extend(mine.into_iter().map(Route::Source));
        out.extend(between.into_iter().map(Route::Source));
        out.extend(checks.into_iter().map(Route::Source));
        out.extend(others.into_iter().map(Route::Source));
    }
    out
}

fn legacy_routes(list: &Value) -> Result<Vec<Route>, String> {
    let list = items(list, "routes")?;
    let mut out = Vec::new();
    let mut seen: Vec<Phase> = Vec::new();
    for (i, item) in list.iter().enumerate() {
        let what = format!("routes[{i}]");
        let Some(obj) = item.as_object() else { return Err(format!("{what} must be an object")) };
        if let Some(handle) = obj.get("handle") {
            if obj.len() != 1 {
                return Err(format!("{what} has `handle` and other properties; a handle route has only `handle`"));
            }
            let phase = match handle.as_str() {
                Some("filesystem") => Phase::Filesystem,
                Some("hit") => Phase::Hit,
                Some("miss") => Phase::Miss,
                Some("error") => Phase::Error,
                Some(h @ ("rewrite" | "resource")) => return Err(format!("{what}: `handle: {h}` isn't supported yet")),
                _ => return Err(format!("{what} has an unknown `handle` value {handle}")),
            };
            if seen.contains(&phase) {
                return Err(format!("{what} repeats `handle: {}`; use each at most once", json_name(phase)));
            }
            seen.push(phase);
            out.push(Route::Handle { handle: phase });
            continue;
        }
        let obj = object(
            item,
            &what,
            &[
                "src",
                "source",
                "dest",
                "destination",
                "headers",
                "methods",
                "continue",
                "caseSensitive",
                "check",
                "status",
                "statusCode",
                "has",
                "missing",
                "transforms",
                "middlewarePath",
                "middlewareRawSrc",
                "override",
                "mitigate",
            ],
            &[LATER_ENV, ("locale", "locales aren't supported"), ("important", "it's deprecated on Vercel")],
        )?;
        let pick = |a: &str, b: &str| -> Result<Option<&str>, String> {
            match (text(obj, a, &what, MAX_PATTERN)?, text(obj, b, &what, MAX_PATTERN)?) {
                (Some(_), Some(_)) => Err(format!("{what} can't have both `{a}` and `{b}`")),
                (x, y) => Ok(x.or(y)),
            }
        };
        let Some(src) = pick("src", "source")? else { return Err(format!("{what} needs `src`")) };
        if matches!(obj.get("destination"), Some(Value::Object(_))) {
            return Err(format!("{what}.destination: services aren't supported"));
        }
        let dest = pick("dest", "destination")?;
        let code = match (status(obj, "status", &what)?, status(obj, "statusCode", &what)?) {
            (Some(_), Some(_)) => return Err(format!("{what} can't have both `status` and `statusCode`")),
            (x, y) => x.or(y),
        };
        let flag = |key: &str| -> Result<bool, String> {
            match obj.get(key) {
                None => Ok(false),
                Some(Value::Bool(b)) => Ok(*b),
                Some(_) => Err(format!("{what}.{key} must be true or false")),
            }
        };
        let mut headers = Vec::new();
        if let Some(h) = obj.get("headers") {
            let Some(h) = h.as_object() else { return Err(format!("{what}.headers must be an object of names and values")) };
            if h.len() > 100 {
                return Err(format!("{what}.headers has {} names; Vercel's limit is 100", h.len()));
            }
            for (key, value) in h {
                let Some(value) = value.as_str() else { return Err(format!("{what}.headers.{key} must be text")) };
                headers.push(Header { key: key.clone(), value: value.to_owned() });
            }
        }
        let methods = match obj.get("methods") {
            None => None,
            Some(Value::Array(m)) if m.len() <= 10 && m.iter().all(Value::is_string) => {
                Some(m.iter().filter_map(Value::as_str).map(str::to_ascii_uppercase).collect())
            }
            Some(_) => return Err(format!("{what}.methods must be a list of up to 10 methods")),
        };
        let case_sensitive = match obj.get("caseSensitive") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(format!("{what}.caseSensitive must be true or false")),
        };
        let middleware = middleware::route_function(obj, &what)?;
        if middleware.is_some() && dest.is_some() {
            return Err(format!("{what} has both `middlewarePath` and `dest`; a middleware route only runs the middleware"));
        }
        let route = convert::legacy_route(SourceRoute {
            src: src.to_owned(),
            dest: dest.map(str::to_owned),
            headers,
            methods,
            cont: flag("continue")?,
            check: flag("check")?,
            status: code,
            case_sensitive,
            has: conditions(obj, "has", &what)?,
            missing: conditions(obj, "missing", &what)?,
            transforms: transforms(obj, &what, None)?,
            from: Some(what.clone()),
            middleware,
            override_: flag("override")?,
            mitigate: match obj.get("mitigate") {
                None => None,
                Some(v) => Some(serde_json::from_value::<Mitigate>(v.clone()).map_err(|_| format!("{what}.mitigate must be {{\"action\": \"challenge\"}} or {{\"action\": \"deny\"}}"))?),
            },
        });
        // Vercel's rules for what each phase's routes may do.
        match seen.last() {
            Some(Phase::Hit) if route.dest.is_some() || route.status.is_some() || !route.cont => {
                return Err(format!("{what}: routes after `handle: hit` may only add headers, with `continue: true`"));
            }
            Some(Phase::Miss) if route.dest.is_some() && !route.check => {
                return Err(format!("{what}: a route with `dest` after `handle: miss` needs `check: true`"));
            }
            Some(Phase::Miss) if route.dest.is_none() && !route.cont => {
                return Err(format!("{what}: a route without `dest` after `handle: miss` needs `continue: true`"));
            }
            _ => {}
        }
        out.push(Route::Source(route));
    }
    Ok(out)
}

fn json_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Filesystem => "filesystem",
        Phase::Hit => "hit",
        Phase::Miss => "miss",
        Phase::Error => "error",
    }
}

#[cfg(test)]
mod tests;
