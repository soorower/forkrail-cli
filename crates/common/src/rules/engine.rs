//! Running a deployment's routes against a request, as Vercel does: the routes before
//! `{ handle: "filesystem" }`, then the deployment's files, then the routes after it, with the
//! `miss`, `hit` and `error` phases where they apply. The result says what answers: a redirect, a
//! static file, the function, a URL elsewhere, or an error status, with the headers to add.
//!
//! A function always gets the path the client asked for, as on Vercel; a rewrite only chooses it,
//! and sets the query parameters its destination has.
//!
//! Checked against Vercel's own answers with `tools/routing-check.py`.

use super::convert::{parse_query, percent_decode};
use super::{Condition, ConditionKind, Matchable, Phase, Program, Route, Transform};

/// What the deployment's files hold, for the engine to check paths against: its static files'
/// manifest.
pub trait Files {
    fn has(&self, path: &str) -> bool;
}

impl Files for crate::statics::Manifest {
    fn has(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }
}

/// A deployment with no static files.
pub struct NoFiles;

impl Files for NoFiles {
    fn has(&self, _: &str) -> bool {
        false
    }
}

/// The request, as routing sees it.
pub struct Request<'a> {
    pub method: &'a str,
    /// The path as the client sent it, starting with `/`, still percent-encoded.
    pub path: &'a str,
    /// The query string without its `?`.
    pub query: Option<&'a str>,
    /// Without its port.
    pub host: &'a str,
    pub headers: &'a http::HeaderMap,
}

/// What answers the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Redirect { location: String },
    /// A static file, by its path in the manifest.
    Static { file: String },
    /// The function. With `query`, the request's query string becomes that: the client's with the
    /// destinations' parameters set over it. Without, it stays as the client sent it.
    Function { query: Option<String> },
    /// A URL elsewhere, proxied.
    External { url: String },
    /// An error page with `Decision::status`.
    Error,
    /// Nothing serves it, and there's no `404.html`.
    NotFound,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub outcome: Outcome,
    /// The status the routes set, if any (a redirect always has one).
    pub status: Option<u16>,
    /// Response headers the routes add, lowercase names, in order.
    pub headers: Vec<(String, String)>,
    /// The route that decided, by its index in the program, if one did.
    pub route: Option<usize>,
    /// Every matched route's transforms, in order, their `$1` and `$name` filled in.
    pub transforms: Vec<Transform>,
    /// With `Outcome::Function`: the function path the routes reached (`/api/users`, or a dynamic
    /// file's `/api/[id]`), when one of the deployment's own did rather than the catch-all. A
    /// deployment of several functions in one (per-file functions) dispatches on it.
    pub function: Option<String>,
    /// With `Outcome::Function`: the captures of the route that sent the request to its
    /// destination (`prerender::captures`), for a prerender's `x-now-route-matches`; `None` when
    /// the path itself was the function's.
    pub captures: Option<Vec<(String, String)>>,
    /// With `Outcome::Static`: the path the filesystem was asked for, after the routes' rewrites
    /// (`/static/index.html` for a rewrite to it, `/docs` for a folder's `index.html`, `/404` for
    /// a `cleanUrls` site's 404 page). Vercel names a file it serves after its last segment
    /// (`content-disposition`), not after the file the path found.
    pub looked_up: Option<String>,
}

/// A program ready to run: patterns compiled, routes grouped by phase.
pub struct Engine {
    main: PhaseRoutes,
    filesystem: PhaseRoutes,
    miss: PhaseRoutes,
    hit: PhaseRoutes,
    error: PhaseRoutes,
    has_filesystem: bool,
    function: Vec<String>,
    fallback: bool,
    clean_urls: bool,
    trailing_slash: Option<bool>,
    bulk: Option<String>,
    max_duration: std::collections::BTreeMap<String, u64>,
    /// Domain → `$wildcard`'s value (`Program::wildcard`).
    wildcard: std::collections::BTreeMap<String, String>,
    /// Function path → its prerender (`Program::prerenders`).
    prerenders: std::collections::BTreeMap<String, std::sync::Arc<super::Prerender>>,
}

/// One phase's routes, and their prefilter if there are many.
#[derive(Default)]
struct PhaseRoutes {
    routes: Vec<Compiled>,
    prefilter: Option<Prefilter>,
}

impl PhaseRoutes {
    fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

struct Compiled {
    index: usize,
    /// Where it came from in `vercel.json` (`redirects[3]`), for the access log.
    from: Option<String>,
    re: Re,
    dest: Option<String>,
    headers: Vec<(String, String)>,
    methods: Option<Vec<String>>,
    status: Option<u16>,
    cont: bool,
    check: bool,
    has: Vec<Cond>,
    missing: Vec<Cond>,
    transforms: Vec<Transform>,
    /// A Routing Middleware route's function: a match stops routing until it has answered.
    middleware: Option<String>,
    /// `mitigate`: a match stops routing until the router has challenged or denied it.
    mitigate: Option<super::MitigateAction>,
}

/// A JavaScript regex, run with `regex`, or `fancy-regex` for what `regex` lacks.
pub enum Re {
    Std(regex::Regex),
    Fancy(fancy_regex::Regex),
}

/// Phases with at least this many routes are prefiltered: one pass of a `RegexSet` over the path
/// says which routes can match, and only those are tried, in order. Measured on this Mac: 2,048
/// redirects cost 274 µs a request tried one by one.
const PREFILTER_FROM: usize = 32;

/// Which of a phase's routes can match a path, from one pass over it.
struct Prefilter {
    set: regex::RegexSet,
    /// The route (its position in the phase) each of the set's patterns belongs to.
    members: Vec<usize>,
    /// Routes the set can't hold (`fancy-regex` patterns): always tried.
    always: Vec<usize>,
}

impl Prefilter {
    fn new(routes: &[Compiled]) -> Option<Prefilter> {
        if routes.len() < PREFILTER_FROM {
            return None;
        }
        let mut patterns = Vec::new();
        let mut members = Vec::new();
        let mut always = Vec::new();
        for (i, route) in routes.iter().enumerate() {
            match &route.re {
                Re::Std(re) => {
                    patterns.push(re.as_str().to_owned());
                    members.push(i);
                }
                Re::Fancy(_) => always.push(i),
            }
        }
        let set = regex::RegexSetBuilder::new(&patterns).size_limit(64 << 20).dfa_size_limit(16 << 20).build().ok()?;
        Some(Prefilter { set, members, always })
    }

    /// For each route of the phase, whether it can match `path` (or `path` without its leading
    /// slash, which routes are also tried against).
    fn candidates(&self, path: &str, len: usize) -> Vec<bool> {
        let mut out = vec![false; len];
        for i in &self.always {
            out[*i] = true;
        }
        for m in self.set.matches(path).iter() {
            out[self.members[m]] = true;
        }
        if let Some(rest) = path.get(1..) {
            for m in self.set.matches(rest).iter() {
                out[self.members[m]] = true;
            }
        }
        out
    }
}

impl Re {
    pub fn new(pattern: &str, case_sensitive: bool) -> Result<Re, String> {
        let pattern = js_to_rust(pattern);
        let pattern = if case_sensitive { pattern } else { format!("(?i){pattern}") };
        match regex::Regex::new(&pattern) {
            Ok(re) => Ok(Re::Std(re)),
            // A deployment's own patterns, run on the router's threads for every request: a
            // tenth of fancy-regex's default backtracking, as the firewall allows.
            Err(std_err) => match fancy_regex::RegexBuilder::new(&pattern).backtrack_limit(100_000).build() {
                Ok(re) => Ok(Re::Fancy(re)),
                Err(_) => Err(std_err.to_string().lines().last().unwrap_or("not a valid regex").trim().to_owned()),
            },
        }
    }

    /// Whether it matches somewhere in `text`. A `fancy-regex` pattern that runs past its
    /// backtracking limit doesn't match.
    pub fn is_match(&self, text: &str) -> bool {
        match self {
            Re::Std(re) => re.is_match(text),
            Re::Fancy(re) => re.is_match(text).unwrap_or(false),
        }
    }

    /// The capture groups of the first match, with their names. A `fancy-regex` pattern that runs
    /// past its backtracking limit doesn't match.
    fn captures(&self, text: &str) -> Option<(Vec<Option<String>>, Vec<Option<String>>)> {
        match self {
            Re::Std(re) => {
                let caps = re.captures(text)?;
                let groups = (0..caps.len()).map(|i| caps.get(i).map(|m| m.as_str().to_owned())).collect();
                Some((groups, re.capture_names().map(|n| n.map(str::to_owned)).collect()))
            }
            Re::Fancy(re) => {
                let caps = re.captures(text).ok()??;
                let groups = (0..caps.len()).map(|i| caps.get(i).map(|m| m.as_str().to_owned())).collect();
                Some((groups, re.capture_names().map(|n| n.map(str::to_owned)).collect()))
            }
        }
    }
}

/// A JavaScript regex in Rust's dialect: `\/` is a plain slash, and `\d`, `\w` (and their
/// negations) are ASCII, as in JavaScript, not Unicode.
fn js_to_rust(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut chars = pattern.chars();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            match (chars.next(), in_class) {
                (Some('/'), _) => out.push('/'),
                (Some('d'), false) => out.push_str("[0-9]"),
                (Some('D'), false) => out.push_str("[^0-9]"),
                (Some('w'), false) => out.push_str("[0-9A-Za-z_]"),
                (Some('W'), false) => out.push_str("[^0-9A-Za-z_]"),
                (Some('d'), true) => out.push_str("0-9"),
                (Some('w'), true) => out.push_str("0-9A-Za-z_"),
                (Some(other), _) => {
                    out.push('\\');
                    out.push(other);
                }
                (None, _) => out.push_str("\\\\"),
            }
            continue;
        }
        match c {
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            _ => {}
        }
        out.push(c);
    }
    out
}

/// A compiled `has` or `missing` item (also Next.js's: `crate::next`).
pub struct Cond {
    kind: ConditionKind,
    key: Option<String>,
    matcher: Matcher,
}

enum Matcher {
    Present,
    Re(Re),
    Ops(Box<CompiledOps>),
}

struct CompiledOps {
    eq: Option<serde_json::Value>,
    neq: Option<String>,
    inc: Option<Vec<String>>,
    ninc: Option<Vec<String>>,
    pre: Option<String>,
    suf: Option<String>,
    re: Option<Re>,
    gt: Option<f64>,
    gte: Option<f64>,
    lt: Option<f64>,
    lte: Option<f64>,
}

impl Cond {
    pub fn new(c: &Condition) -> Result<Cond, String> {
        let matcher = match &c.value {
            None => Matcher::Present,
            // Vercel compares values without regard to case (what they capture keeps it).
            Some(Matchable::Text(text)) => Matcher::Re(Re::new(&format!("^(?:{text})$"), false).map_err(|e| format!("`has` value {text:?}: {e}"))?),
            Some(Matchable::Ops(ops)) => Matcher::Ops(Box::new(CompiledOps {
                eq: ops.eq.clone(),
                neq: ops.neq.clone(),
                inc: ops.inc.clone(),
                ninc: ops.ninc.clone(),
                pre: ops.pre.clone(),
                suf: ops.suf.clone(),
                re: ops.re.as_deref().map(|r| Re::new(r, false).map_err(|e| format!("`has` re {r:?}: {e}"))).transpose()?,
                gt: ops.gt,
                gte: ops.gte,
                lt: ops.lt,
                lte: ops.lte,
            })),
        };
        let key = match c.kind {
            ConditionKind::Header => c.key.as_ref().map(|k| k.to_ascii_lowercase()),
            _ => c.key.clone(),
        };
        Ok(Cond { kind: c.kind, key, matcher })
    }

    fn value(&self, req: &Request<'_>) -> Option<String> {
        let key = self.key.as_deref().unwrap_or("");
        match self.kind {
            ConditionKind::Host => Some(req.host.to_owned()),
            ConditionKind::Header => {
                let values: Vec<&str> = req.headers.get_all(key).iter().filter_map(|v| v.to_str().ok()).collect();
                (!values.is_empty()).then(|| values.join(", "))
            }
            ConditionKind::Cookie => req
                .headers
                .get_all(http::header::COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(';'))
                .filter_map(|pair| pair.trim().split_once('='))
                // Cookie names without regard to case, as Vercel matches them (query keys keep it).
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, value)| percent_decode(value.trim().trim_matches('"'))),
            ConditionKind::Query => req
                .query
                .and_then(|q| parse_query(q).into_iter().find(|(k, _)| k == key))
                .and_then(|(_, mut values)| (!values.is_empty()).then(|| values.swap_remove(0))),
        }
    }

    /// Whether it holds for `req`, as `has` reads it (a `missing` item holds when this is false).
    pub fn holds_for(&self, req: &Request<'_>) -> bool {
        self.holds(req, &mut Vec::new())
    }

    /// Whether it holds, adding the parameters it captures to `params`.
    fn holds(&self, req: &Request<'_>, params: &mut Vec<(String, String)>) -> bool {
        let Some(value) = self.value(req) else { return false };
        match &self.matcher {
            Matcher::Present => {
                if let Some(key) = &self.key {
                    params.push((key.clone(), value));
                }
                true
            }
            Matcher::Re(re) => {
                let Some((groups, names)) = re.captures(&value) else { return false };
                let mut named = false;
                for (i, name) in names.iter().enumerate() {
                    if let (Some(name), Some(Some(v))) = (name, groups.get(i)) {
                        params.push((name.clone(), v.clone()));
                        named = true;
                    }
                }
                if !named && self.kind == ConditionKind::Host {
                    params.push(("host".to_owned(), value));
                }
                true
            }
            Matcher::Ops(ops) => {
                let number = value.trim().parse::<f64>().ok();
                // Without regard to case, as Vercel compares.
                let lower = value.to_lowercase();
                let same = |s: &String| s.to_lowercase() == lower;
                let ok = ops.eq.as_ref().is_none_or(|eq| match eq {
                    serde_json::Value::String(s) => same(s),
                    serde_json::Value::Number(n) => number.is_some_and(|v| Some(v) == n.as_f64()),
                    _ => false,
                }) && ops.neq.as_ref().is_none_or(|s| !same(s))
                    && ops.inc.as_ref().is_none_or(|list| list.iter().any(same))
                    && ops.ninc.as_ref().is_none_or(|list| !list.iter().any(same))
                    && ops.pre.as_ref().is_none_or(|p| lower.starts_with(&p.to_lowercase()))
                    && ops.suf.as_ref().is_none_or(|s| lower.ends_with(&s.to_lowercase()))
                    && ops.gt.is_none_or(|g| number.is_some_and(|v| v > g))
                    && ops.gte.is_none_or(|g| number.is_some_and(|v| v >= g))
                    && ops.lt.is_none_or(|l| number.is_some_and(|v| v < l))
                    && ops.lte.is_none_or(|l| number.is_some_and(|v| v <= l));
                if !ok {
                    return false;
                }
                match &ops.re {
                    None => true,
                    Some(re) => {
                        let Some((groups, names)) = re.captures(&value) else { return false };
                        for (i, name) in names.iter().enumerate() {
                            if let (Some(name), Some(Some(v))) = (name, groups.get(i)) {
                                params.push((name.clone(), v.clone()));
                            }
                        }
                        true
                    }
                }
            }
        }
    }
}

/// Where routing stands while the routes run.
#[derive(Debug, Clone)]
struct State {
    /// The path the routes now see, after rewrites.
    path: String,
    /// Query parameters destinations added, in order.
    query: Vec<(String, String)>,
    /// Response headers, by lowercase name.
    headers: Vec<(String, String)>,
    status: Option<u16>,
    /// Transforms of the routes matched so far.
    transforms: Vec<Transform>,
    /// The captures of the last route whose destination the request took.
    captures: Option<Vec<(String, String)>>,
    /// For each of `transforms`, how many of `query` there were when its route matched: a
    /// `request.query` transform changes the query as it stood then, not parameters a later
    /// route's destination adds (SvelteKit deletes a client's `__pathname`, then sets its own).
    marks: Vec<usize>,
}

impl State {
    fn set_header(&mut self, key: String, value: String) {
        match self.headers.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.headers.push((key, value)),
        }
    }

    fn header(&self, key: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Take `dest` (a path, maybe with a query) as the path the routes see from now on.
    fn go_to(&mut self, dest: &str) {
        let dest = dest.split('#').next().unwrap_or("");
        let (path, query) = dest.split_once('?').unwrap_or((dest, ""));
        self.path = if path.starts_with('/') { path.to_owned() } else { format!("/{path}") };
        for (k, values) in parse_query(query) {
            for v in values {
                self.query.push((k.clone(), v));
            }
        }
    }
}

/// How a phase's routes ended.
enum Found {
    /// A route that doesn't continue matched. `url` is a URL elsewhere, if that's where it went.
    /// `exited`: its destination wasn't there and it has a status, so it ends with that status
    /// (Vercel's `exitWithStatus`).
    Route { index: usize, url: Option<String>, had_dest: bool, exited: bool },
    /// A Routing Middleware route matched: routing waits for its function, then goes on from the
    /// route at `next` in the phase.
    Middleware { index: usize, function: String, next: usize },
    /// A `mitigate` route matched: the router challenges or denies, and on a passed challenge
    /// routing goes on from the route at `next` in the phase.
    Mitigate { index: usize, action: super::MitigateAction, next: usize },
    Nothing,
}

/// Routing's next step: what answers, or a Routing Middleware to run first.
#[derive(Debug)]
pub enum Step {
    Decided(Decision),
    /// Run the middleware `function` (the route at `route` matched), then `Engine::resume` with
    /// `paused` and what it said.
    Middleware { function: String, route: usize, paused: Paused },
    /// The route at `route` says `mitigate`: deny, or challenge and, for a visitor who has passed
    /// the challenge, `Engine::resume` with `paused` and `Resume::Next`.
    Mitigate { action: super::MitigateAction, route: usize, paused: Paused },
}

/// Where routing stopped for a middleware: the phase, the route after the middleware's, and the
/// state then and at the phase's start.
#[derive(Debug, Clone)]
pub struct Paused {
    phase: usize,
    position: usize,
    state: State,
    before: State,
}

impl Paused {
    /// The response headers the routes before the middleware set, lowercase names.
    pub fn headers(&self) -> &[(String, String)] {
        &self.state.headers
    }

    /// The transforms of the routes matched so far.
    pub fn transforms(&self) -> &[Transform] {
        &self.state.transforms
    }
}

/// What a middleware said to do (`middleware::Verdict`), for routing to go on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// Go on as the request was.
    Next,
    /// Go on with this path (percent-encoded) in place of the request's. Its query is the
    /// request's own from here: the caller resumes with a request that has the rewrite's.
    Rewrite { path: String },
}

/// The phases a request goes through, in order, before what's left to the function or the error
/// phase.
const PHASES: [Option<Phase>; 2] = [None, Some(Phase::Filesystem)];

/// What serves a path in the deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Hit {
    Static(String),
    /// The function path that matched.
    Function(String),
}

/// Paths the routes never touch: the ACME challenge must reach whoever answers it.
const ACME_PREFIX: &str = "/.well-known/acme-challenge/";

/// Whether a path is an ACME challenge's: the prefix and one token (base64url, as RFC 8555 makes
/// them). Anything else under the prefix is routed as any path is, middleware and all.
fn acme_challenge(path: &str) -> bool {
    path.strip_prefix(ACME_PREFIX).is_some_and(|token| !token.is_empty() && token.len() <= 256 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

impl Engine {
    pub fn new(program: &Program) -> Result<Engine, String> {
        let mut engine = Engine {
            main: PhaseRoutes::default(),
            filesystem: PhaseRoutes::default(),
            miss: PhaseRoutes::default(),
            hit: PhaseRoutes::default(),
            error: PhaseRoutes::default(),
            has_filesystem: false,
            function: program.function.clone(),
            fallback: program.fallback,
            clean_urls: program.clean_urls,
            trailing_slash: program.trailing_slash,
            bulk: program.bulk.clone(),
            max_duration: program.max_duration.clone(),
            wildcard: program.wildcard.clone(),
            prerenders: program.prerenders.iter().map(|(path, p)| (path.clone(), std::sync::Arc::new(p.clone()))).collect(),
        };
        let mut phase: Option<Phase> = None;
        for (index, route) in program.routes.iter().enumerate() {
            match route {
                Route::Handle { handle } => {
                    phase = Some(*handle);
                    if *handle == Phase::Filesystem {
                        engine.has_filesystem = true;
                    }
                }
                Route::Source(r) => {
                    let what = r.from.clone().unwrap_or_else(|| format!("route {index}"));
                    // Vercel matches a middleware's route with regard to case (seen: `/OLD` doesn't run a
                    // middleware matching `/old`), whatever the program's other routes do.
                    let case_sensitive = if r.middleware.is_some() { r.case_sensitive.unwrap_or(true) } else { r.case_sensitive.unwrap_or(program.case_sensitive) };
                    let re = Re::new(&r.src, case_sensitive).map_err(|e| format!("{what}: the pattern {:?} isn't one forkrail can run: {e}", r.src))?;
                    if r.middleware.is_some() && phase.is_some() {
                        return Err(format!("{what}: Routing Middleware runs before the filesystem; forkrail doesn't run it in a `handle` phase"));
                    }
                    let compiled = Compiled {
                        index,
                        from: r.from.clone(),
                        re,
                        dest: r.dest.clone(),
                        headers: r.headers.iter().map(|h| (h.key.to_ascii_lowercase(), h.value.clone())).collect(),
                        methods: r.methods.clone(),
                        status: r.status,
                        cont: r.cont,
                        check: r.check,
                        has: r.has.iter().map(Cond::new).collect::<Result<_, _>>().map_err(|e| format!("{what}: {e}"))?,
                        missing: r.missing.iter().map(Cond::new).collect::<Result<_, _>>().map_err(|e| format!("{what}: {e}"))?,
                        transforms: r.transforms.clone(),
                        middleware: r.middleware.clone(),
                        mitigate: r.mitigate.map(|m| m.action),
                    };
                    match phase {
                        None => engine.main.routes.push(compiled),
                        Some(Phase::Filesystem) => engine.filesystem.routes.push(compiled),
                        Some(Phase::Miss) => engine.miss.routes.push(compiled),
                        Some(Phase::Hit) => engine.hit.routes.push(compiled),
                        Some(Phase::Error) => engine.error.routes.push(compiled),
                    }
                }
            }
        }
        for phase in [&mut engine.main, &mut engine.filesystem, &mut engine.miss, &mut engine.hit, &mut engine.error] {
            phase.prefilter = Prefilter::new(&phase.routes);
        }
        Ok(engine)
    }

    /// The same engine without its prefilters, for checking they change nothing.
    #[cfg(test)]
    pub(super) fn without_prefilter(mut self) -> Engine {
        for phase in [&mut self.main, &mut self.filesystem, &mut self.miss, &mut self.hit, &mut self.error] {
            phase.prefilter = None;
        }
        self
    }

    /// The default for a deployment with no program: its files, then its function if it has one.
    pub fn plain(function: bool) -> Engine {
        let program = Program { fallback: function, ..Program::default() };
        Engine::new(&program).expect("an empty program compiles")
    }

    /// The id of the deployment's table of bulk redirects, if it has one.
    /// A per-file function's own maxDuration, by the function path a decision reached
    /// (`Decision::function`), when the program gives it one.
    pub fn max_duration(&self, function: &str) -> Option<u64> {
        self.max_duration.get(function).copied()
    }

    pub fn bulk(&self) -> Option<&str> {
        self.bulk.as_deref()
    }

    /// The prerender of the function path a decision reached (`Decision::function`), if it's one.
    pub fn prerender(&self, function: &str) -> Option<&std::sync::Arc<super::Prerender>> {
        self.prerenders.get(function)
    }

    /// What answers the request, Routing Middleware treated as saying go on (`step` runs it).
    pub fn route(&self, req: &Request<'_>, files: &dyn Files) -> Decision {
        let mut step = self.step(req, files);
        loop {
            match step {
                Step::Decided(decision) => return decision,
                Step::Middleware { paused, .. } => step = self.resume(req, files, paused, Resume::Next),
                // As a visitor who passed the challenge sees it; a deny ends with the firewall's 403.
                Step::Mitigate { action: super::MitigateAction::Challenge, paused, .. } => step = self.resume(req, files, paused, Resume::Next),
                Step::Mitigate { action: super::MitigateAction::Deny, route, paused } => {
                    let state = paused.state;
                    return Decision { outcome: Outcome::Error, status: Some(403), headers: state.headers, transforms: state.transforms, route: Some(route), function: None, captures: None, looked_up: None };
                }
            }
        }
    }

    /// What answers the request, or the Routing Middleware to run before routing goes on.
    pub fn step(&self, req: &Request<'_>, files: &dyn Files) -> Step {
        if let Some(decision) = guard(req) {
            return Step::Decided(decision);
        }
        let state = State { path: req.path.to_owned(), query: Vec::new(), headers: Vec::new(), status: None, transforms: Vec::new(), captures: None, marks: Vec::new() };
        if acme_challenge(req.path) {
            return Step::Decided(self.serve_or_miss(req, files, state, None));
        }
        let before = state.clone();
        self.walk(req, files, Paused { phase: 0, position: 0, state, before })
    }

    /// Go on from a middleware's route as it said: `req` is the request as it goes on (with the
    /// headers it gave, and a rewrite's query).
    pub fn resume(&self, req: &Request<'_>, files: &dyn Files, mut paused: Paused, resume: Resume) -> Step {
        if let Resume::Rewrite { path } = resume {
            paused.state.path = path;
            // The rewrite's query replaces what routes before it added, as it replaces the client's;
            // the transforms before it apply from the start of what comes after.
            paused.state.query.clear();
            for mark in &mut paused.state.marks {
                *mark = 0;
            }
            // What the phase falls back to if a later route goes nowhere: the rewrite stands.
            paused.before = paused.state.clone();
        }
        self.walk(req, files, paused)
    }

    /// Run the phases from where `at` says.
    fn walk(&self, req: &Request<'_>, files: &dyn Files, at: Paused) -> Step {
        let Paused { phase: first, position, mut state, before: mut paused_before } = at;
        for (i, phase) in PHASES.into_iter().enumerate().skip(first) {
            let routes = if phase.is_none() { &self.main } else { &self.filesystem };
            let (start, before) = if i == first { (position, paused_before.clone()) } else { (0, state.clone()) };
            let found = self.run_from(routes, start, req, &mut state, phase, files);
            if let Found::Middleware { index, function, next } = found {
                paused_before = before.clone();
                return Step::Middleware { function, route: index, paused: Paused { phase: i, position: next, state, before: paused_before } };
            }
            if let Found::Mitigate { index, action, next } = found {
                return Step::Mitigate { action, route: index, paused: Paused { phase: i, position: next, state, before } };
            }
            if let Found::Route { index, url: Some(url), .. } = &found {
                let status = state.status.or(self.status_of(*index));
                if let Some(status) = status.filter(|s| (300..400).contains(s)) {
                    return Step::Decided(self.redirect(req, state, status, url.clone(), Some(*index)));
                }
                return Step::Decided(Decision { outcome: Outcome::External { url: merge_query(url, req.query) }, status, headers: state.headers, transforms: state.transforms, route: Some(*index), function: None, captures: None, looked_up: None });
            }
            let route = match &found {
                Found::Route { index, .. } => Some(*index),
                Found::Nothing | Found::Middleware { .. } | Found::Mitigate { .. } => None,
            };
            let status = route.and_then(|i| self.status_of(i)).or(state.status);
            if let Some(status) = status.filter(|s| (300..400).contains(s))
                && route.is_some()
                && let Some(location) = state.header("location").map(str::to_owned).or_else(|| route.and_then(|i| self.dest_of(i)).map(|_| state.path.clone()))
            {
                return Step::Decided(self.redirect(req, state, status, location, route));
            }
            if let Some(hit) = self.find(&state.path, files) {
                return Step::Decided(self.hit(req, files, state, hit, status, route));
            }
            if let (Some(status), Found::Route { exited: true, .. }) = (status, &found) {
                return Step::Decided(Decision { outcome: Outcome::Error, status: Some(status), headers: state.headers, transforms: state.transforms, route, function: None, captures: None, looked_up: None });
            }
            if let (Some(status), Found::Route { had_dest: false, .. }) = (status, &found) {
                return Step::Decided(Decision { outcome: Outcome::Error, status: Some(status), headers: state.headers, transforms: state.transforms, route, function: None, captures: None, looked_up: None });
            }
            if !self.miss.is_empty() {
                let mut missed = state.clone();
                if let Found::Route { index, .. } = self.run(&self.miss, req, &mut missed, Some(Phase::Miss), files) {
                    let status = self.status_of(index).or(missed.status);
                    if let Some(status) = status.filter(|s| (300..400).contains(s))
                        && let Some(location) = missed.header("location").map(str::to_owned)
                    {
                        return Step::Decided(self.redirect(req, missed, status, location, Some(index)));
                    }
                    if let Some(hit) = self.find(&missed.path, files) {
                        return Step::Decided(self.hit(req, files, missed, hit, status, Some(index)));
                    }
                }
            }
            // A route that went nowhere leaves the next phase where it started.
            if matches!(found, Found::Route { .. }) {
                state = before;
            }
            if phase.is_none() && !self.has_filesystem {
                break;
            }
        }
        Step::Decided(self.serve_or_miss(req, files, state, None))
    }

    /// After the phases: the function if it takes what nothing else does, else the error phase,
    /// else the deployment's `404.html`, else not found.
    fn serve_or_miss(&self, req: &Request<'_>, files: &dyn Files, mut state: State, route: Option<usize>) -> Decision {
        if let Some(hit) = self.find(&state.path, files) {
            let status = state.status;
            return self.hit(req, files, state, hit, status, route);
        }
        if self.fallback {
            return Decision { outcome: Outcome::Function { query: function_query(req, &state) }, status: state.status, headers: state.headers, transforms: without_query(state.transforms), route, function: None, captures: state.captures, looked_up: None };
        }
        if !self.error.is_empty() {
            let mut errored = state.clone();
            if let Found::Route { index, url, had_dest, .. } = self.run(&self.error, req, &mut errored, Some(Phase::Error), files) {
                let status = self.status_of(index).or(errored.status);
                if let Some(url) = url {
                    return Decision { outcome: Outcome::External { url: merge_query(&url, req.query) }, status, headers: errored.headers, transforms: errored.transforms, route: Some(index), function: None, captures: None, looked_up: None };
                }
                if had_dest && let Some(hit) = self.find(&errored.path, files) {
                    return self.hit(req, files, errored, hit, status, Some(index));
                }
                if !had_dest && let Some(status) = status {
                    return Decision { outcome: Outcome::Error, status: Some(status), headers: errored.headers, transforms: errored.transforms, route: Some(index), function: None, captures: None, looked_up: None };
                }
            }
        }
        if files.has("/404.html") {
            state.status = Some(404);
            // `cleanUrls` builds rename the page `404`, the name Vercel serves it by.
            let looked_up = if self.clean_urls { "/404" } else { "/404.html" };
            return Decision { outcome: Outcome::Static { file: "/404.html".into() }, status: Some(404), headers: state.headers, transforms: state.transforms, route, function: None, captures: None, looked_up: Some(looked_up.into()) };
        }
        Decision { outcome: Outcome::NotFound, status: Some(404), headers: state.headers, transforms: state.transforms, route, function: None, captures: None, looked_up: None }
    }

    /// A path the filesystem has: run the `hit` phase's routes for their headers, then serve it.
    fn hit(&self, req: &Request<'_>, files: &dyn Files, mut state: State, hit: Hit, status: Option<u16>, route: Option<usize>) -> Decision {
        // The path that found it, before the `hit` phase's routes see the request.
        let looked_up = matches!(hit, Hit::Static(_)).then(|| state.path.clone());
        if !self.hit.is_empty() {
            self.run(&self.hit, req, &mut state, Some(Phase::Hit), files);
        }
        let (outcome, function) = match hit {
            Hit::Static(file) => (Outcome::Static { file }, None),
            Hit::Function(path) => (Outcome::Function { query: function_query(req, &state) }, Some(path)),
        };
        let (captures, transforms) = if function.is_some() { (state.captures, without_query(state.transforms)) } else { (None, state.transforms) };
        Decision { outcome, status, headers: state.headers, transforms, route, function, captures, looked_up }
    }

    fn redirect(&self, req: &Request<'_>, mut state: State, status: u16, location: String, route: Option<usize>) -> Decision {
        state.headers.retain(|(k, _)| k != "location");
        Decision { outcome: Outcome::Redirect { location: merge_query(&location, req.query) }, status: Some(status), headers: state.headers, transforms: state.transforms, route, function: None, captures: None, looked_up: None }
    }

    /// What to call the route at `index` in the access log: where it came from in `vercel.json`
    /// (`redirects[3]`, `routes[0]`), or its index.
    pub fn rule_name(&self, index: usize) -> String {
        self.all().find(|c| c.index == index).and_then(|c| c.from.clone()).unwrap_or_else(|| format!("route {index}"))
    }

    fn status_of(&self, index: usize) -> Option<u16> {
        self.all().find(|c| c.index == index).and_then(|c| c.status)
    }

    fn dest_of(&self, index: usize) -> Option<&str> {
        self.all().find(|c| c.index == index).and_then(|c| c.dest.as_deref())
    }

    fn all(&self) -> impl Iterator<Item = &Compiled> {
        self.main.routes.iter().chain(&self.filesystem.routes).chain(&self.miss.routes).chain(&self.hit.routes).chain(&self.error.routes)
    }

    /// Run one phase's routes, as Vercel's router does.
    fn run(&self, routes: &PhaseRoutes, req: &Request<'_>, state: &mut State, phase: Option<Phase>, files: &dyn Files) -> Found {
        self.run_from(routes, 0, req, state, phase, files)
    }

    /// `run`, from the route at `start` in the phase (after a middleware's).
    fn run_from(&self, routes: &PhaseRoutes, start: usize, req: &Request<'_>, state: &mut State, phase: Option<Phase>, files: &dyn Files) -> Found {
        // Which routes can match the path, while it stays the same.
        let mut candidates: Option<(String, Vec<bool>)> = None;
        for (position, route) in routes.routes.iter().enumerate().skip(start) {
            if let Some(prefilter) = &routes.prefilter {
                if candidates.as_ref().is_none_or(|(path, _)| *path != state.path) {
                    candidates = Some((state.path.clone(), prefilter.candidates(&state.path, routes.routes.len())));
                }
                if candidates.as_ref().is_some_and(|(_, c)| !c[position]) {
                    continue;
                }
            }
            if let Some(methods) = &route.methods
                && !methods.iter().any(|m| m.eq_ignore_ascii_case(req.method))
            {
                continue;
            }
            let Some((groups, names)) = route.re.captures(&state.path).or_else(|| state.path.get(1..).and_then(|p| route.re.captures(p))) else {
                continue;
            };
            let mut params: Vec<(String, String)> = Vec::new();
            if !route.has.iter().all(|c| c.holds(req, &mut params)) {
                continue;
            }
            if route.missing.iter().any(|c| c.holds(req, &mut Vec::new())) {
                continue;
            }
            // `$wildcard`: the request's domain's value, under any `has` capture of that name.
            if !self.wildcard.is_empty() {
                let host = req.host.trim_end_matches('.').to_ascii_lowercase();
                params.insert(0, ("wildcard".to_owned(), self.wildcard.get(&host).cloned().unwrap_or_default()));
            }
            let resolve = |template: &str| resolve(template, &groups, &names, &params);
            let dest = route.dest.as_deref().map(resolve);
            for (key, value) in &route.headers {
                if matches!(phase, Some(Phase::Hit | Phase::Miss)) && state.header(key).is_some() {
                    continue;
                }
                state.set_header(key.clone(), resolve(value));
            }
            for t in &route.transforms {
                state.transforms.push(Transform { args: t.args.iter().map(|a| resolve(a)).collect(), ..t.clone() });
                state.marks.push(state.query.len());
            }
            if let Some(function) = &route.middleware {
                return Found::Middleware { index: route.index, function: function.clone(), next: position + 1 };
            }
            if let Some(action) = route.mitigate {
                return Found::Mitigate { index: route.index, action, next: position + 1 };
            }
            if route.cont {
                if let Some(status) = route.status {
                    state.status = Some(status);
                }
                if let Some(dest) = &dest
                    && !is_url(dest)
                {
                    state.go_to(dest);
                    state.captures = Some(super::prerender::captures(&groups, &names));
                }
                continue;
            }
            if let Some(dest) = &dest
                && is_url(dest)
            {
                return Found::Route { index: route.index, url: Some(dest.clone()), had_dest: true, exited: false };
            }
            if route.check
                && phase != Some(Phase::Hit)
                && let Some(dest) = &dest
            {
                let mut next = state.clone();
                next.go_to(dest);
                next.captures = Some(super::prerender::captures(&groups, &names));
                if self.find(&next.path, files).is_none() {
                    if route.status.is_some() && phase != Some(Phase::Miss) {
                        // Ends here with its status, as Vercel's `exitWithStatus`.
                        state.go_to(dest);
                        state.captures = next.captures.take();
                        return Found::Route { index: route.index, url: None, had_dest: true, exited: true };
                    } else if !self.miss.is_empty() && phase != Some(Phase::Miss) {
                        let found = self.run(&self.miss, req, &mut next, Some(Phase::Miss), files);
                        *state = next;
                        if let Found::Route { .. } = found {
                            return found;
                        }
                        continue;
                    } else {
                        if let Some(status) = route.status.filter(|_| phase == Some(Phase::Miss)) {
                            next.status = Some(status);
                        }
                        *state = next;
                        continue;
                    }
                }
            }
            if let Some(dest) = &dest {
                state.go_to(dest);
                state.captures = Some(super::prerender::captures(&groups, &names));
            }
            return Found::Route { index: route.index, url: None, had_dest: dest.is_some(), exited: false };
        }
        Found::Nothing
    }

    /// What serves `path`: the function, or a static file by Vercel's rules for folders and
    /// (with `cleanUrls`) `.html` files.
    fn find(&self, path: &str, files: &dyn Files) -> Option<Hit> {
        if let Some(f) = self.function.iter().find(|f| *f == path) {
            return Some(Hit::Function(f.clone()));
        }
        let decoded = percent_decode(path);
        // An encoded `/` is part of a segment, which no file or function's path has: decoded, it
        // would reach `/admin/x.html` past routes written for `/admin/`.
        if decoded != path && path.to_ascii_lowercase().contains("%2f") {
            return None;
        }
        if decoded != path
            && let Some(f) = self.function.iter().find(|f| **f == decoded)
        {
            return Some(Hit::Function(f.clone()));
        }
        static_file(&decoded, files, self.clean_urls, self.trailing_slash).map(Hit::Static)
    }
}

/// The static file for a decoded path: the path itself, a folder's `index.html` (with or without
/// the trailing slash), or with `cleanUrls` the path's `.html` file. With `cleanUrls` an `.html`
/// file has only its clean name, as on Vercel, whose build renames them: a destination naming
/// `about.html` finds nothing.
pub fn static_file(path: &str, files: &dyn Files, clean_urls: bool, trailing_slash: Option<bool>) -> Option<String> {
    let mut candidates: Vec<String> = Vec::with_capacity(4);
    if !(clean_urls && path.ends_with(".html")) {
        candidates.push(path.to_owned());
    }
    if let Some(dir) = path.strip_suffix('/') {
        candidates.push(format!("{path}index.html"));
        if clean_urls && !dir.is_empty() {
            candidates.push(format!("{dir}.html"));
        }
        // A file found without the slash, as Vercel's filesystem does unless `trailingSlash: false`
        // (whose redirect comes first anyway): seen on Vercel, `/photo.png/` serves `/photo.png`.
        if trailing_slash != Some(false) && !dir.is_empty() {
            candidates.push(dir.to_owned());
        }
    } else {
        candidates.push(format!("{path}/index.html"));
        if clean_urls {
            candidates.push(format!("{path}.html"));
        }
    }
    candidates.into_iter().find(|c| files.has(c))
}

fn is_url(dest: &str) -> bool {
    let lower = dest.get(..8).unwrap_or(dest).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// `$1`, `$name`: a capture of the route's pattern by number or name, or a parameter a `has`
/// condition captured; nothing if there's no such capture, as on Vercel. A reference is read as
/// `vercel dev`'s router reads it (`$` and `[1-9a-zA-Z]+`), except that a group's whole name is
/// taken when it has `_` or other digits in it: Nitro's ISR routes send `$__isr_route`, which
/// Vercel fills in (seen: the function gets the path, not the reference).
pub(super) fn resolve(template: &str, groups: &[Option<String>], names: &[Option<String>], params: &[(String, String)]) -> String {
    if !template.contains('$') {
        return template.to_owned();
    }
    let known = |name: &str| names.iter().any(|n| n.as_deref() == Some(name)) || params.iter().any(|(k, _)| k == name);
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let word = if after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            after.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count()
        } else {
            0
        };
        let len = if word > 0 && known(&after[..word]) {
            word
        } else {
            after.bytes().take_while(|b| (b'1'..=b'9').contains(b) || b.is_ascii_alphabetic()).count()
        };
        if len == 0 {
            out.push('$');
            rest = after;
            continue;
        }
        let param = &after[..len];
        let value = if let Some(i) = names.iter().position(|n| n.as_deref() == Some(param)) {
            groups.get(i).cloned().flatten()
        } else if let Some((_, v)) = params.iter().rev().find(|(k, _)| k == param) {
            Some(v.clone())
        } else {
            let digits: String = param.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<usize>().ok().and_then(|i| groups.get(i).cloned().flatten())
        };
        out.push_str(&value.unwrap_or_default());
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// `url` with the client's query under its own, as Vercel builds a redirect's `Location` or an
/// external rewrite's URL: the client's parameters, with those `url` has set over them.
pub fn merge_query(url: &str, client: Option<&str>) -> String {
    let (base, hash) = match url.find('#') {
        Some(at) => (&url[..at], &url[at..]),
        None => (url, ""),
    };
    let (path, own) = match base.split_once('?') {
        Some((path, own)) => (path, Some(own)),
        None => (base, None),
    };
    if own.is_none() && client.is_none_or(str::is_empty) {
        return url.to_owned();
    }
    let mut query = Query::parse(client.unwrap_or(""));
    query.overlay(Query::parse(own.unwrap_or("")));
    if query.is_empty() {
        return format!("{path}{hash}");
    }
    format!("{path}?{}{hash}", query.encode())
}

/// The query string as Vercel passes it on: every one is rebuilt (`Query`), whatever the routes
/// did. `None` when that's what the client sent already.
pub fn normalize_query(raw: &str) -> Option<String> {
    let encoded = Query::parse(raw).encode();
    (encoded != raw).then_some(encoded)
}

/// The query string the function gets: the client's with the destinations' parameters set over
/// it, and the routes' `request.query` transforms applied each where its route matched, rebuilt as
/// Vercel does. `None` when that's the client's as it came.
fn function_query(req: &Request<'_>, state: &State) -> Option<String> {
    let raw = req.query.unwrap_or("");
    let transforms = state.transforms.iter().any(|t| t.kind == super::TransformKind::RequestQuery);
    if state.query.is_empty() && !transforms {
        return normalize_query(raw);
    }
    let added = |from: usize, to: usize| {
        let mut added = Query::default();
        for (k, v) in &state.query[from..to] {
            added.0.push((k.as_bytes().to_vec(), Some(v.as_bytes().to_vec())));
        }
        added
    };
    let mut query = Query::parse(raw);
    let mut applied = 0;
    for (t, &mark) in state.transforms.iter().zip(&state.marks) {
        if t.kind != super::TransformKind::RequestQuery {
            continue;
        }
        let mark = mark.min(state.query.len());
        if mark > applied {
            query.overlay(added(applied, mark));
            applied = mark;
        }
        super::transform::apply_one(t, &mut query);
    }
    if applied < state.query.len() {
        query.overlay(added(applied, state.query.len()));
    }
    Some(query.encode())
}

/// A function's decision carries its transforms without the `request.query` ones: they're in its
/// query already (`function_query`).
fn without_query(transforms: Vec<Transform>) -> Vec<Transform> {
    transforms.into_iter().filter(|t| t.kind != super::TransformKind::RequestQuery).collect()
}

/// A query string the way Vercel's proxy rebuilds every one: its pairs in order, each key and
/// value percent-decoded to bytes (`+` is a space; a `%` that doesn't start an escape is dropped)
/// and written back escaped as `encodeURIComponent` does, byte by byte; a key sent without `=`
/// stays without one.
#[derive(Default)]
pub(super) struct Query(pub(super) Vec<(Vec<u8>, Option<Vec<u8>>)>);

impl Query {
    pub(super) fn parse(text: &str) -> Query {
        let pairs = text
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|piece| match piece.split_once('=') {
                Some((k, v)) => (unescape_bytes(k), Some(unescape_bytes(v))),
                None => (unescape_bytes(piece), None),
            })
            .collect();
        Query(pairs)
    }

    /// `other`'s keys set over `self`'s: each takes the place of the first pair with its key
    /// (the others with that key go), or goes last if `self` has none.
    fn overlay(&mut self, other: Query) {
        let mut keys: Vec<&Vec<u8>> = Vec::new();
        for (k, _) in &other.0 {
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
        for key in keys {
            let values: Vec<(Vec<u8>, Option<Vec<u8>>)> = other.0.iter().filter(|(k, _)| k == key).cloned().collect();
            match self.0.iter().position(|(k, _)| k == key) {
                Some(at) => {
                    let mut i = 0;
                    self.0.retain(|(k, _)| {
                        i += 1;
                        i - 1 < at || k != key
                    });
                    self.0.splice(at..at, values);
                }
                None => self.0.extend(values),
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn encode(&self) -> String {
        let mut out = String::new();
        for (i, (key, value)) in self.0.iter().enumerate() {
            if i > 0 {
                out.push('&');
            }
            escape_bytes(key, &mut out);
            if let Some(value) = value {
                out.push('=');
                escape_bytes(value, &mut out);
            }
        }
        out
    }
}

fn unescape_bytes(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                if let (Some(hi), Some(lo)) = (bytes.get(i + 1).and_then(|b| hex_value(*b)), bytes.get(i + 2).and_then(|b| hex_value(*b))) {
                    out.push(hi << 4 | lo);
                    i += 2;
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    out
}

fn escape_bytes(bytes: &[u8], out: &mut String) {
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// What Vercel answers before any route runs: a path with repeated slashes goes, with a 308, to
/// the same path with single ones; an encoded `..` segment is refused, with 400 when it would climb
/// above the site's root and 403 otherwise. (A plain `..` matches nothing and goes on to a 404.)
fn guard(req: &Request<'_>) -> Option<Decision> {
    let path = req.path;
    if path.contains("//") {
        let mut collapsed = String::with_capacity(path.len());
        for c in path.chars() {
            if !(c == '/' && collapsed.ends_with('/')) {
                collapsed.push(c);
            }
        }
        let location = merge_query(&collapsed, req.query);
        return Some(Decision { outcome: Outcome::Redirect { location }, status: Some(308), headers: Vec::new(), route: None, transforms: Vec::new(), function: None, captures: None, looked_up: None });
    }
    if path.contains('%') {
        let mut depth: i32 = 0;
        let mut climbed = false;
        for segment in path.split('/').skip(1) {
            if segment.contains('%') && percent_decode(segment) == ".." {
                climbed = true;
                depth -= 1;
                if depth < 0 {
                    return Some(Decision { outcome: Outcome::Error, status: Some(400), headers: Vec::new(), route: None, transforms: Vec::new(), function: None, captures: None, looked_up: None });
                }
            } else if !segment.is_empty() {
                depth += 1;
            }
        }
        if climbed {
            return Some(Decision { outcome: Outcome::Error, status: Some(403), headers: Vec::new(), route: None, transforms: Vec::new(), function: None, captures: None, looked_up: None });
        }
    }
    None
}

#[allow(dead_code)]
fn engines_are_shared_across_threads() {
    fn check<T: Send + Sync>() {}
    check::<Engine>();
}
