//! Vercel's Build Output API, version 3: the `.vercel/output` folder a framework's Vercel adapter
//! writes (SvelteKit's `adapter-vercel`, Astro's `@astrojs/vercel`, Nitro's `vercel` preset, ...),
//! read into typed parts for the importer. Read as Vercel documents it (vercel.com/docs/build-output-api,
//! 2026-11-16) and as `vercel build` 65.16.0 writes it (docs/c16-frameworks.md):
//!
//! - `config.json`: `version` 3, `routes`, `images`, `wildcard`, `overrides`, `cache`, `framework`,
//!   `crons`, `services`, `daemons`, `deploymentId`. Anything else is kept in `other` for the
//!   importer to warn about.
//! - `static/`: files served at their path.
//! - `functions/<path>.func/`: a function served at `/<path>`, with its `.vc-config.json`. A `.func`
//!   may be a symlink to another, a second name for the same function (SvelteKit names one per
//!   route); the link must stay inside `functions/`.
//! - `functions/<path>.prerender-config.json` (and its fallback file): the function at `<path>` is
//!   cached as a page (ISR).
//!
//! Nothing here decides what forkrail supports: the importer does, and refuses by name what it
//! can't run yet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

/// Where a build leaves its Build Output, relative to the project.
pub const DIR: &str = ".vercel/output";
/// A function folder's suffix, which isn't part of its path.
const FUNC: &str = ".func";
const PRERENDER_CONFIG: &str = ".prerender-config.json";
/// More function names than this is a broken output, not a site (Vercel's own limit on functions
/// in a deployment is far lower).
const MAX_FUNCTIONS: usize = 50_000;

/// A Build Output, read.
#[derive(Debug)]
pub struct Output {
    /// The `.vercel/output` folder.
    pub root: PathBuf,
    pub config: Config,
    /// Every function name, sorted by path: real folders and symlinks to them.
    pub functions: Vec<Function>,
    /// Every prerender, sorted by path.
    pub prerenders: Vec<Prerender>,
}

/// `config.json`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub version: u32,
    /// Routes in `vercel.json`'s `routes` form, kept as they are for the rules parser.
    #[serde(default)]
    pub routes: Option<Vec<Value>>,
    /// Image optimization's settings, as they are (F5 reads them).
    #[serde(default)]
    pub images: Option<Value>,
    #[serde(default)]
    pub wildcard: Vec<Wildcard>,
    /// A static file served at another path, or with another type, by its path in `static/`.
    #[serde(default)]
    pub overrides: BTreeMap<String, Override>,
    /// Globs of files to keep between builds.
    #[serde(default)]
    pub cache: Vec<String>,
    #[serde(default)]
    pub framework: Option<FrameworkInfo>,
    #[serde(default)]
    pub crons: Vec<Cron>,
    /// Vercel Schedules, by name: `{schedule, path | function | topic, ...}` (Nitro 3 writes them
    /// with `NITRO_VERCEL_SCHEDULES`).
    #[serde(default)]
    pub schedules: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    pub services: Option<Vec<Value>>,
    #[serde(default)]
    pub daemons: Option<BTreeMap<String, Value>>,
    /// For skew protection, when the build chose its own id.
    #[serde(default)]
    pub deployment_id: Option<String>,
    /// Anything Vercel's documentation doesn't name.
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Wildcard {
    pub domain: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Override {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct FrameworkInfo {
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Cron {
    pub path: String,
    pub schedule: String,
}

/// A function's name in the output: its path, and the folder it is or links to.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// `/api/hello` for `functions/api/hello.func`, `/![-]/catchall`, `/isr/[slug]`.
    pub path: String,
    /// The real function's path when this one is a symlink to it; `None` for a real folder.
    pub alias_of: Option<String>,
    /// The real function's folder.
    pub dir: PathBuf,
    /// The real function's `.vc-config.json`.
    pub config: FunctionConfig,
}

/// What runs a function, from its `runtime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `nodejs20.x`, `nodejs24.x`, ...
    Node,
    /// `edge`: Vercel's Edge runtime.
    Edge,
    /// `python3.12`, ...
    Python,
    /// Anything else (`bun1.x`, `go1.x`, a custom runtime): its name.
    Other,
}

/// A function's `.vc-config.json`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionConfig {
    pub runtime: String,
    /// The file a serverless function runs, relative to its folder.
    #[serde(default)]
    pub handler: Option<String>,
    /// The file an Edge function runs, relative to its folder.
    #[serde(default)]
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub memory: Option<u32>,
    #[serde(default)]
    pub max_duration: Option<MaxDuration>,
    /// Env vars this function has besides the project's.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Vercel's regions (a list, one name, or `all`): forkrail runs a project in its own regions.
    #[serde(default)]
    pub regions: Option<Value>,
    /// `x86_64` or `arm64`: forkrail builds and runs arm64.
    #[serde(default)]
    pub architecture: Option<String>,
    /// `Nodejs` for a Node function.
    #[serde(default)]
    pub launcher_type: Option<String>,
    /// Vercel's request and response helpers (`req.query`, `res.json`, ...) for a Node handler.
    #[serde(default)]
    pub should_add_helpers: bool,
    #[serde(default)]
    pub should_add_sourcemap_support: bool,
    #[serde(default)]
    pub supports_response_streaming: Option<bool>,
    /// The older name of `supportsResponseStreaming`, which SvelteKit's adapter still writes.
    #[serde(default)]
    pub experimental_response_streaming: Option<bool>,
    /// The handler written for AWS Lambda's own Node signature (`index.handler`).
    #[serde(default)]
    pub aws_lambda_handler: Option<String>,
    #[serde(default)]
    pub supports_wrapper: Option<bool>,
    /// An Edge function's: ignored by Vercel too.
    #[serde(default)]
    pub env_vars_in_use: Vec<String>,
    #[serde(default)]
    pub framework: Option<FrameworkInfo>,
    /// Next.js's map of files kept outside the function's own folder.
    #[serde(default)]
    pub file_path_map: Option<BTreeMap<String, String>>,
    /// Anything Vercel's documentation doesn't name.
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

/// `maxDuration`: seconds, or `"max"` for the plan's longest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum MaxDuration {
    Seconds(u64),
    Max(MaxWord),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum MaxWord {
    #[serde(rename = "max")]
    Max,
}

impl FunctionConfig {
    pub fn kind(&self) -> Kind {
        let runtime = self.runtime.as_str();
        if runtime.starts_with("nodejs") {
            Kind::Node
        } else if runtime == "edge" {
            Kind::Edge
        } else if runtime.starts_with("python") {
            Kind::Python
        } else {
            Kind::Other
        }
    }

    /// The file it runs, relative to its folder: `handler`, or an Edge function's `entrypoint`.
    pub fn entry(&self) -> Option<&str> {
        self.handler.as_deref().or(self.entrypoint.as_deref())
    }

    /// Whether it streams its responses (either of the two names says so).
    pub fn streams(&self) -> bool {
        self.supports_response_streaming.or(self.experimental_response_streaming).unwrap_or(false)
    }
}

/// A prerender: the function at `path` cached as a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Prerender {
    pub path: String,
    pub config: PrerenderConfig,
    /// The fallback file the config names, when it's there.
    pub fallback: Option<PathBuf>,
}

/// `<path>.prerender-config.json`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrerenderConfig {
    /// Seconds before the cached page is made again; `false` never.
    #[serde(default)]
    pub expiration: Expiration,
    /// Prerenders with the same group are made again together.
    #[serde(default)]
    pub group: Option<u64>,
    /// The `__prerender_bypass` cookie's and `x-prerender-revalidate` header's value.
    #[serde(default)]
    pub bypass_token: Option<String>,
    /// The fallback file, relative to the config file.
    #[serde(default)]
    pub fallback: Option<String>,
    /// The query parameters cached apart; `None` every one, empty none.
    #[serde(default)]
    pub allow_query: Option<Vec<String>>,
    /// The function gets the query string (`allowQuery` still filters it).
    #[serde(default)]
    pub pass_query: bool,
    #[serde(default)]
    pub initial_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub initial_status: Option<u16>,
    #[serde(default)]
    pub expose_err_body: bool,
    #[serde(flatten)]
    pub other: BTreeMap<String, Value>,
}

/// A prerender's `expiration`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Expiration {
    Seconds(u64),
    /// `false`: never made again by time (only on demand).
    #[default]
    Never,
}

impl<'de> Deserialize<'de> for Expiration {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Bool(false) => Ok(Expiration::Never),
            Value::Number(n) if n.as_u64().is_some() => Ok(Expiration::Seconds(n.as_u64().unwrap_or_default())),
            other => Err(serde::de::Error::custom(format!("expiration must be a number of seconds or false, not {other}"))),
        }
    }
}

impl Output {
    /// The functions that are folders of their own (one per real function), sorted by path.
    pub fn real_functions(&self) -> impl Iterator<Item = &Function> {
        self.functions.iter().filter(|f| f.alias_of.is_none())
    }

    pub fn static_dir(&self) -> PathBuf {
        self.root.join("static")
    }
}

/// Whether the project's build left a Build Output.
pub fn exists(project: &Path) -> bool {
    project.join(DIR).join("config.json").is_file()
}

/// Whether a Build Output was written since `when` (by the build that just ran, not left from an
/// earlier `vercel build`).
pub fn written_since(project: &Path, when: std::time::SystemTime) -> bool {
    std::fs::metadata(project.join(DIR).join("config.json")).and_then(|m| m.modified()).is_ok_and(|t| t >= when)
}

/// Read the Build Output at `root` (a `.vercel/output` folder).
pub fn read(root: &Path) -> Result<Output> {
    let config_path = root.join("config.json");
    let text = std::fs::read_to_string(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let config: Config = serde_json::from_str(&text).with_context(|| format!("{} isn't a Build Output configuration", config_path.display()))?;
    if config.version != 3 {
        bail!("{} is version {} of Vercel's Build Output API; forkrail reads version 3", config_path.display(), config.version);
    }
    let functions_dir = root.join("functions");
    let mut found = Found::default();
    if functions_dir.is_dir() {
        let base = functions_dir.canonicalize().with_context(|| format!("resolving {}", functions_dir.display()))?;
        walk(&functions_dir, &base, "", &mut found)?;
    }
    let mut functions = Vec::with_capacity(found.funcs.len());
    for (path, entry) in &found.funcs {
        let (dir, alias_of) = match entry {
            FuncEntry::Real(dir) => (dir.clone(), None),
            FuncEntry::Link(target) => {
                let Some(FuncEntry::Real(dir)) = found.funcs.get(target) else {
                    bail!("functions{path}.func links to functions{target}.func, which isn't a function folder");
                };
                (dir.clone(), Some(target.clone()))
            }
        };
        let config = read_function_config(&dir, path)?;
        functions.push(Function { path: path.clone(), alias_of, dir, config });
    }
    let mut prerenders = Vec::with_capacity(found.prerenders.len());
    for (path, file) in &found.prerenders {
        let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let config: PrerenderConfig = serde_json::from_str(&text).with_context(|| format!("{} isn't a prerender configuration", file.display()))?;
        if !found.funcs.contains_key(path) {
            bail!("{} names no function: there's no functions{path}.func beside it", file.display());
        }
        let fallback = match &config.fallback {
            Some(name) => {
                let at = file.parent().unwrap_or(root).join(name);
                if !inside(&at, &functions_dir) {
                    bail!("{}: its fallback {name:?} is outside the functions folder", file.display());
                }
                at.is_file().then_some(at)
            }
            None => None,
        };
        prerenders.push(Prerender { path: path.clone(), config, fallback });
    }
    Ok(Output { root: root.to_path_buf(), config, functions, prerenders })
}

enum FuncEntry {
    Real(PathBuf),
    /// A symlink to the function at this path.
    Link(String),
}

#[derive(Default)]
struct Found {
    funcs: BTreeMap<String, FuncEntry>,
    prerenders: BTreeMap<String, PathBuf>,
}

/// Find every `.func` folder (or link to one) and prerender config under `dir`, whose path below
/// `functions/` is `prefix`. A `.func` folder's own files aren't looked into.
fn walk(dir: &Path, base: &Path, prefix: &str, found: &mut Found) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if let Some(stem) = name.strip_suffix(FUNC) {
            if stem.is_empty() {
                bail!("{} has no name before `.func`", path.display());
            }
            let fn_path = format!("{prefix}/{stem}");
            if found.funcs.len() >= MAX_FUNCTIONS {
                bail!("the Build Output has more than {MAX_FUNCTIONS} functions");
            }
            let entry = if meta.file_type().is_symlink() {
                FuncEntry::Link(link_target(&path, base)?)
            } else if meta.is_dir() {
                FuncEntry::Real(path.clone())
            } else {
                bail!("{} is a file; a function is a folder", path.display());
            };
            found.funcs.insert(fn_path, entry);
            continue;
        }
        if let Some(stem) = name.strip_suffix(PRERENDER_CONFIG) {
            found.prerenders.insert(format!("{prefix}/{stem}"), path.clone());
            continue;
        }
        // Folders are path segments; links to folders other than `.func` ones aren't followed.
        if meta.is_dir() {
            walk(&path, base, &format!("{prefix}/{name}"), found)?;
        }
    }
    Ok(())
}

/// The function path a `.func` symlink points at, which must be another `.func` inside
/// `functions/` (canonical `base`).
fn link_target(link: &Path, base: &Path) -> Result<String> {
    let target = std::fs::read_link(link).with_context(|| format!("reading the link {}", link.display()))?;
    let resolved = link.parent().unwrap_or(base).join(&target);
    let real = resolved.canonicalize().with_context(|| format!("{} links to {}, which isn't there", link.display(), target.display()))?;
    let Ok(rel) = real.strip_prefix(base) else {
        bail!("{} links outside the functions folder", link.display());
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    let Some(stem) = rel.strip_suffix(FUNC) else {
        bail!("{} links to {}, which isn't a `.func` folder", link.display(), target.display());
    };
    Ok(format!("/{stem}"))
}

fn read_function_config(dir: &Path, path: &str) -> Result<FunctionConfig> {
    let file = dir.join(".vc-config.json");
    let text = std::fs::read_to_string(&file).with_context(|| format!("functions{path}.func has no .vc-config.json"))?;
    let config: FunctionConfig = serde_json::from_str(&text).with_context(|| format!("functions{path}.func/.vc-config.json isn't a function configuration"))?;
    let Some(entry) = config.entry() else {
        bail!("functions{path}.func/.vc-config.json names no `handler` (or `entrypoint`)");
    };
    let entry_path = dir.join(entry);
    if !inside(&entry_path, dir) {
        bail!("functions{path}.func: its entry {entry:?} is outside the function's folder");
    }
    // Vercel's builders may leave a function's files where they are and list them in `filePathMap`
    // (paths relative to the project) instead of copying them into its folder.
    let mapped = config.file_path_map.as_ref().is_some_and(|m| m.contains_key(entry));
    if !entry_path.is_file() && !mapped && config.aws_lambda_handler.is_none() {
        bail!("functions{path}.func: its entry {entry} isn't there");
    }
    Ok(config)
}

/// Whether `path` stays inside `dir` by its components (no `..` climbing out, no absolute path).
fn inside(path: &Path, dir: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(dir) else { return false };
    let mut depth = 0i32;
    for part in rel.components() {
        match part {
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

// ---- The import: a Build Output as forkrail's build ----

/// Where the functions go in the package's `app/`, and the dispatcher that serves them (the
/// runtime passes the router's function header to this entry: crates/runtime/src/app.rs).
pub const FUNCTIONS_DIR: &str = ".forkrail/fn";
pub const DISPATCHER: &str = ".forkrail/functions.mjs";
/// Where functions whose files all stay in the project (`filePathMap`, as `vercel build` leaves
/// React Router's and Remix's) keep them, laid out as the project: one copy of a file serves every
/// such function whose map gives it the same source.
pub const PROJECT_DIR: &str = ".forkrail/project";
/// Vercel's `"maxDuration": "max"` (the plan's longest): forkrail's, as `vercel.json`'s `max`.
const MAX_DURATION_S: u64 = 900;
/// Lambda's limit on a package, unzipped, less room for the runtime.
const MAX_PACKAGE_BYTES: u64 = 240 * 1024 * 1024;

/// What a Build Output becomes in forkrail's build.
#[derive(Debug)]
pub struct Imported {
    /// The routes, merged with the project's `vercel.json`, every function path in the filesystem.
    pub program: forkrail_common::rules::Program,
    /// The functions' files, by their path in the package's `app/` (under `FUNCTIONS_DIR`).
    pub files: Vec<crate::package::SourceFile>,
    /// `DISPATCHER`'s source.
    pub dispatcher: String,
    /// The most memory and the longest maxDuration any function asks for: one Lambda runs them all.
    pub floor: Option<crate::state::Floor>,
    /// `config.json`'s cron jobs.
    pub crons: Vec<Cron>,
    /// `config.json`'s schedules (path targets only: the import refuses the others).
    pub schedules: Option<serde_json::Map<String, Value>>,
    /// The framework, as the output names it (`sveltekit`), for the record.
    pub framework: Option<String>,
    /// Image Optimization's settings (`config.json`'s `images`), as written.
    pub images: Option<Value>,
    /// The static files' folder, and how `overrides` renames or retypes them.
    pub static_dir: PathBuf,
    pub overrides: BTreeMap<String, Override>,
    /// Prerenders' fallback pages, by their key in the static manifest
    /// (`forkrail_common::statics::fallback_key`): uploaded with the static files, served by the
    /// router while a page has never been made.
    pub fallbacks: BTreeMap<String, PathBuf>,
    /// The real functions' paths, for the record.
    pub functions: Vec<String>,
    /// `config.json`'s `deploymentId`: the name the framework gave the deployment, which its pages
    /// put in `?dpl=` and `x-deployment-id` (skew protection).
    pub deployment_id: Option<String>,
    /// Its functions, prerenders and middleware, for the record (`state::Outline`).
    pub outline: crate::state::Outline,
    /// Things forkrail does differently, worth telling the user.
    pub warnings: Vec<String>,
}

/// The paths a function answers at in Vercel's filesystem: its own, with a trailing slash too
/// (SvelteKit's adapter relies on it), and an `index` function also its folder's (`/index` is
/// `/`, as `index.html` is), with and without the slash.
pub fn paths_of(path: &str) -> Vec<String> {
    let mut out = vec![path.to_owned(), format!("{path}/")];
    if let Some(dir) = path.strip_suffix("/index") {
        if dir.is_empty() {
            out.push("/".to_owned());
        } else {
            out.push(dir.to_owned());
            out.push(format!("{dir}/"));
        }
    }
    out
}

/// Turn a read Build Output into forkrail's build: its program (with `vercel_json`'s routing keys
/// merged in, as `vercel build` does), its functions packaged for one Lambda with a dispatcher,
/// its static files. `framework` names it in errors and logs. What forkrail can't run yet is
/// refused by name, never deployed half-working.
pub fn import(project: &Path, output: Output, vercel_json: &serde_json::Map<String, Value>, framework: &str) -> Result<Imported> {
    let mut warnings = Vec::new();
    let config = &output.config;
    if config.services.as_ref().is_some_and(|s| !s.is_empty()) {
        bail!("{framework}'s Build Output has services (several apps in one deployment), which forkrail doesn't run yet");
    }
    if config.daemons.as_ref().is_some_and(|d| !d.is_empty()) {
        bail!("{framework}'s Build Output has daemons, which forkrail doesn't run yet");
    }
    if let Some(name) = &config.deployment_id
        && !forkrail_common::routes::valid_skew_id(name)
    {
        bail!("{framework}'s Build Output names its deployment {name:?} (`deploymentId`): use 1-64 of A-Z, a-z, 0-9, - and _");
    }
    // Vercel Schedules sent to a function by name (Nitro's `NITRO_VERCEL_SCHEDULES=private`) or to
    // a queue: forkrail's schedules call a path on the deployment.
    for (name, entry) in config.schedules.iter().flatten() {
        if let Some(target) = ["function", "topic", "service"].into_iter().find(|t| entry.get(*t).is_some()) {
            bail!(
                "{framework}'s Build Output has a schedule ({name}) sent to a `{target}`, which forkrail doesn't run yet: its schedules call a path. For Nitro, leave NITRO_VERCEL_SCHEDULES unset (its tasks then run as cron jobs, through /_vercel/cron)"
            );
        }
    }
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    // Functions a route runs as Routing Middleware (`middlewarePath`): not paths anyone can ask for
    // (seen on Vercel: a middleware function's own path is a 404), only the router calls them.
    let middleware = middleware_functions(config.routes.as_deref().unwrap_or_default());
    for name in &middleware {
        if !output.functions.iter().any(|f| f.path == *name) {
            bail!("{framework}'s routes run {name} as middleware, but the Build Output has no functions{name}.func");
        }
    }
    for f in output.real_functions() {
        match f.config.kind() {
            Kind::Node => {}
            // Run on Node, as Vercel now runs its own middleware: the shim gives them the Edge
            // runtime's shape (shim.mjs, "Edge functions and Routing Middleware").
            Kind::Edge => {
                if f.config.other.get("assets").is_some_and(|a| a.as_array().is_some_and(|a| !a.is_empty())) {
                    warnings.push(format!("{}: its Edge `assets` aren't served to `fetch` as on Vercel's Edge runtime", f.path));
                }
            }
            Kind::Python | Kind::Other => bail!("{framework}'s function {} runs on `{}`; forkrail runs a Build Output's Node and Edge functions", f.path, f.config.runtime),
        }
        if f.config.aws_lambda_handler.is_some() {
            bail!("{framework}'s function {} is written for AWS Lambda's own handler signature (`awsLambdaHandler`), which forkrail doesn't run yet", f.path);
        }
        // Functions Vercel runs on an event (Vercel Queues, Vercel Schedules), not by a request.
        if f.config.other.get("experimentalTriggers").is_some_and(|t| t.as_array().is_none_or(|t| !t.is_empty())) {
            bail!("{framework}'s function {} runs on Vercel's triggers (`experimentalTriggers`: Vercel Queues or Schedules), which forkrail doesn't run yet", f.path);
        }
        if f.config.should_add_sourcemap_support {
            warnings.push(format!("{}: source maps in stack traces aren't added", f.path));
        }
        // One process runs every function: their own env vars must agree.
        for (key, value) in &f.config.environment {
            match env.get(key) {
                Some(other) if other != value => bail!("{framework}'s functions give {key} different values; forkrail runs them in one process"),
                _ => {
                    env.insert(key.clone(), value.clone());
                }
            }
        }
    }

    // Every name a function answers at, and each one's limit.
    let mut paths: Vec<String> = Vec::new();
    let mut max_duration: BTreeMap<String, u64> = BTreeMap::new();
    let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &output.functions {
        let real = f.alias_of.clone().unwrap_or_else(|| f.path.clone());
        let limit = f.config.max_duration.map(|d| match d {
            MaxDuration::Seconds(s) => s.clamp(1, MAX_DURATION_S),
            MaxDuration::Max(_) => MAX_DURATION_S,
        });
        if middleware.contains(&f.path) {
            // The router calls it by its name (the dispatcher knows it by that), with its own
            // limit; the routes never reach it.
            if let Some(limit) = limit {
                max_duration.insert(f.path.clone(), limit);
            }
            names.entry(real.clone()).or_default().push(f.path.clone());
            continue;
        }
        for p in paths_of(&f.path) {
            if paths.contains(&p) {
                continue;
            }
            if let Some(limit) = limit {
                max_duration.insert(p.clone(), limit);
            }
            names.entry(real.clone()).or_default().push(p.clone());
            paths.push(p);
        }
    }
    let routes = Value::Array(config.routes.clone().unwrap_or_default());
    let wildcard = config.wildcard.iter().map(|w| (w.domain.clone(), w.value.clone())).collect();
    let mut program = forkrail_common::rules::Program::from_build_output(vercel_json, framework, &routes, paths, max_duration, wildcard).map_err(|e| anyhow::anyhow!("{framework}'s routes: {e}"))?;
    let (prerenders, fallbacks) = prerenders(&output.prerenders, &mut warnings);
    program.prerenders = prerenders;

    // The functions' files, each real function once, under its own folder; or, when all its
    // files stay in the project and agree with those already there, in `PROJECT_DIR`.
    let mut files = Vec::new();
    let mut bytes = 0u64;
    let mut seen = std::collections::HashSet::new();
    let mut shared: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut roots: BTreeMap<String, String> = BTreeMap::new();
    for f in output.real_functions() {
        let mut own = Vec::new();
        for entry in ignore::WalkBuilder::new(&f.dir).standard_filters(false).follow_links(true).build() {
            let entry = entry.with_context(|| format!("listing functions{}.func", f.path))?;
            if entry.path().is_file() {
                own.push(entry);
            }
        }
        let map = f.config.file_path_map.as_ref().filter(|m| !m.is_empty());
        let mapped_only = map.is_some_and(|m| {
            own.iter().all(|e| e.file_name() == ".vc-config.json") && m.iter().all(|(within, from)| shared.get(within).is_none_or(|at| *at == project.join(from)))
        });
        let base = if mapped_only { PROJECT_DIR.to_owned() } else { format!("{FUNCTIONS_DIR}{}.func", f.path) };
        roots.insert(f.path.clone(), base.clone());
        for entry in own.iter().filter(|_| !mapped_only) {
            let rel = entry.path().strip_prefix(&f.dir).expect("walked from it").to_string_lossy().replace('\\', "/");
            let to = format!("{base}/{rel}");
            if seen.insert(to.clone()) {
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                files.push(crate::package::SourceFile { rel: to, abs: entry.path().to_path_buf() });
            }
        }
        // Files a builder left where they were, by their path in the function.
        for (within, from) in f.config.file_path_map.iter().flatten() {
            let abs = project.join(from);
            if !inside(&abs, project) || !abs.is_file() {
                bail!("functions{}.func lists {from:?} in filePathMap, which isn't a file in the project", f.path);
            }
            let to = format!("{base}/{within}");
            if mapped_only {
                shared.insert(within.clone(), abs.clone());
            }
            if seen.insert(to.clone()) {
                bytes += abs.metadata().map(|m| m.len()).unwrap_or(0);
                files.push(crate::package::SourceFile { rel: to, abs });
            }
        }
    }
    let real: Vec<&Function> = output.real_functions().collect();
    let main = main_function(&real, &names);
    // One process runs every function, in the main one's folder; on Vercel each runs in its own.
    // A function reading a file by a path relative to its folder (SvelteKit's `read()` of a server
    // asset the adapter copied into that function only, Astro's `includeFiles`) finds it in the
    // shared folder too: the other functions' data files are copied there, code left out.
    if let Some(main) = main {
        let (extra, extra_bytes, conflicts) = shared_working_directory(main, &real, &files, &roots);
        for c in conflicts {
            warnings.push(c);
        }
        bytes += extra_bytes;
        files.extend(extra);
    }
    if bytes > MAX_PACKAGE_BYTES {
        bail!("{framework}'s functions come to {} MB, past Lambda's 250 MB limit for a package", bytes / (1024 * 1024));
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));

    let floor = {
        let memory = real.iter().filter_map(|f| f.config.memory).max();
        let duration = real
            .iter()
            .filter_map(|f| f.config.max_duration)
            .map(|d| match d {
                MaxDuration::Seconds(s) => s.clamp(1, MAX_DURATION_S),
                MaxDuration::Max(_) => MAX_DURATION_S,
            })
            .max();
        (memory.is_some() || duration.is_some()).then_some(crate::state::Floor { memory_mb: memory, max_duration_s: duration })
    };
    let dispatcher = dispatcher_source(&real, main, &names, &roots, &env);
    let framework_slug = config.framework.as_ref().and_then(|f| f.slug.clone()).or_else(|| real.iter().find_map(|f| f.config.framework.as_ref()?.slug.clone()));
    Ok(Imported {
        program,
        files,
        dispatcher,
        floor,
        crons: config.crons.clone(),
        schedules: config.schedules.clone(),
        images: config.images.clone(),
        framework: framework_slug,
        static_dir: output.static_dir(),
        overrides: config.overrides.clone(),
        fallbacks,
        functions: real.iter().map(|f| f.path.clone()).collect(),
        deployment_id: config.deployment_id.clone().filter(|name| !name.is_empty()),
        outline: outline(&output, &middleware),
        warnings,
    })
}

/// What the output runs, for people to read (`state::Outline`): each real function with the names
/// linked to it, its prerenders, and the routes that run middleware (a function of their own,
/// listed there rather than with the functions).
fn outline(output: &Output, middleware: &[String]) -> crate::state::Outline {
    use crate::state::{FunctionOutline, MiddlewareOutline, Outline, PrerenderOutline};
    let functions = output
        .real_functions()
        .filter(|f| !middleware.contains(&f.path))
        .map(|f| {
            let mut routes: Vec<String> = output.functions.iter().filter(|a| a.alias_of.as_deref() == Some(f.path.as_str())).map(|a| a.path.clone()).collect();
            routes.sort();
            FunctionOutline {
                path: f.path.clone(),
                routes,
                runtime: f.config.runtime.clone(),
                memory_mb: f.config.memory,
                max_duration_s: f.config.max_duration.map(|d| match d {
                    MaxDuration::Seconds(s) => s.clamp(1, MAX_DURATION_S),
                    MaxDuration::Max(_) => MAX_DURATION_S,
                }),
                streams: f.config.streams(),
            }
        })
        .collect();
    let prerenders = output
        .prerenders
        .iter()
        .map(|p| PrerenderOutline {
            path: p.path.clone(),
            expiration_s: match p.config.expiration {
                Expiration::Seconds(s) => Some(s),
                Expiration::Never => None,
            },
            group: p.config.group,
            fallback: p.fallback.is_some(),
            allow_query: p.config.allow_query.clone(),
        })
        .collect();
    let mut found: Vec<MiddlewareOutline> = Vec::new();
    for route in output.config.routes.as_deref().unwrap_or_default() {
        let Some(name) = route.get("middlewarePath").and_then(Value::as_str) else { continue };
        let function = format!("/{}", name.trim_start_matches('/'));
        // The paths as the framework wrote them (`middlewareRawSrc`), else the route's pattern.
        let raw: Vec<String> = route.get("middlewareRawSrc").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
        let matcher = if raw.is_empty() { route.get("src").and_then(Value::as_str).map(|s| vec![s.to_owned()]) } else { Some(raw) };
        let edge = output.functions.iter().find(|f| f.path == function).is_some_and(|f| f.config.kind() == Kind::Edge);
        match found.iter_mut().find(|m| m.function == function) {
            Some(m) => {
                if let (Some(have), Some(more)) = (&mut m.matcher, matcher) {
                    let new: Vec<String> = more.into_iter().filter(|p| !have.contains(p)).collect();
                    have.extend(new);
                }
            }
            None => found.push(MiddlewareOutline { function, entry: None, matcher, edge }),
        }
    }
    Outline { functions, prerenders, middleware: found, images: None }
}

/// The prerenders as the router serves them (Vercel's ISR: `forkrail_common::rules::prerender`), by
/// every path their function answers at, and their fallback pages by manifest key. A bypass token
/// is kept as its SHA-256: the routers only have to recognise it.
fn prerenders(found: &[Prerender], warnings: &mut Vec<String>) -> (BTreeMap<String, forkrail_common::rules::Prerender>, BTreeMap<String, PathBuf>) {
    use sha2::{Digest, Sha256};
    let mut out = BTreeMap::new();
    let mut fallbacks = BTreeMap::new();
    for p in found {
        let c = &p.config;
        if !c.other.is_empty() {
            let keys: Vec<&str> = c.other.keys().map(String::as_str).collect();
            warnings.push(format!("{}'s prerender settings {} aren't read by forkrail", p.path, keys.join(", ")));
        }
        if c.fallback.is_some() && p.fallback.is_none() {
            warnings.push(format!("{}'s prerender fallback {:?} isn't in the Build Output; the page is made on its first request", p.path, c.fallback.as_deref().unwrap_or_default()));
        }
        let fallback = p.fallback.as_ref().map(|file| {
            let key = forkrail_common::statics::fallback_key(&p.path);
            fallbacks.insert(key.clone(), file.clone());
            key
        });
        let prerender = forkrail_common::rules::Prerender {
            path: p.path.clone(),
            expiration: match c.expiration {
                Expiration::Seconds(s) => Some(s),
                Expiration::Never => None,
            },
            group: c.group,
            // An empty token would let anyone bypass the cache (Next.js's builder may write one).
            bypass_hash: c.bypass_token.as_deref().filter(|t| !t.is_empty()).map(|t| forkrail_common::hex(&Sha256::digest(t.as_bytes()))),
            allow_query: c.allow_query.clone(),
            pass_query: c.pass_query,
            fallback,
            initial_status: c.initial_status,
            initial_headers: c.initial_headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.clone())).collect(),
            expose_err_body: c.expose_err_body,
        };
        for path in paths_of(&p.path) {
            out.insert(path, prerender.clone());
        }
    }
    (out, fallbacks)
}

/// The function whose folder is the process's working directory: the one with the most names (the
/// one most routes reach), the first by path among equals.
fn main_function<'a>(functions: &[&'a Function], names: &BTreeMap<String, Vec<String>>) -> Option<&'a Function> {
    functions.iter().max_by_key(|f| (names.get(&f.path).map_or(0, Vec::len), std::cmp::Reverse(f.path.clone()))).copied()
}

/// Whether a function's file, by its path in the function, is data a function may read by a path
/// relative to its folder (an asset, an included file), not code it loads by its own module paths
/// or the function's own settings.
fn data_file(rel: &str) -> bool {
    const CODE: [&str; 9] = [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".map", ".node", ".wasm"];
    !rel.starts_with("node_modules/") && !rel.contains("/node_modules/") && !matches!(rel, ".vc-config.json" | "package.json") && !CODE.iter().any(|ext| rel.ends_with(ext))
}

/// The other functions' data files (`data_file`) that the main function's folder doesn't have,
/// copied into it, as the package's files; their bytes; and a warning for each one the main
/// function has differently (it reads the main function's there, not its own as on Vercel).
/// The main function's folder is its root (`roots`: the shared project folder when its files all
/// stay in the project); functions there already share one folder, so nothing of theirs is copied.
fn shared_working_directory(main: &Function, functions: &[&Function], files: &[crate::package::SourceFile], roots: &BTreeMap<String, String>) -> (Vec<crate::package::SourceFile>, u64, Vec<String>) {
    use sha2::{Digest, Sha256};
    let digest = |path: &Path| std::fs::read(path).ok().map(|bytes| Sha256::digest(&bytes));
    let main_base = format!("{}/", roots.get(&main.path).cloned().unwrap_or_else(|| format!("{FUNCTIONS_DIR}{}.func", main.path)));
    let others: std::collections::HashSet<&str> = functions.iter().filter(|f| f.path != main.path).map(|f| f.path.as_str()).collect();
    let in_main: BTreeMap<&str, &Path> = files.iter().filter_map(|s| Some((s.rel.strip_prefix(&main_base)?, s.abs.as_path()))).collect();
    let mut main_digests = std::collections::HashMap::new();
    let (mut extra, mut bytes, mut conflicts) = (Vec::new(), 0u64, Vec::new());
    let mut added = std::collections::HashSet::new();
    // One pass over the package's files: each one's function is the path before `.func/` (no
    // function's path holds a `.func` folder: the reader stops at the first).
    for s in files {
        let Some((path, rel)) = s.rel.strip_prefix(FUNCTIONS_DIR).and_then(|r| r.split_once(".func/")) else { continue };
        if !others.contains(path) || !data_file(rel) {
            continue;
        }
        match in_main.get(rel) {
            Some(theirs) => {
                if conflicts.len() < 20 && digest(&s.abs) != *main_digests.entry(rel).or_insert_with(|| digest(theirs)) {
                    conflicts.push(format!("{path}'s {rel} differs from {}'s; forkrail runs every function in {}'s folder, so {path} reads that one", main.path, main.path));
                }
            }
            None if added.insert(rel) => {
                bytes += s.abs.metadata().map(|m| m.len()).unwrap_or(0);
                extra.push(crate::package::SourceFile { rel: format!("{main_base}{rel}"), abs: s.abs.clone() });
            }
            None => {}
        }
    }
    (extra, bytes, conflicts)
}

/// The dispatcher: every real function with the paths it answers at, whether it gets Vercel's
/// helpers, and a loader for its handler, handed to the runtime's shim (`forkrail.buildOutput`).
/// The working directory is `main`'s folder (`main_function`), with the other functions' data
/// files in it (`shared_working_directory`), as each function's own is on Vercel. A function's
/// folder is its root (`roots`): its own `.func` copy, or the shared project folder.
fn dispatcher_source(functions: &[&Function], main: Option<&Function>, names: &BTreeMap<String, Vec<String>>, roots: &BTreeMap<String, String>, env: &BTreeMap<String, String>) -> String {
    // A function's folder, relative to the dispatcher (both in `.forkrail/`).
    let url = |f: &Function, rest: &str| {
        let root = roots.get(&f.path).and_then(|r| r.strip_prefix(".forkrail/")).map_or_else(|| format!("fn{}.func", f.path), str::to_owned);
        let path = format!("{root}/{rest}");
        path.split('/').map(url_segment).collect::<Vec<_>>().join("/")
    };
    let mut out = String::from(
        "// Written by forkrail: the functions of the build's Build Output (.vercel/output), served by\n\
         // forkrail's runtime (shim.mjs), which picks one by the router's function header.\n\
         import { fileURLToPath } from 'node:url';\n\
         const serve = globalThis[Symbol.for('forkrail.buildOutput')];\n\
         if (typeof serve !== 'function') throw new Error(\"forkrail: this app runs under forkrail's runtime\");\n\
         const at = (rel) => new URL(rel, import.meta.url);\n\
         serve({\n",
    );
    if let Some(main) = main {
        out.push_str(&format!("  cwd: fileURLToPath(at({})),\n", js(&url(main, ""))));
    }
    out.push_str(&format!("  env: {},\n  functions: [\n", serde_json::to_string(env).expect("strings")));
    for f in functions {
        let entry = f.config.entry().unwrap_or_default();
        let mut extra = String::new();
        // A Node handler whose default export takes a Web `Request` (React Router's).
        if f.config.kind() == Kind::Node && f.config.other.get("useWebApi") == Some(&Value::Bool(true)) {
            extra.push_str(", web: true");
        }
        if f.config.kind() == Kind::Edge {
            extra.push_str(", edge: true");
            // WebAssembly an Edge function's config binds by name, as globals.
            if let Some(Value::Object(wasm)) = f.config.other.get("wasm") {
                let bindings: Vec<String> = wasm
                    .iter()
                    .filter_map(|(name, path)| Some(format!("{{ name: {}, url: at({}).href }}", js(name), js(&url(f, path.as_str()?)))))
                    .collect();
                extra.push_str(&format!(", wasm: [{}]", bindings.join(", ")));
            }
        }
        out.push_str(&format!(
            "    {{ name: {}, paths: {}, helpers: {}{extra}, load: () => import(at({}).href) }},\n",
            js(&f.path),
            serde_json::to_string(names.get(&f.path).map(Vec::as_slice).unwrap_or_default()).expect("strings"),
            f.config.should_add_helpers,
            js(&url(f, entry)),
        ));
    }
    out.push_str("  ],\n});\n");
    out
}

/// A dispatcher (`dispatcher_source`) with one more function: a project's own Routing Middleware
/// beside the framework's (crates/cli/src/middleware.rs), as `Found::dispatcher_entry` writes it.
pub fn with_function(dispatcher: &str, entry: &str) -> String {
    dispatcher.replacen("  ],\n});\n", &format!("    {entry},\n  ],\n}});\n"), 1)
}

/// The functions `routes` run as Routing Middleware, by path (`/_middleware`).
fn middleware_functions(routes: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for route in routes {
        if let Some(path) = route.get("middlewarePath").and_then(Value::as_str) {
            let path = format!("/{}", path.trim_start_matches('/'));
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

fn js(text: &str) -> String {
    serde_json::to_string(text).expect("a string")
}

/// A path segment as a URL keeps it: letters, digits and `-._~!$&'()*+,;=:@` as they are, the
/// rest percent-encoded (`[` in `[slug]`, spaces, `%`).
fn url_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-boa-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn link(root: &Path, rel: &str, target: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    /// A Build Output laid out as SvelteKit's adapter writes it (docs/data/c16/fw-sveltekit).
    fn sveltekit_like() -> PathBuf {
        let root = temp();
        write(&root, "config.json", r#"{"version": 3, "daemons": {}, "routes": [{"handle": "filesystem"}, {"src": "/.*", "dest": "/![-]/catchall"}],
            "overrides": {"static.html": {"path": "static"}}, "crons": [{"path": "/api/cron", "schedule": "0 5 * * *"}],
            "images": {"sizes": [640], "domains": []}}"#);
        write(&root, "static/static.html", "<h1>static</h1>");
        for (name, extra) in [("0", ""), ("1", r#""memory": 1024, "maxDuration": 30,"#), ("2", ""), ("catchall", "")] {
            let streaming = if name == "2" { "false" } else { "true" };
            write(&root, &format!("functions/![-]/{name}.func/.vc-config.json"), &format!(
                r#"{{"runtime": "nodejs24.x", {extra} "handler": ".svelte-kit/vercel-tmp/index.js", "launcherType": "Nodejs",
                    "experimentalResponseStreaming": {streaming}, "framework": {{"slug": "sveltekit", "version": "3.2.0"}}}}"#
            ));
            write(&root, &format!("functions/![-]/{name}.func/.svelte-kit/vercel-tmp/index.js"), "export default { fetch() {} }");
        }
        link(&root, "functions/index.func", "![-]/0.func");
        link(&root, "functions/index/__data.json.func", "../![-]/0.func");
        link(&root, "functions/api/heavy.func", "../![-]/1.func");
        link(&root, "functions/isr.func", "![-]/2.func");
        link(&root, "functions/isr/[slug].func", "../![-]/2.func");
        write(&root, "functions/isr.prerender-config.json", r#"{"expiration": 30, "bypassToken": "t0ken", "allowQuery": ["__pathname"], "group": 1, "passQuery": true}"#);
        write(&root, "functions/isr/[slug].prerender-config.json", r#"{"expiration": false, "allowQuery": ["__pathname", "v"], "group": 2, "passQuery": true}"#);
        root
    }

    #[test]
    fn reads_functions_their_names_and_prerenders() {
        let root = sveltekit_like();
        let out = read(&root).unwrap();
        let paths: Vec<(&str, Option<&str>)> = out.functions.iter().map(|f| (f.path.as_str(), f.alias_of.as_deref())).collect();
        assert_eq!(
            paths,
            vec![
                ("/![-]/0", None),
                ("/![-]/1", None),
                ("/![-]/2", None),
                ("/![-]/catchall", None),
                ("/api/heavy", Some("/![-]/1")),
                ("/index", Some("/![-]/0")),
                ("/index/__data.json", Some("/![-]/0")),
                ("/isr", Some("/![-]/2")),
                ("/isr/[slug]", Some("/![-]/2")),
            ]
        );
        assert_eq!(out.real_functions().count(), 4);
        let heavy = out.functions.iter().find(|f| f.path == "/api/heavy").unwrap();
        assert_eq!((heavy.config.memory, heavy.config.max_duration), (Some(1024), Some(MaxDuration::Seconds(30))));
        assert_eq!(heavy.config.kind(), Kind::Node);
        assert!(heavy.config.streams());
        assert!(!out.functions.iter().find(|f| f.path == "/isr").unwrap().config.streams());
        assert_eq!(heavy.config.framework.as_ref().and_then(|f| f.slug.as_deref()), Some("sveltekit"));
        let isr: Vec<(&str, Expiration, Option<&[String]>)> = out.prerenders.iter().map(|p| (p.path.as_str(), p.config.expiration, p.config.allow_query.as_deref())).collect();
        assert_eq!(isr.len(), 2);
        assert_eq!((isr[0].0, isr[0].1), ("/isr", Expiration::Seconds(30)));
        assert_eq!((isr[1].0, isr[1].1), ("/isr/[slug]", Expiration::Never));
        assert_eq!(out.prerenders[0].config.bypass_token.as_deref(), Some("t0ken"));
        assert!(out.prerenders[1].config.pass_query);
        assert_eq!(out.config.overrides["static.html"].path.as_deref(), Some("static"));
        assert_eq!(out.config.crons, vec![Cron { path: "/api/cron".into(), schedule: "0 5 * * *".into() }]);
        assert!(out.config.images.is_some() && out.config.daemons.as_ref().is_some_and(BTreeMap::is_empty));
        assert!(out.config.other.is_empty(), "every key in config.json is a known one: {:?}", out.config.other);
        assert_eq!(out.routes_len(), 2);
    }

    impl Output {
        fn routes_len(&self) -> usize {
            self.config.routes.as_ref().map_or(0, Vec::len)
        }
    }

    #[test]
    fn outline_names_functions_prerenders_and_middleware() {
        use crate::state::{FunctionOutline, MiddlewareOutline, PrerenderOutline};
        let root = sveltekit_like();
        let i = import(&root, read(&root).unwrap(), &serde_json::Map::new(), "SvelteKit").unwrap();
        let f = |path: &str, routes: &[&str], memory_mb, max_duration_s, streams| FunctionOutline {
            path: path.into(),
            routes: routes.iter().map(|r| r.to_string()).collect(),
            runtime: "nodejs24.x".into(),
            memory_mb,
            max_duration_s,
            streams,
        };
        assert_eq!(
            i.outline.functions,
            vec![
                f("/![-]/0", &["/index", "/index/__data.json"], None, None, true),
                f("/![-]/1", &["/api/heavy"], Some(1024), Some(30), true),
                f("/![-]/2", &["/isr", "/isr/[slug]"], None, None, false),
                f("/![-]/catchall", &[], None, None, true),
            ]
        );
        assert_eq!(
            i.outline.prerenders,
            vec![
                PrerenderOutline { path: "/isr".into(), expiration_s: Some(30), group: Some(1), fallback: false, allow_query: Some(vec!["__pathname".into()]) },
                PrerenderOutline { path: "/isr/[slug]".into(), expiration_s: None, group: Some(2), fallback: false, allow_query: Some(vec!["__pathname".into(), "v".into()]) },
            ]
        );
        assert!(i.outline.middleware.is_empty() && i.outline.images.is_none());

        // Middleware is listed apart from the functions, with the paths the framework wrote.
        let root = temp();
        write(
            &root,
            "config.json",
            r#"{"version": 3, "routes": [{"src": "^(?:/(about))$", "middlewareRawSrc": ["/about"], "middlewarePath": "_middleware", "continue": true}, {"src": "/(.*)", "middlewarePath": "_middleware", "continue": true}]}"#,
        );
        write(&root, "functions/_middleware.func/.vc-config.json", r#"{"runtime": "edge", "entrypoint": "index.js"}"#);
        write(&root, "functions/_middleware.func/index.js", "");
        write(&root, "functions/hello.func/.vc-config.json", r#"{"runtime": "nodejs24.x", "handler": "index.js", "supportsResponseStreaming": true}"#);
        write(&root, "functions/hello.func/index.js", "");
        let i = import(&root, read(&root).unwrap(), &serde_json::Map::new(), "boa").unwrap();
        assert_eq!(i.outline.functions.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["/hello"]);
        assert_eq!(i.outline.middleware, vec![MiddlewareOutline { function: "/_middleware".into(), entry: None, matcher: Some(vec!["/about".into(), "/(.*)".into()]), edge: true }]);
    }

    #[test]
    fn edge_functions_and_middleware_import() {
        // test-apps/boa/edge-middleware and edge-functions in one output.
        let root = temp();
        write(&root, "config.json", r#"{"version": 3, "routes": [{"src": "/(.*)", "middlewarePath": "_middleware", "continue": true}]}"#);
        write(&root, "static/index.html", "<h1>hi</h1>");
        write(&root, "functions/_middleware.func/.vc-config.json", r#"{"runtime": "edge", "entrypoint": "index.js"}"#);
        write(&root, "functions/_middleware.func/index.js", "export default () => new Response(null, { headers: { 'x-middleware-next': '1' } })");
        write(&root, "functions/hello.func/.vc-config.json", r#"{"runtime": "edge", "entrypoint": "index.js", "wasm": {"wasm_abc": "wasm/abc.wasm"}}"#);
        write(&root, "functions/hello.func/index.js", "export default () => new Response('hi')");
        let output = read(&root).unwrap();
        let i = import(&root, output, &serde_json::Map::new(), "boa").unwrap();
        // The middleware is no path anyone can ask for; the router names it.
        assert!(!i.program.function.iter().any(|p| p.starts_with("/_middleware")), "{:?}", i.program.function);
        assert!(i.program.function.contains(&"/hello".to_owned()));
        assert!(i.dispatcher.contains(r#"{ name: "/_middleware", paths: ["/_middleware"], helpers: false, edge: true"#), "{}", i.dispatcher);
        assert!(i.dispatcher.contains(r#"wasm: [{ name: "wasm_abc", url: at("fn/hello.func/wasm/abc.wasm").href }]"#), "{}", i.dispatcher);
        // A route naming a middleware the output doesn't have.
        let root = temp();
        write(&root, "config.json", r#"{"version": 3, "routes": [{"src": "/(.*)", "middlewarePath": "nope", "continue": true}]}"#);
        let output = read(&root).unwrap();
        assert!(import(&root, output, &serde_json::Map::new(), "boa").unwrap_err().to_string().contains("no functions/nope.func"));
    }

    #[test]
    fn refuses_what_it_cant_trust() {
        // Another version of the API.
        let root = temp();
        write(&root, "config.json", r#"{"version": 2}"#);
        assert!(read(&root).unwrap_err().to_string().contains("version 2"));
        // A link out of the functions folder.
        let root = temp();
        write(&root, "config.json", r#"{"version": 3}"#);
        let outside = temp();
        write(&outside, "evil.func/.vc-config.json", r#"{"runtime": "nodejs24.x", "handler": "index.js"}"#);
        write(&outside, "evil.func/index.js", "");
        std::fs::create_dir_all(root.join("functions")).unwrap();
        std::os::unix::fs::symlink(outside.join("evil.func"), root.join("functions/x.func")).unwrap();
        assert!(read(&root).unwrap_err().to_string().contains("outside the functions folder"));
        // A handler climbing out of its folder.
        let root = temp();
        write(&root, "config.json", r#"{"version": 3}"#);
        write(&root, "functions/a.func/.vc-config.json", r#"{"runtime": "nodejs24.x", "handler": "../../../etc/passwd"}"#);
        assert!(read(&root).unwrap_err().to_string().contains("outside the function's folder"));
        // A missing handler, and none named.
        let root = temp();
        write(&root, "config.json", r#"{"version": 3}"#);
        write(&root, "functions/a.func/.vc-config.json", r#"{"runtime": "nodejs24.x", "handler": "index.js"}"#);
        assert!(read(&root).unwrap_err().to_string().contains("isn't there"));
        write(&root, "functions/a.func/.vc-config.json", r#"{"runtime": "edge"}"#);
        assert!(read(&root).unwrap_err().to_string().contains("names no `handler`"));
        // A prerender with no function, and a bad expiration.
        let root = temp();
        write(&root, "config.json", r#"{"version": 3}"#);
        write(&root, "functions/p.prerender-config.json", r#"{"expiration": 60}"#);
        assert!(read(&root).unwrap_err().to_string().contains("names no function"));
        write(&root, "functions/p.func/.vc-config.json", r#"{"runtime": "nodejs24.x", "handler": "i.js"}"#);
        write(&root, "functions/p.func/i.js", "");
        write(&root, "functions/p.prerender-config.json", r#"{"expiration": true}"#);
        assert!(format!("{:#}", read(&root).unwrap_err()).contains("number of seconds or false"));
    }

    /// Build Outputs shaped as Vercel's own examples (vercel/examples `build-output-api/`, read
    /// 2026-11-16): each imports and routes as its README says.
    #[test]
    fn vercel_examples_import_and_route() {
        use forkrail_common::rules::{Engine, Files, Outcome, Request};
        struct Static(Vec<String>);
        impl Files for Static {
            fn has(&self, path: &str) -> bool {
                self.0.iter().any(|p| p == path)
            }
        }
        let output = |config: &str, files: &[(&str, &str)], links: &[(&str, &str)]| {
            let root = temp();
            write(&root, "config.json", config);
            for (rel, text) in files {
                write(&root, rel, text);
            }
            for (rel, target) in links {
                link(&root, rel, target);
            }
            root
        };
        let load = |root: &Path| import(root, read(root)?, &serde_json::Map::new(), "example");
        let route = |imported: &Imported, files: &[&str], host: &str, path: &str| {
            let engine = Engine::new(&imported.program).unwrap();
            let headers = hyper::HeaderMap::new();
            let files = Static(files.iter().map(|f| (*f).to_owned()).collect());
            let d = engine.route(&Request { method: "GET", path, query: None, host, headers: &headers }, &files);
            (d.outcome, d.status, d.function)
        };
        let node = r#"{"runtime": "nodejs20.x", "handler": "index.js", "launcherType": "Nodejs", "shouldAddHelpers": true}"#;
        // static-files: served at their paths, nothing else.
        let i = load(&output(r#"{"version": 3}"#, &[("static/index.html", "x"), ("static/another.html", "x"), ("static/data.json", "{}")], &[])).unwrap();
        let site = ["/index.html", "/data.json", "/another.html"];
        assert_eq!(route(&i, &site, "x", "/").0, Outcome::Static { file: "/index.html".into() });
        assert_eq!(route(&i, &site, "x", "/another.html").0, Outcome::Static { file: "/another.html".into() });
        assert_eq!(route(&i, &site, "x", "/another").0, Outcome::NotFound);
        // serverless-functions: a function and a symlinked second name for it; `/` is `index.func`.
        let fns = output(r#"{"version": 3}"#, &[("functions/index.func/.vc-config.json", node), ("functions/index.func/index.js", "module.exports = (req, res) => res.end('hi')")], &[("functions/another.func", "index.func")]);
        let i = load(&fns).unwrap();
        assert_eq!(i.functions, vec!["/index".to_owned()]);
        for path in ["/", "/index", "/another"] {
            let (outcome, _, function) = route(&i, &[], "x", path);
            assert!(matches!(outcome, Outcome::Function { .. }), "{path}");
            assert!(i.dispatcher.contains(&format!("\"{}\"", function.clone().unwrap())), "{path} dispatches as {function:?}");
        }
        assert!(i.dispatcher.contains("helpers: true"), "the example asks for Vercel's helpers");
        // routes: a redirect with its capture.
        let i = load(&output(r#"{"version": 3, "routes": [{"src": "/(.*)", "status": 307, "headers": {"Location": "https://example.com/$1"}}]}"#, &[], &[])).unwrap();
        assert_eq!(route(&i, &[], "x", "/a/b"), (Outcome::Redirect { location: "https://example.com/a/b".into() }, Some(307), None));
        // overrides: renamed and retyped files.
        let i = load(&output(r#"{"version": 3, "overrides": {"data": {"contentType": "application/json"}, "another.html": {"path": "something-else"}}}"#, &[("static/data", "{}"), ("static/another.html", "x")], &[])).unwrap();
        assert_eq!(i.overrides["another.html"].path.as_deref(), Some("something-else"));
        assert_eq!(i.overrides["data"].content_type.as_deref(), Some("application/json"));
        // wildcard: `$wildcard` by the request's domain.
        let i = load(&output(
            r#"{"version": 3, "wildcard": [{"domain": "en.example.test", "value": "english"}, {"domain": "es.example.test", "value": "spanish"}], "routes": [{"src": "/", "dest": "/$wildcard.html"}]}"#,
            &[("static/english.html", "x"), ("static/spanish.html", "x")],
            &[],
        ))
        .unwrap();
        let langs = ["/index.html", "/english.html", "/spanish.html"];
        assert_eq!(route(&i, &langs, "es.example.test", "/").0, Outcome::Static { file: "/spanish.html".into() });
        assert_eq!(route(&i, &langs, "EN.example.test", "/").0, Outcome::Static { file: "/english.html".into() });
        // `deploymentId`: carried for skew protection; one that can't travel in a URL refused.
        let named = load(&output(r#"{"version": 3, "deploymentId": "build-42_a"}"#, &[], &[])).unwrap();
        assert_eq!(named.deployment_id.as_deref(), Some("build-42_a"));
        let bad = load(&output(r#"{"version": 3, "deploymentId": "a b"}"#, &[], &[])).unwrap_err();
        assert!(bad.to_string().contains("deploymentId"), "{bad}");
        // Images are carried to the build (F5).
        let images = load(&output(r#"{"version": 3, "images": {"sizes": [640], "domains": []}}"#, &[], &[])).unwrap();
        assert_eq!(images.images, Some(serde_json::json!({"sizes": [640], "domains": []})), "images are carried to the build");
        // Prerenders (F4): each name of the function cached as a page, its fallback uploaded with
        // the static files, its token kept only as a hash. Shaped as Vercel's prerender-functions,
        // on-demand-isr and draft-mode examples.
        let fallback = |name: &str| (format!("functions/{name}"), "<p>fallback</p>".to_owned());
        let func = |path: &str| vec![(format!("functions/{path}.func/.vc-config.json"), node.to_owned()), (format!("functions/{path}.func/index.js"), String::new())];
        let isr = |items: Vec<(String, String)>| {
            let files: Vec<(&str, &str)> = items.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            output(r#"{"version": 3}"#, &files, &[])
        };
        let mut items = func("blog/post");
        items.push(("functions/blog/post.prerender-config.json".into(), r#"{"expiration": 5, "group": 1, "bypassToken": "2ec9172003a647b296f324848dd3d407", "allowQuery": ["slug"], "fallback": "post.prerender-fallback.html"}"#.into()));
        items.push(fallback("blog/post.prerender-fallback.html"));
        let i = load(&isr(items)).unwrap();
        let post = &i.program.prerenders["/blog/post"];
        assert_eq!((post.expiration, post.group, post.pass_query), (Some(5), Some(1), false));
        assert_eq!(post.allow_query.as_deref(), Some(&["slug".to_owned()][..]));
        assert_eq!(post.bypass_hash.as_deref().map(str::len), Some(64));
        assert!(!serde_json::to_string(&i.program).unwrap().contains("2ec9172003a647b296f324848dd3d407"), "the token itself never leaves the build");
        assert_eq!(post.fallback.as_deref(), Some("prerender-fallback:/blog/post"));
        assert!(i.fallbacks["prerender-fallback:/blog/post"].ends_with("post.prerender-fallback.html"));
        assert!(i.program.prerenders.contains_key("/blog/post/"));
        let token = "87734ad8259d67c3c11747d3e4e112d0";
        let mut items = Vec::new();
        for (path, config) in [
            ("index", format!(r#"{{"expiration": 180, "group": 1, "bypassToken": "{token}", "allowQuery": []}}"#)),
            ("data", format!(r#"{{"expiration": 180, "group": 2, "bypassToken": "{token}", "allowQuery": []}}"#)),
            ("blog/post-1", format!(r#"{{"expiration": false, "group": 3, "bypassToken": "{token}", "allowQuery": ["slug"], "fallback": "post-1.prerender-fallback.html"}}"#)),
            ("blog/post-2", format!(r#"{{"expiration": false, "group": 3, "bypassToken": "{token}", "allowQuery": ["slug"], "fallback": "post-2.prerender-fallback.html"}}"#)),
            ("blog/[slug]", format!(r#"{{"expiration": false, "group": 3, "bypassToken": "{token}", "allowQuery": ["slug"], "fallback": "404.prerender-fallback.html"}}"#)),
        ] {
            items.extend(func(path));
            items.push((format!("functions/{path}.prerender-config.json"), config));
        }
        items.extend(func("revalidate"));
        for name in ["blog/post-1.prerender-fallback.html", "blog/post-2.prerender-fallback.html", "blog/404.prerender-fallback.html"] {
            items.push(fallback(name));
        }
        let i = load(&isr(items)).unwrap();
        assert_eq!(i.program.prerenders["/"].expiration, Some(180));
        assert_eq!(i.program.prerenders["/blog/post-1"].expiration, None);
        assert_eq!(i.fallbacks.len(), 3);
        let mut items = func("index");
        items.push(("functions/index.prerender-config.json".into(), format!(r#"{{"expiration": 60, "group": 1, "bypassToken": "{token}", "allowQuery": []}}"#)));
        items.extend(func("login"));
        let i = load(&isr(items)).unwrap();
        assert_eq!(i.program.prerenders["/index"].allow_query.as_deref(), Some(&[][..]));
        assert!(i.fallbacks.is_empty());
        // edge-middleware: middleware on every path, before the static file; its function isn't a path.
        let edge = r#"{"runtime": "edge", "entrypoint": "index.js"}"#;
        let i = load(&output(
            r#"{"version": 3, "routes": [{"src": "/(.*)", "middlewarePath": "_middleware", "continue": true}]}"#,
            &[("static/index.html", "x"), ("functions/_middleware.func/.vc-config.json", edge), ("functions/_middleware.func/index.js", "")],
            &[],
        ))
        .unwrap();
        let engine = Engine::new(&i.program).unwrap();
        let headers = hyper::HeaderMap::new();
        let site = Static(vec!["/index.html".to_owned()]);
        let step = engine.step(&Request { method: "GET", path: "/", query: None, host: "x", headers: &headers }, &site);
        assert!(matches!(&step, forkrail_common::rules::Step::Middleware { function, .. } if function == "/_middleware"), "{step:?}");
        assert_eq!(route(&i, &["/index.html"], "x", "/").0, Outcome::Static { file: "/index.html".into() });
        assert_eq!(route(&i, &["/index.html"], "x", "/_middleware").1, Some(404));
        // edge-functions: an Edge function at `/` (and `/index`), run on Node.
        let i = load(&output(r#"{"version": 3}"#, &[("functions/index.func/.vc-config.json", edge), ("functions/index.func/index.js", "")], &[])).unwrap();
        assert!(i.dispatcher.contains("edge: true"));
        for path in ["/", "/index"] {
            assert!(matches!(route(&i, &[], "x", path).0, Outcome::Function { .. }), "{path}");
        }
    }

    /// A split SvelteKit app's server asset is copied into the one function whose route reads it;
    /// with every function in one process, the shared working directory gets it too (measured
    /// 2026-11-16 on a 20-function split app: one process 132 MB, one per function 1.3 GB).
    /// Every function shares one process, so one `process.env`: functions whose own `environment`
    /// gives a variable different values are refused (one would read the other's), and agreeing
    /// ones are merged into the dispatcher's env.
    #[test]
    fn functions_env_vars_must_agree_in_one_process() {
        let make = |a: &str, b: &str| {
            let root = temp();
            write(&root, "config.json", r#"{"version": 3}"#);
            for (f, env) in [("a", a), ("b", b)] {
                write(&root, &format!("functions/{f}.func/.vc-config.json"), &format!(r#"{{"runtime": "nodejs24.x", "handler": "index.js", "launcherType": "Nodejs", "environment": {env}}}"#));
                write(&root, &format!("functions/{f}.func/index.js"), "export default { fetch() {} }");
            }
            root
        };
        let root = make(r#"{"DB_URL": "postgres://a"}"#, r#"{"DB_URL": "postgres://b"}"#);
        let err = format!("{:#}", import(&root, read(&root).unwrap(), &serde_json::Map::new(), "kit").unwrap_err());
        assert!(err.contains("DB_URL different values"), "{err}");
        let root = make(r#"{"DB_URL": "postgres://a", "ONLY_A": "1"}"#, r#"{"DB_URL": "postgres://a"}"#);
        let i = import(&root, read(&root).unwrap(), &serde_json::Map::new(), "kit").unwrap();
        assert!(i.dispatcher.contains(r#"env: {"DB_URL":"postgres://a","ONLY_A":"1"}"#), "{}", i.dispatcher);
    }

    #[test]
    fn other_functions_data_files_join_the_working_directory() {
        let root = temp();
        write(&root, "config.json", r#"{"version": 3}"#);
        let node = r#"{"runtime": "nodejs24.x", "handler": "index.js", "launcherType": "Nodejs"}"#;
        for f in ["a", "b"] {
            write(&root, &format!("functions/{f}.func/.vc-config.json"), node);
            write(&root, &format!("functions/{f}.func/index.js"), &format!("export default {{ fetch() {{}} }} // {f}"));
            write(&root, &format!("functions/{f}.func/package.json"), r#"{"type": "module"}"#);
            write(&root, &format!("functions/{f}.func/shared.txt"), "same");
            write(&root, &format!("functions/{f}.func/node_modules/x/data.txt"), f);
        }
        write(&root, "functions/b.func/_app/immutable/assets/note.abc.txt", "only b reads this");
        write(&root, "functions/b.func/chunk.js", "export const b = 1");
        write(&root, "functions/b.func/node_modules/y/data.txt", "b's package");
        write(&root, "functions/b.func/settings.json", "b's");
        write(&root, "functions/a.func/settings.json", "a's");
        // `a` has more names, so its folder is the working directory.
        link(&root, "functions/a2.func", "a.func");
        let i = import(&root, read(&root).unwrap(), &serde_json::Map::new(), "kit").unwrap();
        let rels: Vec<&str> = i.files.iter().map(|f| f.rel.as_str()).collect();
        assert!(i.dispatcher.contains(r#"cwd: fileURLToPath(at("fn/a.func/"))"#), "{}", i.dispatcher);
        assert!(rels.contains(&".forkrail/fn/a.func/_app/immutable/assets/note.abc.txt"), "{rels:?}");
        assert!(rels.contains(&".forkrail/fn/b.func/_app/immutable/assets/note.abc.txt"), "b keeps its own");
        for code in [".forkrail/fn/a.func/chunk.js", ".forkrail/fn/a.func/node_modules/y/data.txt"] {
            assert!(!rels.contains(&code), "{code}: code and packages stay in their function");
        }
        assert_eq!(rels.iter().filter(|r| r.ends_with("a.func/shared.txt")).count(), 1, "the same file once");
        assert!(i.warnings.iter().any(|w| w.contains("/b's settings.json differs from /a's")), "{:?}", i.warnings);
    }

    /// Functions whose files all stay in the project (`filePathMap`, as `vercel build` leaves React
    /// Router's) share one copy of them; one whose map gives a file another source, or that has
    /// files of its own, keeps its own folder.
    #[test]
    fn mapped_functions_share_the_projects_files() {
        let project = temp();
        write(&project, "package.json", "{}");
        write(&project, "a/entry.mjs", "export default () => new Response('a')");
        write(&project, "b/entry.mjs", "");
        write(&project, "lib.js", "");
        write(&project, "other-lib.js", "");
        let root = project.join(".vercel/output");
        let mapped = |handler: &str, lib: &str| format!(r#"{{"runtime": "nodejs24.x", "handler": "{handler}", "useWebApi": true, "filePathMap": {{"{handler}": "{handler}", "lib.js": "{lib}", "package.json": "package.json"}}}}"#);
        write(&root, "config.json", r#"{"version": 3}"#);
        write(&root, "functions/a.func/.vc-config.json", &mapped("a/entry.mjs", "lib.js"));
        write(&root, "functions/b.func/.vc-config.json", &mapped("b/entry.mjs", "lib.js"));
        write(&root, "functions/c.func/.vc-config.json", &mapped("b/entry.mjs", "other-lib.js"));
        let i = import(&project, read(&root).unwrap(), &serde_json::Map::new(), "test").unwrap();
        let rels: Vec<&str> = i.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels.iter().filter(|r| **r == ".forkrail/project/lib.js").count(), 1, "{rels:?}");
        assert!(rels.contains(&".forkrail/project/a/entry.mjs") && rels.contains(&".forkrail/project/b/entry.mjs"));
        assert!(rels.contains(&".forkrail/fn/c.func/lib.js"), "c's lib.js is another file: {rels:?}");
        assert!(i.dispatcher.contains(r#"load: () => import(at("project/a/entry.mjs").href)"#), "{}", i.dispatcher);
        assert!(i.dispatcher.contains(r#"load: () => import(at("fn/c.func/b/entry.mjs").href)"#), "{}", i.dispatcher);
        assert_eq!(i.dispatcher.matches("web: true").count(), 3);
    }

    /// Nitro's Build Outputs as forkrail's build of test-apps/fw-nuxt (Nitro 2), fw-tanstack and
    /// fw-vite-nitro (Nitro 3) left them, with skew protection on (`testdata/nitro/<app>.json`):
    /// made again on disk, imported with the project's vercel.json, and what the import makes (its
    /// functions, prerenders, crons, routes, and how a list of requests routes) checked against
    /// `testdata/nitro/<app>.golden.json`. `FORKRAIL_BLESS=1` writes the golden files anew.
    #[test]
    fn nitro_outputs_import_as_their_golden_files() {
        use forkrail_common::rules::{Engine, Files, Request};
        struct Static(Vec<String>);
        impl Files for Static {
            fn has(&self, path: &str) -> bool {
                self.0.iter().any(|p| p == path)
            }
        }
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/testdata/nitro");
        let bless = std::env::var("FORKRAIL_BLESS").is_ok_and(|v| v == "1");
        for app in ["fw-nuxt", "fw-tanstack", "fw-vite-nitro"] {
            let input: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{app}.json"))).unwrap()).unwrap();
            let root = temp();
            write(&root, "config.json", &input["config"].to_string());
            for (path, f) in input["functions"].as_object().unwrap() {
                let rel = format!("functions{path}.func");
                match f.get("link").and_then(Value::as_str) {
                    // As Nitro links them: `./__server.func`, `./../__server.func`.
                    Some(target) => link(&root, &rel, &format!("./{}{}.func", "../".repeat(path.matches('/').count() - 1), target.trim_start_matches('/'))),
                    None => {
                        write(&root, &format!("{rel}/.vc-config.json"), &f["vc"].to_string());
                        write(&root, &format!("{rel}/{}", f["vc"]["handler"].as_str().unwrap()), "export default {}");
                    }
                }
            }
            for (path, config) in input["prerenders"].as_object().unwrap() {
                write(&root, &format!("functions{path}.prerender-config.json"), &config.to_string());
            }
            let statics: Vec<String> = input["static"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_owned()).collect();
            for file in &statics {
                write(&root, &format!("static{file}"), "");
            }
            let vercel_json = input["vercel_json"].as_object().cloned().unwrap_or_default();
            let imported = import(&root, read(&root).unwrap(), &vercel_json, input["framework"].as_str().unwrap()).unwrap();
            let engine = Engine::new(&imported.program).unwrap();
            let files = Static(statics);
            let headers = hyper::HeaderMap::new();
            let mut decisions = serde_json::Map::new();
            for url in input["requests"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()) {
                let (path, query) = url.split_once('?').map_or((url, None), |(p, q)| (p, Some(q)));
                let d = engine.route(&Request { method: "GET", path, query, host: "app.example.test", headers: &headers }, &files);
                decisions.insert(
                    url.to_owned(),
                    serde_json::json!({"outcome": format!("{:?}", d.outcome), "status": d.status, "headers": d.headers, "function": d.function, "captures": d.captures}),
                );
            }
            let got = serde_json::json!({
                "functions": imported.functions,
                "paths": imported.program.function,
                "dispatcher": imported.dispatcher.lines().filter(|l| l.contains("name:")).map(|l| l.split(", load:").next().unwrap_or(l).trim()).collect::<Vec<_>>(),
                "floor": imported.floor,
                "max_duration": imported.program.max_duration,
                "prerenders": imported.program.prerenders,
                "crons": imported.crons.iter().map(|c| serde_json::json!({"path": c.path, "schedule": c.schedule})).collect::<Vec<_>>(),
                "schedules": imported.schedules,
                "images": imported.images,
                "framework": imported.framework,
                "warnings": imported.warnings,
                "routes": imported.program.routes,
                "decisions": decisions,
            });
            let golden = dir.join(format!("{app}.golden.json"));
            if bless {
                std::fs::write(&golden, serde_json::to_string_pretty(&got).unwrap() + "\n").unwrap();
                continue;
            }
            let want: Value = serde_json::from_str(&std::fs::read_to_string(&golden).unwrap_or_else(|_| panic!("{} is missing: FORKRAIL_BLESS=1 writes it", golden.display()))).unwrap();
            for key in want.as_object().unwrap().keys() {
                assert_eq!(got[key], want[key], "{app}: {key} differs from {}", golden.display());
            }
            assert_eq!(got, want, "{app}");
        }
    }

    #[test]
    fn edge_functions_and_unknown_keys() {
        let root = temp();
        write(&root, "config.json", r#"{"version": 3, "someday": 1}"#);
        write(&root, "functions/_middleware.func/.vc-config.json", r#"{"runtime": "edge", "entrypoint": "index.js", "envVarsInUse": ["A"], "regions": "all"}"#);
        write(&root, "functions/_middleware.func/index.js", "export default () => new Response('x')");
        write(&root, "functions/max.func/.vc-config.json", r#"{"runtime": "nodejs24.x", "handler": "i.js", "maxDuration": "max", "supportsResponseStreaming": true}"#);
        write(&root, "functions/max.func/i.js", "");
        let out = read(&root).unwrap();
        let edge = &out.functions[0];
        assert_eq!((edge.path.as_str(), edge.config.kind(), edge.config.entry()), ("/_middleware", Kind::Edge, Some("index.js")));
        assert_eq!(out.functions[1].config.max_duration, Some(MaxDuration::Max(MaxWord::Max)));
        assert!(out.functions[1].config.streams());
        assert_eq!(out.config.other.get("someday"), Some(&serde_json::json!(1)));
    }
}

/// Reads a real Build Output named by `FORKRAIL_BOA` (a `.vercel/output` folder), when set:
/// `FORKRAIL_BOA=test-apps/fw-sveltekit/.vercel/output cargo test -p forkrail-cli real_output -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore]
fn real_output() {
    let Ok(dir) = std::env::var("FORKRAIL_BOA") else { return };
    let out = read(Path::new(&dir)).unwrap();
    println!("functions: {} ({} real), prerenders: {}, routes: {}", out.functions.len(), out.real_functions().count(), out.prerenders.len(), out.config.routes.as_ref().map_or(0, Vec::len));
    for f in out.real_functions() {
        println!("  {} runtime={} entry={:?} memory={:?} maxDuration={:?} streams={} other={:?}", f.path, f.config.runtime, f.config.entry(), f.config.memory, f.config.max_duration, f.config.streams(), f.config.other);
    }
    for p in &out.prerenders {
        println!("  prerender {} {:?} allowQuery={:?}", p.path, p.config.expiration, p.config.allow_query);
    }
    println!("unknown config keys: {:?}", out.config.other.keys().collect::<Vec<_>>());
}

/// Imports every Build Output example in `test-apps/boa/` (kept on this machine only: Vercel's
/// vercel/examples `build-output-api/`) and prints what it made or why it was refused: `cargo test -p forkrail-cli boa_examples -- --ignored --nocapture`.
#[cfg(test)]
#[test]
#[ignore]
fn boa_examples() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-apps/boa");
    if !root.is_dir() {
        return;
    }
    let mut names: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().filter(|e| e.path().join(".vercel/output").is_dir()).map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let dir = root.join(&name);
        let result = read(&dir.join(".vercel/output")).and_then(|out| import(&dir, out, &serde_json::Map::new(), "example"));
        match result {
            Ok(i) => println!("{:<22} ok: {} functions, {} routes, {} overrides", name.to_string_lossy(), i.functions.len(), i.program.routes.len(), i.overrides.len()),
            Err(e) => println!("{:<22} refused: {e:#}", name.to_string_lossy()),
        }
    }
}
