//! What forkrail takes from a project's `vercel.json`, and what kind of project it is:
//!
//! - **An API**: a function's entry (`builds`, `functions`, or `api/main.py` / `server.mjs`), its
//!   memory and maxDuration, whether it runs on concurrent compute (`fluid`), and `public/` for
//!   static files.
//! - **A server a framework builds** (Nitro: TanStack Start, Nuxt, SolidStart; see `framework.rs`).
//! - **A backend preset's server** (FastAPI, Flask, Django, Express, Hono, Go, Rust, …; see
//!   `backends.rs`), its entry found the way Vercel finds it.
//! - **A static site**: a framework's build output, or the folder itself (no function at all).
//!
//! Every kind takes Vercel's routing keys (`redirects`, `rewrites`, `headers`, `cleanUrls`,
//! `trailingSlash`, `routes`), compiled into a program the router runs (`forkrail_common::rules`).
//! Keys forkrail doesn't understand are refused rather than silently ignored, so a deployment
//! never behaves differently from Vercel without saying so.
//!
//! Two keys say which pushes to a connected repository deploy at all, as on Vercel:
//! `git.deploymentEnabled` (false, or branches and patterns set to false) is read before anything
//! is built (`push_deploys`), and `ignoreCommand` runs at the start of a push's build, which is
//! skipped when it exits 0 (build.rs).

use std::path::Path;

use anyhow::{Context, Result, bail};
use forkrail_common::rules::{self, Program};
use serde_json::{Map, Value};

use forkrail_common::schedules::{self as sched, Cron, Schedule};

use crate::backends::{self, Backend};
use crate::framework::{self, Framework};
use crate::runtimes::{self, Version};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Python,
    Node,
    /// A Go web server, built into a program (native.rs).
    Go,
    /// A Rust web server, built into a program (native.rs).
    Rust,
}

impl Language {
    /// The Lambda runtime a build from before versions were read ran on: the managed runtime the
    /// app runs on with our bootstrap as its exec wrapper, or, for a program, Amazon Linux with our
    /// bootstrap as the runtime itself. A build says its own (`Build::runtime`).
    pub fn runtime(self) -> &'static str {
        match self {
            Language::Python => "python3.13",
            Language::Node => "nodejs24.x",
            Language::Go | Language::Rust => crate::native::RUNTIME,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Language::Python => "Python",
            Language::Node => "Node",
            Language::Go => "Go",
            Language::Rust => "Rust",
        }
    }

    /// Whether the app is a program of its own, on `provided.al2023`, rather than code a managed
    /// runtime's interpreter runs.
    pub fn compiled(self) -> bool {
        matches!(self, Language::Go | Language::Rust)
    }

    /// The language a deployment's recorded Lambda runtime runs. A program's runtime doesn't say
    /// which language built it: see `from_record`.
    pub fn from_runtime(runtime: &str) -> Option<Self> {
        if runtime.starts_with("python") {
            Some(Language::Python)
        } else if runtime.starts_with("nodejs") {
            Some(Language::Node)
        } else {
            None
        }
    }

    /// The language of a deployment by its record's runtime and framework: a program on
    /// `provided.al2023` was Rust's if its framework says so, else Go's (they publish alike).
    pub fn from_record(runtime: &str, framework: Option<&str>) -> Option<Self> {
        if runtime.starts_with("provided") {
            return Some(if framework == Some("Rust") { Language::Rust } else { Language::Go });
        }
        Self::from_runtime(runtime)
    }

    fn of(entry: &str) -> Result<Self> {
        match entry.rsplit_once('.').map(|(_, ext)| ext) {
            Some("py") => Ok(Language::Python),
            Some("mjs" | "js" | "cjs") => Ok(Language::Node),
            _ => bail!("{entry}: only Python (.py) and JavaScript (.mjs, .js, .cjs) entries are supported so far"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// Relative to the project directory, with forward slashes, e.g. `api/main.py`.
    pub entry: String,
    pub language: Language,
    /// From `functions`, if set there.
    pub memory_mb: Option<u32>,
    pub max_duration_s: Option<u64>,
    /// Concurrent compute, from `fluid` (Vercel's key for its own version), if set: `false` runs one
    /// request per worker, as Vercel does with fluid compute off.
    pub concurrent: Option<bool>,
    /// Things forkrail does differently from Vercel, worth telling the user.
    pub warnings: Vec<String>,
    /// The `vercel.json` it came from, kept in the deployment record.
    pub config: Option<Value>,
    /// The framework that builds it, if any. Then `entry` is relative to its output.
    pub framework: Option<Framework>,
    /// `ignoreCommand`: run at the start of a push's build, which is skipped if it exits 0.
    pub ignore_command: Option<String>,
    /// The routing rules, if `vercel.json` has any that change routing.
    pub routing: Option<Program>,
    /// An API's static files, relative to the project: `public/`, when it has one. A framework
    /// says where its own are.
    pub public: Option<String>,
    /// `bulkRedirectsPath`: a file or folder of redirects, relative to the project, read after the
    /// build (which may make it).
    pub bulk_redirects: Option<String>,
    /// `crons` and `schedules`: requests made to the production deployment on a cron expression.
    pub schedules: Vec<Schedule>,
    /// The backend preset that found the entry, if one did.
    pub backend: Option<Backend>,
    /// The version of its language the function runs (Python, Node), and its Lambda runtime. None
    /// for a static site, and for Go and Rust, whose program runs on `provided.al2023`.
    pub version: Option<Version>,
    /// Per-file functions: each one's own settings, from `functions` and its file.
    pub functions: Vec<FunctionSettings>,
    /// Per-file functions: the least memory and maxDuration their shared function needs.
    pub floor: Option<crate::state::Floor>,
}

/// A per-file function's own settings: from the first `functions` pattern that matches its file (as
/// on Vercel, patterns don't merge), and for maxDuration, else from the file's own `config`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunctionSettings {
    pub file: String,
    pub memory_mb: Option<u32>,
    pub max_duration_s: Option<u64>,
    /// `includeFiles` and `excludeFiles`: globs relative to the project.
    pub include_files: Option<String>,
    pub exclude_files: Option<String>,
}

impl Spec {
    /// The Lambda runtime its function runs on.
    pub fn runtime(&self) -> String {
        match &self.version {
            Some(v) => v.runtime.clone(),
            None => self.language.runtime().to_owned(),
        }
    }

    /// The file a Python project's version and requirements are looked for beside: its entry, or
    /// for per-file functions the first of them (`api/index.py`), as Vercel looks beside each.
    pub fn python_entry(&self) -> &str {
        match self.functions.first() {
            Some(f) => &f.file,
            None => self.entry.split(':').next().unwrap_or(&self.entry),
        }
    }

    /// Whether it deploys static files alone, with no function.
    pub fn static_only(&self) -> bool {
        self.framework.as_ref().is_some_and(|f| !f.server()) && self.backend.is_none()
    }

    /// Its per-file functions (Vercel's `api/*.go`, `api/*.rs`), if that's what it runs.
    pub fn file_functions(&self) -> Option<&backends::FileFunctions> {
        self.backend.as_ref().and_then(|b| b.files.as_ref())
    }

    /// What it is, for the user: `api/main.py (Python)`, `TanStack Start`, `a static site (Vite)`.
    pub fn describe(&self) -> String {
        let version = self.version.as_ref().map(|v| format!(" {}", v.version)).unwrap_or_default();
        match (&self.framework, &self.backend) {
            (Some(f), Some(b)) if b.files.is_some() => {
                let site = if f.name == "Other" { String::new() } else { format!(" and a static site ({})", f.name) };
                format!("{} functions in api/{site}", b.language().name())
            }
            (Some(f), _) if !f.server() => format!("a static site ({})", f.name),
            (Some(f), _) if f.next() => format!("{} (Node{version})", f.name),
            (Some(f), _) => format!("{} ({}, Node{version})", self.entry, f.name),
            (None, Some(b)) if b.preset.name != self.language.name() => format!("{} ({}, {}{version})", self.entry, b.preset.name, self.language.name()),
            _ => format!("{} ({}{version})", self.entry, self.language.name()),
        }
    }
}

/// Keys any project may have: ones that change nothing about how it runs on forkrail, the git
/// keys, the routing keys, Image Optimization's `images` (checked at build, see `build::images`)
/// and `proxy` (Routing Middleware: the build says where it can run, middleware.rs).
const COMMON_KEYS: [&str; 11] = ["$schema", "version", "name", "regions", "github", "devCommand", "git", "ignoreCommand", "bulkRedirectsPath", "images", "proxy"];
/// And by kind of project.
const API_KEYS: [&str; 3] = ["builds", "functions", "fluid"];
const SERVER_KEYS: [&str; 4] = ["fluid", "framework", "buildCommand", "installCommand"];
/// Next.js's: a server's, and `functions`, whose patterns name its routes' source files
/// (`app/api/**/*`) for their `memory` and `maxDuration` (next.rs), and `outputDirectory`,
/// accepted and unused (`next::framework`).
const NEXT_KEYS: [&str; 6] = ["fluid", "framework", "buildCommand", "installCommand", "functions", "outputDirectory"];
/// A backend preset's: an API's, its preset, and for Node its install and build commands.
const BACKEND_KEYS: [&str; 6] = ["builds", "functions", "fluid", "framework", "installCommand", "buildCommand"];
/// Per-file functions beside a site: a site's keys, and the functions' settings.
const FUNCTIONS_KEYS: [&str; 6] = ["functions", "fluid", "framework", "buildCommand", "installCommand", "outputDirectory"];
const STATIC_KEYS: [&str; 4] = ["framework", "buildCommand", "installCommand", "outputDirectory"];
const FUNCTION_KEYS: [&str; 2] = ["memory", "maxDuration"];
/// Keys of projects with a function: schedules.
const SCHEDULE_KEYS: [&str; 2] = ["crons", "schedules"];
/// Keys of Vercel's that a later part of forkrail brings, and what to tell the user meanwhile.
const LATER: [(&str, &str); 4] = [
    ("skipMiddlewareRequestBody", "a middleware gets the request's body on forkrail; this setting isn't supported yet"),
    ("functionFailoverRegions", "function failover regions aren't supported yet"),
    ("bunVersion", "the Bun runtime isn't supported yet"),
    ("public", "Vercel no longer supports `public` either: deployments are public unless Deployment Protection says otherwise"),
];

/// What kind of project a `vercel.json` belongs to, for the keys it may have.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Api,
    Server,
    Next,
    Static,
    Backend,
    Functions,
}

fn check_keys(top: &Map<String, Value>, kind: Kind, what: &str) -> Result<()> {
    let own: &[&str] = match kind {
        Kind::Api => &API_KEYS,
        Kind::Server => &SERVER_KEYS,
        Kind::Next => &NEXT_KEYS,
        Kind::Static => &STATIC_KEYS,
        Kind::Backend => &BACKEND_KEYS,
        Kind::Functions => &FUNCTIONS_KEYS,
    };
    for key in top.keys() {
        if COMMON_KEYS.contains(&key.as_str()) || rules::ROUTING_KEYS.contains(&key.as_str()) || own.contains(&key.as_str()) {
            continue;
        }
        // Schedules call a function, which a static site hasn't got, unless its build makes one
        // (a Build Output, Routing Middleware's doesn't count): the build checks (build.rs).
        if SCHEDULE_KEYS.contains(&key.as_str()) {
            continue;
        }
        // `"framework": null` is Vercel's "Other": what an API is.
        if key == "framework" && kind == Kind::Api && top[key].is_null() {
            continue;
        }
        if let Some((_, why)) = LATER.iter().find(|(k, _)| k == key) {
            bail!("`{key}`: {why}");
        }
        bail!("`{key}` isn't supported for {what} (forkrail understands {}, the routing keys and {})", own.join(", "), COMMON_KEYS.join(", "));
    }
    if let Some(version) = top.get("version")
        && version != 2
    {
        bail!("`version` must be 2");
    }
    Ok(())
}

/// The routing program for a `vercel.json`, if it has routing keys, with the framework preset's
/// own routes if it has any.
fn routing(top: Option<&Map<String, Value>>, function: &rules::Function, framework: Option<&Framework>) -> Result<Option<Program>> {
    let preset = match framework.filter(|f| !f.routes.is_empty()) {
        Some(f) => Some((f.name, serde_json::from_str::<Value>(f.routes).context("a preset's routes aren't JSON")?)),
        None => None,
    };
    let empty = Map::new();
    let top = match top {
        Some(top) => top,
        None if preset.is_some() || !function.files.is_empty() => &empty,
        None => return Ok(None),
    };
    Program::from_vercel(top, function, preset.as_ref().map(|(name, list)| (*name, list))).map_err(anyhow::Error::msg)
}

/// The configuration files Vercel reads, in its order of preference. A project may have one.
pub const CONFIG_FILES: [&str; 7] = ["vercel.json", "vercel.toml", "vercel.ts", "vercel.mts", "vercel.js", "vercel.mjs", "vercel.cjs"];

/// How long a `vercel.ts` may run (it may fetch its rules from elsewhere).
const CONFIG_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The project's configuration as `vercel.json` would hold it, and the file it came from:
/// `vercel.json`, `vercel.toml`, or the `config` a `vercel.ts` (or `.mts`, `.js`, `.mjs`, `.cjs`)
/// exports, run with Node at build time as on Vercel. Two such files is a mistake, as on Vercel.
pub fn load_config(dir: &Path) -> Result<Option<(&'static str, Value)>> {
    Ok(load_config_ordered(dir, true)?.map(|(name, value, _)| (name, value)))
}

/// `load_config` without running anything: a `vercel.ts` (or `.js` …) is the repository's code,
/// run only in a builder, so outside one it can't be read. What decides a push before it's built
/// (`push_deploys`, `github_keys`) runs where the commit is fetched, which may be a person's
/// own machine with their credentials (`forkrail git deploy`).
pub fn load_static_config(dir: &Path) -> Result<Option<(&'static str, Value)>> {
    Ok(load_config_ordered(dir, false)?.map(|(name, value, _)| (name, value)))
}

/// A configuration file's name, its settings, and `functions`' keys in their order.
type Loaded = (&'static str, Value, Option<Vec<String>>);

/// `load_config`, with the keys of `functions` in the order they're written (`serde_json` keeps
/// objects sorted), which decides which pattern a file gets, as on Vercel. None for `vercel.toml`,
/// whose order is lost: there they're tried sorted.
fn load_config_ordered(dir: &Path, run: bool) -> Result<Option<Loaded>> {
    let found: Vec<&'static str> = CONFIG_FILES.iter().copied().filter(|f| dir.join(f).is_file()).collect();
    let name = match found.as_slice() {
        [] => return Ok(None),
        [one] => *one,
        many => bail!("{} are all configuration files: keep one (Vercel refuses more than one too)", many.join(" and ")),
    };
    let path = dir.join(name);
    let (value, order): (Value, Option<Vec<String>>) = match name {
        "vercel.json" => {
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {name}"))?;
            (serde_json::from_str(&text).with_context(|| format!("{name}: not valid JSON"))?, keys_in_order(&text, "functions"))
        }
        "vercel.toml" => {
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {name}"))?;
            let table: toml::Table = toml::from_str(&text).with_context(|| format!("{name}: not valid TOML"))?;
            (serde_json::to_value(table).with_context(|| format!("{name}: can't be read as JSON"))?, None)
        }
        _ if !run => bail!("{name} is the repository's code: it runs only in a builder"),
        _ => {
            let json = run_config(dir, name)?;
            let order = keys_in_order(&json, "functions");
            (serde_json::from_str(&json).with_context(|| format!("{name}: its config isn't plain JSON data"))?, order)
        }
    };
    if !value.is_object() {
        bail!("{name}: expected an object of settings");
    }
    Ok(Some((name, value, order)))
}

/// The keys of the top-level object's `key` object in JSON `text`, in the order they're written.
fn keys_in_order(text: &str, key: &str) -> Option<Vec<String>> {
    use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
    struct Keys;
    impl<'de> Visitor<'de> for Keys {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Vec<String>, A::Error> {
            let mut keys = Vec::new();
            while let Some(k) = map.next_key::<String>()? {
                map.next_value::<IgnoredAny>()?;
                keys.push(k);
            }
            Ok(keys)
        }
    }
    impl<'de> DeserializeSeed<'de> for Keys {
        type Value = Vec<String>;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Vec<String>, D::Error> {
            d.deserialize_map(Keys)
        }
    }
    struct Top<'a>(&'a str);
    impl<'de> Visitor<'de> for Top<'_> {
        type Value = Option<Vec<String>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut found = None;
            while let Some(k) = map.next_key::<String>()? {
                if k == self.0 {
                    found = map.next_value_seed(Keys).ok();
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(found)
        }
    }
    let mut d = serde_json::Deserializer::from_str(text);
    d.deserialize_map(Top(key)).ok().flatten()
}

/// Run a `vercel.ts` (or `.js`...) with Node and take the `config` it exports (or its default
/// export), as JSON. Node strips its types. If it imports `@vercel/config` before the project's
/// packages are installed, that one package is installed in a cache of forkrail's and the file run
/// again.
fn run_config(dir: &Path, name: &str) -> Result<String> {
    // Resolve `@vercel/config` from forkrail's cache when the project hasn't got it.
    const HOOKS: &str = r#"let fallback; export async function initialize(d) { fallback = d }
export async function resolve(s, c, next) { try { return await next(s, c) } catch (e) { if (fallback && s.startsWith('@vercel/config')) return next(s, { ...c, parentURL: fallback }); throw e } }"#;
    const RUN: &str = r#"import { register } from 'node:module'; import { pathToFileURL } from 'node:url';
const [file, hooks, fallback] = process.argv.slice(1);
register('data:text/javascript,' + encodeURIComponent(hooks), import.meta.url, { data: fallback });
const m = await import(pathToFileURL(file).href);
let config = m.config ?? m.default?.config ?? m.default;
if (typeof config === 'function') config = config();
config = await config;
process.stdout.write('\n\u0000forkrail-config\u0000' + JSON.stringify(config ?? null));"#;
    let cache = std::env::temp_dir().join("forkrail-vercel-config");
    let fallback = format!("file://{}/package.json", cache.display());
    let run = || -> Result<(bool, String, String)> {
        let mut child = std::process::Command::new("node")
            .args(["--input-type=module", "-e", RUN])
            .arg(dir.join(name))
            .arg(HOOKS)
            .arg(&fallback)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("{name} needs Node to run, and `node` couldn't be started"))?;
        let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let read_out = std::thread::spawn(move || {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut out, &mut s).ok();
            s
        });
        let read_err = std::thread::spawn(move || {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut err, &mut s).ok();
            s
        });
        let started = std::time::Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() > CONFIG_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{name} ran for more than {} s", CONFIG_TIMEOUT.as_secs());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        Ok((status.success(), read_out.join().unwrap_or_default(), read_err.join().unwrap_or_default()))
    };
    let (mut ok, mut out, mut err) = run()?;
    if !ok && err.contains("@vercel/config") && (err.contains("ERR_MODULE_NOT_FOUND") || err.contains("Cannot find package")) {
        let version = std::fs::read_to_string(dir.join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|p| ["dependencies", "devDependencies"].iter().find_map(|d| p.get(d)?.get("@vercel/config")?.as_str().map(str::to_owned)))
            .unwrap_or_else(|| "latest".to_owned());
        std::fs::create_dir_all(&cache)?;
        let installed = std::process::Command::new("npm")
            .args(["install", "--no-save", "--ignore-scripts", "--no-audit", "--no-fund", "--loglevel=error", "--prefix"])
            .arg(&cache)
            .arg(format!("@vercel/config@{version}"))
            .stdin(std::process::Stdio::null())
            .output()
            .context("installing @vercel/config for vercel.ts")?;
        if !installed.status.success() {
            bail!("{name} imports @vercel/config, which couldn't be installed: {}", String::from_utf8_lossy(&installed.stderr).trim());
        }
        (ok, out, err) = run()?;
    }
    if !ok {
        let err = err.trim();
        let tail: Vec<&str> = err.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
        bail!("{name} failed to run:\n{}", tail.join("\n"));
    }
    let Some((_, json)) = out.rsplit_once("\u{0}forkrail-config\u{0}") else { bail!("{name} ran but gave no configuration") };
    let value: Value = serde_json::from_str(json).with_context(|| format!("{name}: its config isn't plain JSON data"))?;
    if value.is_null() {
        bail!("{name} exports no `config`");
    }
    Ok(json.to_owned())
}

/// Read the project's configuration (see `load_config`), or find the entry without one.
pub fn read(dir: &Path) -> Result<Spec> {
    read_as(dir, None)
}

/// `read`, for a project whose settings choose its framework preset (`preset`, a Vercel slug such
/// as `flask`): `vercel.json`'s `framework` still wins, as on Vercel.
pub fn read_as(dir: &Path, preset: Option<&str>) -> Result<Spec> {
    let mut spec = read_spec(dir, preset)?;
    if let Some(top) = spec.config.as_ref().and_then(Value::as_object) {
        spec.bulk_redirects = bulk_path(top)?;
        spec.schedules = schedules(top)?;
    }
    Ok(spec)
}

/// `crons` (`[{ "path", "schedule" }]`, GET, UTC) and `schedules` (`{ name: { "schedule", "path",
/// "requestMethod", "timezone", "jitter" } }`, path targets only), checked as Vercel checks them.
pub fn schedules(top: &Map<String, Value>) -> Result<Vec<Schedule>> {
    let mut out: Vec<Schedule> = Vec::new();
    match top.get("crons") {
        None | Some(Value::Null) => {}
        Some(Value::Array(crons)) => {
            for (i, cron) in crons.iter().enumerate() {
                let what = format!("`crons[{i}]`");
                let Some(cron) = cron.as_object() else { bail!("{what} must be an object with `path` and `schedule`") };
                if let Some(key) = cron.keys().find(|k| !["path", "schedule"].contains(&k.as_str())) {
                    bail!("{what}: `{key}` isn't a cron job key (they have `path` and `schedule`)");
                }
                let path = schedule_path(cron.get("path"), &what)?;
                let expression = expression(cron.get("schedule"), &what)?;
                let key = sched::cron_key(&path, &expression);
                if out.iter().any(|s| s.key == key) {
                    bail!("{what}: {path} on {expression:?} is listed twice");
                }
                out.push(Schedule { key, name: None, path, method: "GET".to_owned(), expression, timezone: None, jitter: None });
            }
        }
        Some(_) => bail!("`crons` must be a list of {{ \"path\", \"schedule\" }}"),
    }
    match top.get("schedules") {
        None | Some(Value::Null) => {}
        Some(Value::Object(schedules)) => {
            for (name, entry) in schedules {
                let what = format!("`schedules.{name}`");
                if !sched::valid_name(name) {
                    bail!("{what}: a name is 1-{} of letters, digits, `.`, `_` and `-`, starting with a letter or digit", sched::MAX_NAME);
                }
                let Some(entry) = entry.as_object() else { bail!("{what} must be an object") };
                for target in ["function", "service", "topic"] {
                    if entry.contains_key(target) {
                        bail!("{what}: `{target}` targets aren't supported yet; give the schedule a `path` on the deployment");
                    }
                }
                if let Some(key) = entry.keys().find(|k| !["schedule", "path", "requestMethod", "timezone", "jitter"].contains(&k.as_str())) {
                    bail!("{what}: `{key}` isn't a schedule key (they have `schedule`, `path`, `requestMethod`, `timezone` and `jitter`)");
                }
                if !entry.contains_key("path") {
                    bail!("{what} needs a `path` on the deployment to send its request to");
                }
                let path = schedule_path(entry.get("path"), &what)?;
                let expression = expression(entry.get("schedule"), &what)?;
                let method = match entry.get("requestMethod") {
                    None | Some(Value::Null) => "POST",
                    Some(Value::String(m)) if m == "GET" || m == "POST" => m.as_str(),
                    Some(_) => bail!("{what}: `requestMethod` is \"GET\" or \"POST\""),
                };
                let timezone = match entry.get("timezone") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(tz)) => {
                        if tz.parse::<chrono_tz::Tz>().is_err() {
                            bail!("{what}: `timezone` {tz:?} isn't an IANA time zone such as \"America/New_York\"");
                        }
                        Some(tz.clone()).filter(|tz| tz != "UTC" && tz != "Etc/UTC")
                    }
                    Some(_) => bail!("{what}: `timezone` must be a string"),
                };
                let jitter = match entry.get("jitter") {
                    None | Some(Value::Null) => 1,
                    Some(v) => int(v, &format!("{what}: `jitter`"), 1, 15)? as u32,
                };
                let shortest = Cron::parse(&expression).map(|c| c.min_interval_s()).unwrap_or(u64::MAX);
                if u64::from(jitter) * 60 > shortest {
                    bail!("{what}: a jitter of {jitter} minutes is longer than the {} minutes between its runs", shortest / 60);
                }
                out.push(Schedule { key: sched::schedule_key(name), name: Some(name.clone()), path, method: method.to_owned(), expression, timezone, jitter: Some(jitter) });
            }
        }
        Some(_) => bail!("`schedules` must be an object of named schedules"),
    }
    if out.len() > sched::MAX_PER_PROJECT {
        bail!("{} schedules: a project may have at most {}, `crons` and `schedules` together", out.len(), sched::MAX_PER_PROJECT);
    }
    let mut keys = std::collections::BTreeSet::new();
    if let Some(s) = out.iter().find(|s| !keys.insert(s.key.as_str())) {
        bail!("schedule {:?} is listed twice", s.label());
    }
    Ok(out)
}

/// A schedule's `path`: on the deployment, starting with `/`, at most 512 characters.
fn schedule_path(value: Option<&Value>, what: &str) -> Result<String> {
    let Some(Value::String(path)) = value else { bail!("{what} needs a `path` (a string starting with /)") };
    if !path.starts_with('/') || path.starts_with("//") {
        bail!("{what}: `path` {path:?} must start with a single /");
    }
    if path.len() > sched::MAX_PATH {
        bail!("{what}: `path` is longer than {} characters", sched::MAX_PATH);
    }
    if path.parse::<hyper::http::uri::PathAndQuery>().is_err() || path.contains('#') {
        bail!("{what}: `path` {path:?} isn't a valid URL path");
    }
    Ok(path.clone())
}

/// A schedule's cron expression, checked.
fn expression(value: Option<&Value>, what: &str) -> Result<String> {
    let Some(Value::String(e)) = value else { bail!("{what} needs a `schedule`, a cron expression such as \"0 5 * * *\"") };
    let cron = Cron::parse(e).map_err(|err| anyhow::anyhow!("{what}: {err}"))?;
    if !cron.ever_runs() {
        bail!("{what}: {e:?} never runs (that day never comes in those months)");
    }
    Ok(e.clone())
}

/// `bulkRedirectsPath`, checked: a path inside the project.
fn bulk_path(top: &Map<String, Value>) -> Result<Option<String>> {
    match top.get("bulkRedirectsPath") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(p)) => {
            let p = p.trim_start_matches("./").trim_end_matches('/');
            if p.is_empty() || p.starts_with('/') || p.split('/').any(|s| s == "..") {
                bail!("`bulkRedirectsPath` must be a file or folder inside the project");
            }
            Ok(Some(p.to_owned()))
        }
        Some(_) => bail!("`bulkRedirectsPath` must be a path"),
    }
}

fn read_spec(dir: &Path, preset: Option<&str>) -> Result<Spec> {
    let mut spec = find_spec(dir, preset)?;
    // The version of its language the function runs: a static site has no function, and Go and
    // Rust programs run on Amazon Linux whatever their version.
    if !spec.static_only() {
        spec.version = match spec.language {
            Language::Python => Some(runtimes::python_version(dir, spec.python_entry())),
            Language::Node => Some(runtimes::node_version(dir)?),
            Language::Go | Language::Rust => None,
        };
        if let Some(warning) = spec.version.as_ref().and_then(|v| v.warning.clone()) {
            spec.warnings.push(warning);
        }
    }
    Ok(spec)
}

fn find_spec(dir: &Path, preset: Option<&str>) -> Result<Spec> {
    let exists = |rel: &str| dir.join(rel).is_file();
    let (name, config, order) = match load_config_ordered(dir, true)? {
        Some((name, config, order)) => (name, Some(config), order),
        None => ("vercel.json", None, None),
    };
    let path = std::path::PathBuf::from(name);
    let top = config.as_ref().and_then(Value::as_object);
    // The preset: `vercel.json`'s `framework` (null is "Other"), else the project's setting.
    let slug: Option<String> = match top.and_then(|t| t.get("framework")) {
        Some(Value::String(slug)) => Some(slug.clone()),
        Some(Value::Null) => None,
        Some(_) => bail!("{}: `framework` must be one of Vercel's framework names, or null", path.display()),
        None => preset.map(str::to_owned),
    };
    let backend_slug = slug.as_deref().filter(|s| backends::preset(s).is_some());
    // What finds frameworks that build (Nitro, static sites) sees the setting as `vercel.json`'s.
    let detect_with: Option<Value> = match (&config, preset) {
        (Some(c), _) if backend_slug.is_some() => Some(without_framework(c)),
        (Some(c), _) => Some(c.clone()),
        (None, Some(p)) if backend_slug.is_none() => Some(serde_json::json!({ "framework": p })),
        (None, _) => None,
    };
    let detect_with = match (&detect_with, preset, top.is_some_and(|t| t.contains_key("framework"))) {
        (Some(Value::Object(c)), Some(p), false) if backend_slug.is_none() => {
            let mut c = c.clone();
            c.insert("framework".to_owned(), Value::String(p.to_owned()));
            Some(Value::Object(c))
        }
        _ => detect_with,
    };
    // The functions in `api/`, every file one, as Vercel's zero-config detection finds them.
    let found = backends::discover_functions(dir);
    // One function, as forkrail has always run a project: `builds`; `functions` naming one file, or
    // forkrail's own `api/main.py` or `server.mjs` without `functions`, when it's the only function
    // of its language in `api/` (a project of Vercel's with one function and a route to it).
    let has_functions = top.is_some_and(|c| c.contains_key("functions"));
    let named = top
        .and_then(|t| t.get("functions"))
        .and_then(Value::as_object)
        .filter(|f| f.len() == 1)
        .and_then(|f| f.keys().next())
        .map(|k| k.trim_start_matches("./").to_owned())
        .filter(|k| !k.starts_with('/') && !k.contains(['*', '?', '[', '{']) && exists(k));
    let legacy = match named {
        Some(entry) => Some(entry),
        None if !has_functions => ["api/main.py", "server.mjs"].iter().find(|e| exists(e)).map(|e| (*e).to_owned()),
        None => None,
    }
    // What that has always taken: Python and JavaScript, not TypeScript.
    .filter(|e| Language::of(e).is_ok());
    // A Next.js project's `functions` name its routes' source files, not a function of forkrail's.
    let next = backend_slug.is_none() && matches!(framework::detect(dir, detect_with.as_ref()), Ok(Some(f)) if f.next());
    let one_function = top.is_some_and(|c| c.contains_key("builds")) || (!next && legacy.as_deref().is_some_and(|e| found.alone(e)));
    // `builds` and `functions` name a function explicitly; otherwise a framework may build the app.
    let explicit = one_function || (!next && has_functions && found.is_empty());
    let api_entry = ["api/main.py", "server.mjs"].iter().any(|e| exists(e));
    let mut site = None;
    if !explicit && backend_slug.is_none() && let Some(framework) = framework::detect(dir, detect_with.as_ref())? {
        if framework.server() {
            if !found.is_empty() {
                bail!(
                    "{}: functions in api/ ({}) beside a {} server aren't supported yet: forkrail runs one or the other in a project so far",
                    path.display(),
                    found.files().join(", "),
                    framework.name
                );
            }
            return framework_spec(framework, config).with_context(|| format!("{}", path.display()));
        }
        site = Some(framework);
    }
    // Functions in `api/`, every file its own, as Vercel builds them beside whatever site the
    // project has: its framework's, or its own folder ("Other").
    if !explicit && !found.is_empty() && per_file(dir, &found, backend_slug, site.is_some(), top.and_then(|t| t.get("framework")).is_some_and(Value::is_null)).with_context(|| format!("{}", path.display()))? {
        let backend = found.one_language(dir).with_context(|| format!("{}", path.display()))?.map(backends::file_functions_backend).expect("functions were found");
        let site = match site {
            Some(site) => site,
            None => framework::other(dir, detect_with.as_ref())?,
        };
        return functions_spec(dir, backend, site, config, order).with_context(|| format!("{}", path.display()));
    }
    // A backend preset: chosen, or found (a Python app wins over a static site beside it, as
    // `api/main.py` always has; a Node server doesn't).
    if !explicit && (backend_slug.is_some() || slug.is_none()) {
        let found = backends::detect(dir, backend_slug).with_context(|| format!("{}", path.display()))?;
        if let Some(backend) = found.filter(|b| site.is_none() || b.language() == Language::Python) {
            return backend_spec(dir, backend, config).with_context(|| format!("{}", path.display()));
        }
    }
    if let Some(framework) = site
        && !api_entry
    {
        return static_spec(framework, config).with_context(|| format!("{}", path.display()));
    }
    if explicit || api_entry {
        let mut spec = match config {
            Some(config) => parse_config(config, exists).with_context(|| format!("{}", path.display()))?,
            None => detect(exists)?,
        };
        if dir.join("public").is_dir() {
            spec.public = Some("public".to_owned());
        }
        return Ok(spec);
    }
    static_spec(framework::other(dir, detect_with.as_ref())?, config).with_context(|| format!("{}", path.display()))
}

/// A copy of `vercel.json` without `framework`, for detection that a backend's slug would confuse.
fn without_framework(config: &Value) -> Value {
    let mut c = config.clone();
    if let Some(top) = c.as_object_mut() {
        top.remove("framework");
    }
    c
}

/// Whether a project with functions in `api/` (`found`) serves them, each file its own, rather than
/// a backend preset's one server. As Vercel decides by the project's framework setting:
/// - a Python preset chosen (FastAPI, Flask, Django, Python) serves the app and turns per-file
///   Python functions off; the Node.js preset does the same for Node's. A function of another
///   language beside it is refused (forkrail runs one language a project so far), as are Node
///   functions beside another Node server (Express, Hono, ...), which Vercel runs side by side;
/// - "Other" (`"framework": null`), a static site's framework, or Rust: per-file functions;
/// - nothing chosen: a static site found, or no backend found, means per-file functions. A Python
///   app found from the project's dependencies is the preset, as Vercel's import picks it, unless
///   its entry is in `api/` beside other Python functions (a layout made for per-file functions);
///   a Node server (Express, ...) found beside Node functions is refused, as above.
fn per_file(dir: &Path, found: &backends::Found, slug: Option<&str>, site: bool, other: bool) -> Result<bool> {
    let not_yet = |server: &str, language: Language| -> Result<bool> {
        let files = found.of(language);
        if files.is_empty() {
            return Ok(false);
        }
        bail!("{} functions in api/ ({}) beside the {server} server aren't supported yet: forkrail runs one or the other in a project so far", language.name(), files.join(", "))
    };
    let others = |served: Language| -> Vec<String> {
        [Language::Node, Language::Python, Language::Go, Language::Rust].into_iter().filter(|l| *l != served).flat_map(|l| found.of(l)).collect()
    };
    let chosen = slug.and_then(backends::preset);
    if let Some(preset) = chosen {
        if preset.slug == "rust" {
            return Ok(true);
        }
        // The preset's own language's functions are off; any other's would need a second runtime.
        let rest = others(preset.language);
        if !rest.is_empty() {
            bail!("functions in api/ ({}) beside the {} server aren't supported yet: forkrail runs one language a project so far", rest.join(", "), preset.name);
        }
        if preset.language == Language::Node && preset.slug != "node" {
            return not_yet(preset.name, Language::Node);
        }
        return Ok(false);
    }
    // Go and Rust functions alone: no Python or Node preset could take them.
    if other || site || (found.python.is_empty() && found.node.is_empty()) {
        return Ok(true);
    }
    // Only whether a Python or Node preset would serve the project instead: one that can't (a Go
    // or Rust server that isn't there) doesn't.
    match backends::detect(dir, None).unwrap_or(None) {
        Some(backend) if backend.language() == Language::Python && backend.files.is_none() => {
            let entry = backend.entry.split(':').next().unwrap_or(&backend.entry).to_owned();
            let layout = entry.starts_with("api/") && found.python.len() > 1 && found.python.iter().any(|f| f.file == entry);
            if !layout && !others(Language::Python).is_empty() {
                bail!("functions in api/ ({}) beside the {} app ({entry}) aren't supported yet: forkrail runs one language a project so far; set \"framework\": null in vercel.json to serve each file in api/ as its own function", others(Language::Python).join(", "), backend.preset.name);
            }
            Ok(layout)
        }
        Some(backend) if backend.language() == Language::Node && backend.preset.slug != "node" => not_yet(backend.preset.name, Language::Node),
        // The Node.js preset (a `server.*`) serves everything, as Vercel's, which turns per-file Node
        // functions off.
        Some(backend) if backend.language() == Language::Node => Ok(found.node.is_empty()),
        _ => Ok(true),
    }
}

/// The most a maxDuration may be, and what `"max"` asks for: Lambda's limit.
const MAX_DURATION_S: u64 = 900;

/// The runtimes `functions.*.runtime` may name: each language's own, which forkrail runs anyway.
const OWN_RUNTIMES: [(&str, Language); 5] =
    [("@vercel/node", Language::Node), ("@vercel/python", Language::Python), ("@vercel/go", Language::Go), ("@vercel/rust", Language::Rust), ("vercel-rust", Language::Rust)];

/// Per-file functions beside a site. `functions` sets each one's memory, maxDuration, includeFiles
/// and excludeFiles by pattern (`api/*.ts`, `api/**/*.py`), the first pattern that matches a file
/// winning, as on Vercel; a Node file's own `export const config` may set its maxDuration. They share
/// one Lambda function, which gets the largest memory and maxDuration any asks for (`Spec::floor`
/// when some ask for none); each one's own maxDuration is the router's limit for its routes.
fn functions_spec(dir: &Path, backend: Backend, mut site: Framework, config: Option<Value>, order: Option<Vec<String>>) -> Result<Spec> {
    let top = config.as_ref().and_then(Value::as_object);
    let files = backend.files.as_ref().map(|f| f.files()).unwrap_or_default();
    let language = backend.language();
    let mut concurrent = None;
    let mut ignore_command = None;
    let mut warnings = Vec::new();
    let mut settings: Vec<FunctionSettings> = files.iter().map(|f| FunctionSettings { file: f.clone(), ..Default::default() }).collect();
    let mut taken = vec![false; files.len()];
    if let Some(top) = top {
        check_keys(top, Kind::Functions, &format!("{} functions", backend.preset.name))?;
        ignore_command = git_keys(top)?;
        concurrent = match top.get("fluid") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => bail!("`fluid` must be true or false"),
        };
        if let Some(functions) = top.get("functions") {
            let Some(functions) = functions.as_object() else { bail!("`functions` must be an object") };
            if functions.len() > 50 {
                bail!("`functions` has {} patterns; Vercel allows at most 50", functions.len());
            }
            // In the order they're written: the first that matches a file is the one it gets.
            let mut patterns: Vec<(&String, &Value)> = functions.iter().collect();
            if let Some(order) = &order {
                patterns.sort_by_key(|(k, _)| order.iter().position(|o| o == *k).unwrap_or(usize::MAX));
            }
            for (pattern, value) in patterns {
                if pattern.starts_with('/') {
                    bail!("functions: the pattern {pattern:?} must not start with / (patterns are relative to the project, as on Vercel)");
                }
                if pattern.len() > 256 {
                    bail!("functions: the pattern {pattern:?} is longer than 256 characters");
                }
                let Some(value) = value.as_object().filter(|v| !v.is_empty()) else { bail!("functions.{pattern} must be an object with at least one setting") };
                for (key, v) in value {
                    match key.as_str() {
                        "memory" | "maxDuration" | "includeFiles" | "excludeFiles" | "runtime" => {}
                        "architecture" if v == "arm64" => {}
                        "architecture" => bail!("functions.{pattern}.architecture: forkrail runs functions on arm64"),
                        _ => bail!("functions.{pattern}.{key} isn't supported yet"),
                    }
                }
                if let Some(runtime) = value.get("runtime") {
                    let Some(runtime) = runtime.as_str() else { bail!("functions.{pattern}.runtime must be a string") };
                    let name = runtime.rsplit_once('@').map_or(runtime, |(n, _)| n);
                    if !OWN_RUNTIMES.iter().any(|(own, l)| *own == name && *l == language) {
                        bail!("functions.{pattern}.runtime: {runtime} isn't supported (forkrail runs {} functions with its own runtime; community runtimes aren't supported)", language.name());
                    }
                }
                let memory = value.get("memory").map(|m| int(m, &format!("functions.{pattern}.memory"), 128, 10240)).transpose()?.map(|m| m as u32);
                let duration = match value.get("maxDuration") {
                    None => None,
                    Some(Value::String(m)) if m == "max" => Some(MAX_DURATION_S),
                    Some(d) => Some(int(d, &format!("functions.{pattern}.maxDuration"), 1, MAX_DURATION_S)?),
                };
                let glob = |key: &str| -> Result<Option<String>> {
                    match value.get(key) {
                        None => Ok(None),
                        Some(Value::String(g)) if g.len() <= 256 => Ok(Some(g.trim_start_matches("./").to_owned())),
                        Some(_) => bail!("functions.{pattern}.{key} must be a glob of at most 256 characters, relative to the project"),
                    }
                };
                let (include_files, exclude_files) = (glob("includeFiles")?, glob("excludeFiles")?);
                let clean = pattern.trim_start_matches("./");
                let mut matched = false;
                for (i, file) in files.iter().enumerate() {
                    let hit = clean == file || crate::functions::glob_match(clean, file) || (language == Language::Python && file.strip_suffix(".py") == Some(clean));
                    if !hit {
                        continue;
                    }
                    matched = true;
                    // The first pattern that matches a file is the one it gets.
                    if !std::mem::replace(&mut taken[i], true) {
                        settings[i] = FunctionSettings { file: file.clone(), memory_mb: memory, max_duration_s: duration, include_files: include_files.clone(), exclude_files: exclude_files.clone() };
                    }
                }
                if !matched {
                    bail!("The pattern {pattern:?} defined in `functions` doesn't match any Serverless Functions inside the `api` directory ({})", files.join(", "));
                }
            }
        }
    }
    // A Node function's own `export const config = { maxDuration }`, where `functions` gave none.
    if language == Language::Node {
        for s in settings.iter_mut().filter(|s| s.max_duration_s.is_none()) {
            if let Ok(text) = std::fs::read_to_string(dir.join(&s.file))
                && let Some(duration) = crate::nodefns::file_max_duration(&text)
            {
                s.max_duration_s = Some(match duration {
                    crate::nodefns::ConfigDuration::Seconds(n) => n.clamp(1, MAX_DURATION_S),
                    crate::nodefns::ConfigDuration::Max => MAX_DURATION_S,
                });
            }
        }
        // Their packages are installed flat, so that the package can take only what they use.
        if top.is_none_or(|t| !t.contains_key("installCommand")) {
            site.install = framework::flat_install_command(dir);
        }
    }
    // The shared Lambda function's: the largest asked for, for all of them when all ask, else a floor
    // under the project's settings.
    let mut floor = crate::state::Floor::default();
    let memories: Vec<u32> = settings.iter().filter_map(|s| s.memory_mb).collect();
    let durations: Vec<u64> = settings.iter().filter_map(|s| s.max_duration_s).collect();
    let (mut memory_mb, mut max_duration_s) = (None, None);
    if let Some(m) = memories.iter().max().copied() {
        if memories.len() == settings.len() {
            memory_mb = Some(m);
        } else {
            floor.memory_mb = Some(m);
        }
        if memories.iter().any(|x| *x != m) || memories.len() != settings.len() {
            warnings.push(format!("the functions share one Lambda function here, which gets the largest memory any asks for ({m} MB) or the project's, if larger"));
        }
    }
    if let Some(d) = durations.iter().max().copied() {
        if durations.len() == settings.len() {
            max_duration_s = Some(d);
        } else {
            floor.max_duration_s = Some(d);
        }
    }
    let function = rules::Function { entry: None, catch_all: false, files: files.clone() };
    let mut routing = routing(top, &function, Some(&site))?;
    // Each function's own maxDuration, the router's limit for the requests its routes reach.
    if let Some(program) = routing.as_mut() {
        for s in &settings {
            if let Some(d) = s.max_duration_s {
                for path in rules::file_paths(&s.file) {
                    program.max_duration.insert(path, d);
                }
            }
        }
    }
    Ok(Spec {
        entry: backend.entry.clone(),
        language,
        memory_mb,
        max_duration_s,
        concurrent,
        warnings,
        config,
        framework: Some(site),
        ignore_command,
        routing,
        public: None,
        bulk_redirects: None,
        schedules: Vec::new(),
        backend: Some(backend),
        version: None,
        functions: settings,
        floor: (floor != crate::state::Floor::default()).then_some(floor),
    })
}

/// A backend preset's server: it takes every path, after the project's `public/` files.
fn backend_spec(dir: &Path, backend: Backend, config: Option<Value>) -> Result<Spec> {
    let top = config.as_ref().and_then(Value::as_object);
    let mut concurrent = None;
    let mut ignore_command = None;
    let (mut memory_mb, mut max_duration_s) = (None, None);
    if let Some(top) = top {
        check_keys(top, Kind::Backend, backend.preset.name)?;
        ignore_command = git_keys(top)?;
        concurrent = match top.get("fluid") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => bail!("`fluid` must be true or false"),
        };
        if backend.language() != Language::Node {
            for key in ["installCommand", "buildCommand"] {
                if top.contains_key(key) {
                    bail!("`{key}` isn't supported for {} yet", backend.preset.name);
                }
            }
        }
        if top.contains_key("builds") {
            bail!("`builds` names its own entry: remove it to use the {} preset, or remove `framework`", backend.preset.name);
        }
        // `functions` may set the server's memory and maxDuration, under its entry's file.
        if let Some(functions) = top.get("functions") {
            let (entry, memory, max_duration) = entry_from_functions(functions)?;
            let file = backend.entry.split(':').next().unwrap_or(&backend.entry);
            if entry != file {
                bail!("`functions` names {entry}, but the {} server's entry is {file}", backend.preset.name);
            }
            (memory_mb, max_duration_s) = (memory, max_duration);
        }
    }
    let routing = routing(top, &rules::Function { entry: None, catch_all: true, ..Default::default() }, None)?;
    let public = dir.join("public").is_dir().then(|| "public".to_owned());
    Ok(Spec {
        entry: backend.entry.clone(),
        language: backend.language(),
        memory_mb,
        max_duration_s,
        concurrent,
        warnings: Vec::new(),
        config,
        framework: None,
        ignore_command,
        routing,
        public,
        bulk_redirects: None,
        schedules: Vec::new(),
        backend: Some(backend),
        version: None,
        functions: Vec::new(),
        floor: None,
    })
}

/// Vercel's limit on `ignoreCommand`.
const MAX_IGNORE_COMMAND: usize = 256;

/// `ignoreCommand`, once `git` and it are checked for shape.
fn git_keys(top: &Map<String, Value>) -> Result<Option<String>> {
    if let Some(git) = top.get("git") {
        let Some(git) = git.as_object() else { bail!("`git` must be an object") };
        if let Some(key) = git.keys().find(|k| *k != "deploymentEnabled") {
            bail!("`git.{key}` isn't supported yet (forkrail understands git.deploymentEnabled)");
        }
        match git.get("deploymentEnabled") {
            None | Some(Value::Bool(_)) => {}
            Some(Value::Object(branches)) if branches.values().all(Value::is_boolean) => {}
            Some(_) => bail!("`git.deploymentEnabled` must be true, false, or branches (or patterns like \"internal-*\") set to true or false"),
        }
    }
    match top.get("ignoreCommand") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(command)) if command.len() <= MAX_IGNORE_COMMAND => Ok(Some(command.trim().to_owned()).filter(|c| !c.is_empty())),
        Some(Value::String(_)) => bail!("`ignoreCommand` is longer than {MAX_IGNORE_COMMAND} characters, Vercel's limit"),
        Some(_) => bail!("`ignoreCommand` must be a command, as text"),
    }
}

/// Whether a push to `branch` deploys, by the project's `vercel.json` (`config`): not when
/// `git.deploymentEnabled` is false, or names the branch (or a pattern it matches) as false and
/// no pattern it matches as true, as on Vercel. The older `github.enabled: false` turns every
/// branch off too.
pub fn deploys_branch(config: Option<&Value>, branch: &str) -> bool {
    let Some(top) = config.and_then(Value::as_object) else { return true };
    if top.get("github").and_then(|g| g.get("enabled")) == Some(&Value::Bool(false)) {
        return false;
    }
    match top.get("git").and_then(|g| g.get("deploymentEnabled")) {
        Some(Value::Bool(enabled)) => *enabled,
        Some(Value::Object(branches)) => {
            let matching: Vec<bool> = branches.iter().filter(|(pattern, _)| glob(pattern, branch)).filter_map(|(_, on)| on.as_bool()).collect();
            matching.is_empty() || matching.contains(&true)
        }
        _ => true,
    }
}

/// Whether a push to `branch` deploys the project in `dir`. A `vercel.json` that can't be read
/// says nothing here: the build reads it again, and fails with the reason in its log. Nor does a
/// `vercel.ts`: it's never run here (`load_static_config`), so its pushes all deploy.
pub fn push_deploys(dir: &Path, branch: &str) -> bool {
    let config = load_static_config(dir).ok().flatten().map(|(_, c)| c);
    deploys_branch(config.as_ref(), branch)
}

/// What a project's `github` keys say about pushes, with Vercel's defaults for what they don't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GithubKeys {
    /// `github.autoAlias`: the production branch's deployments become production. When false
    /// they're built as production deployments but leave production where it is.
    pub auto_alias: bool,
    /// `github.autoJobCancelation`: a branch's newer push cancels its older builds still running.
    pub auto_job_cancelation: bool,
}

impl GithubKeys {
    pub fn of(config: Option<&Value>) -> GithubKeys {
        let github = config.and_then(|c| c.get("github"));
        let flag = |k: &str| github.and_then(|g| g.get(k)).and_then(Value::as_bool).unwrap_or(true);
        GithubKeys { auto_alias: flag("autoAlias"), auto_job_cancelation: flag("autoJobCancelation") }
    }
}

/// The `github` keys of the project in `dir`; Vercel's defaults if its configuration can't be read.
pub fn github_keys(dir: &Path) -> GithubKeys {
    GithubKeys::of(load_static_config(dir).ok().flatten().map(|(_, c)| c).as_ref())
}

/// Whether `text` matches `pattern`, where `*` is any run of characters but `/`, `**` any run at
/// all, and `?` one character but `/`: enough of minimatch for branch names.
fn glob(pattern: &str, text: &str) -> bool {
    fn at(p: &[u8], t: &[u8]) -> bool {
        match p {
            [] => t.is_empty(),
            [b'*', b'*', rest @ ..] => (0..=t.len()).any(|i| at(rest, &t[i..])),
            [b'*', rest @ ..] => (0..=t.len()).take_while(|i| *i == 0 || t[i - 1] != b'/').any(|i| at(rest, &t[i..])),
            [b'?', rest @ ..] => t.first().is_some_and(|c| *c != b'/') && at(rest, &t[1..]),
            [c, rest @ ..] => t.first() == Some(c) && at(rest, &t[1..]),
        }
    }
    at(pattern.as_bytes(), text.as_bytes())
}

fn framework_spec(framework: Framework, config: Option<Value>) -> Result<Spec> {
    let mut concurrent = None;
    let mut ignore_command = None;
    let top = config.as_ref().and_then(Value::as_object);
    if let Some(top) = top {
        check_keys(top, if framework.next() { Kind::Next } else { Kind::Server }, framework.name)?;
        ignore_command = git_keys(top)?;
        concurrent = match top.get("fluid") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => bail!("`fluid` must be true or false"),
        };
    }
    // A Build Output's routes come with it, after the build (build_output.rs); its functions are
    // served by the dispatcher the import writes.
    let (routing, entry) = if framework.build_output() {
        (None, crate::build_output::DISPATCHER.to_owned())
    } else {
        (routing(top, &rules::Function { entry: None, catch_all: true, ..Default::default() }, Some(&framework))?, framework.entry.to_owned())
    };
    Ok(Spec {
        entry,
        language: Language::Node,
        memory_mb: None,
        max_duration_s: None,
        concurrent,
        warnings: Vec::new(),
        config,
        framework: Some(framework),
        ignore_command,
        routing,
        public: None,
        bulk_redirects: None,
        schedules: Vec::new(),
        backend: None,
        version: None,
        functions: Vec::new(),
        floor: None,
    })
}

/// A framework's spec, once its build made a server of its own rather than the Build Output it
/// was expected to (Nitro's `node-server` preset, `Framework::nitro_server`): as `framework_spec`
/// makes one for such a server, its routes around it.
pub fn use_server(spec: &mut Spec, framework: Framework) -> Result<()> {
    let top = spec.config.as_ref().and_then(Value::as_object);
    spec.routing = routing(top, &rules::Function { entry: None, catch_all: true, ..Default::default() }, Some(&framework))?;
    spec.entry = framework.entry.to_owned();
    spec.framework = Some(framework);
    Ok(())
}

/// A framework's spec, once its build made static files alone rather than the server it was
/// expected to (a Next.js static export, next_export.rs): a static site, with the routes the
/// framework's builder puts around its files (`routes`, in `routes` form) and its `.html` pages
/// served by their names without it, by `cleanUrls`' lookup but not its redirects (Vercel's Next
/// builder renames them).
pub fn use_static(spec: &mut Spec, framework: Framework, routes: &Value) -> Result<()> {
    let empty = Map::new();
    let top = spec.config.as_ref().and_then(Value::as_object).unwrap_or(&empty);
    let mut program = Program::from_vercel(top, &rules::Function::default(), Some((framework.name, routes))).map_err(anyhow::Error::msg)?.unwrap_or_default();
    program.clean_urls = true;
    spec.routing = Some(program);
    spec.entry = String::new();
    spec.framework = Some(framework);
    Ok(())
}

/// A static site: a framework's build output, or the folder itself.
fn static_spec(framework: Framework, config: Option<Value>) -> Result<Spec> {
    let top = config.as_ref().and_then(Value::as_object);
    let mut ignore_command = None;
    if let Some(top) = top {
        check_keys(top, Kind::Static, "a static site")?;
        ignore_command = git_keys(top)?;
    }
    let routing = routing(top, &rules::Function::default(), Some(&framework))?;
    Ok(Spec {
        entry: String::new(),
        language: Language::Node,
        memory_mb: None,
        max_duration_s: None,
        concurrent: None,
        warnings: Vec::new(),
        config,
        framework: Some(framework),
        ignore_command,
        routing,
        public: None,
        bulk_redirects: None,
        schedules: Vec::new(),
        backend: None,
        version: None,
        functions: Vec::new(),
        floor: None,
    })
}

/// No `vercel.json`: a FastAPI app laid out for Vercel, or a Node server.
pub fn detect(exists: impl Fn(&str) -> bool) -> Result<Spec> {
    for entry in ["api/main.py", "server.mjs"] {
        if exists(entry) {
            return Ok(Spec {
                entry: entry.to_owned(),
                language: Language::of(entry)?,
                memory_mb: None,
                max_duration_s: None,
                concurrent: None,
                warnings: Vec::new(),
                config: None,
                framework: None,
                ignore_command: None,
                routing: None,
                public: None,
                bulk_redirects: None,
                schedules: Vec::new(),
                backend: None,
                version: None,
                functions: Vec::new(),
                floor: None,
            });
        }
    }
    bail!("no vercel.json, and neither api/main.py nor server.mjs to serve")
}

pub fn parse(text: &str, exists: impl Fn(&str) -> bool) -> Result<Spec> {
    let mut spec = parse_config(serde_json::from_str(text).context("not valid JSON")?, exists)?;
    if let Some(top) = spec.config.as_ref().and_then(Value::as_object) {
        spec.schedules = schedules(top)?;
    }
    Ok(spec)
}

fn parse_config(config: Value, exists: impl Fn(&str) -> bool) -> Result<Spec> {
    let Some(top) = config.as_object() else { bail!("expected a JSON object") };
    check_keys(top, Kind::Api, "an API")?;
    let ignore_command = git_keys(top)?;
    let concurrent = match top.get("fluid") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => bail!("`fluid` must be true or false"),
    };
    let mut warnings = Vec::new();
    let (entry, memory_mb, max_duration_s) = match (top.get("builds"), top.get("functions")) {
        (Some(_), Some(_)) => bail!("`builds` and `functions` can't be used together (Vercel refuses that too)"),
        (Some(builds), None) => (entry_from_builds(builds, &mut warnings)?, None, None),
        (None, Some(functions)) => entry_from_functions(functions)?,
        (None, None) => {
            let spec = detect(&exists)?;
            (spec.entry, None, None)
        }
    };
    if !exists(&entry) {
        bail!("the entry {entry} doesn't exist");
    }
    let language = Language::of(&entry)?;
    let routing = routing(Some(top), &rules::Function { entry: Some(entry.clone()), catch_all: false, ..Default::default() }, None)?;
    if !top.contains_key("routes") && !top.contains_key("rewrites") {
        warnings.push(format!(
            "no catch-all route: forkrail sends every path to {entry}, where Vercel would serve it only at /{}",
            entry.rsplit_once('.').map_or(entry.as_str(), |(stem, _)| stem)
        ));
    }
    Ok(Spec { entry, language, memory_mb, max_duration_s, concurrent, warnings, config: Some(config), framework: None, ignore_command, routing, public: None, bulk_redirects: None, schedules: Vec::new(), backend: None, version: None, functions: Vec::new(), floor: None })
}

/// The memory, maxDuration and `fluid` a `vercel.json` that was accepted when a deployment was built
/// asks for, from the copy its record keeps: for publishing that build again.
pub fn settings_of(config: Option<&Value>) -> (Option<u32>, Option<u64>, Option<bool>) {
    let Some(top) = config.and_then(Value::as_object) else { return (None, None, None) };
    let concurrent = top.get("fluid").and_then(Value::as_bool);
    let function = top.get("functions").and_then(Value::as_object).filter(|f| f.len() == 1).and_then(|f| f.values().next()).and_then(Value::as_object);
    let memory = function.and_then(|f| f.get("memory")).and_then(Value::as_u64).and_then(|m| u32::try_from(m).ok());
    let max_duration = function.and_then(|f| f.get("maxDuration")).and_then(Value::as_u64);
    (memory, max_duration, concurrent)
}

/// `vercel.json`'s `regions`: the regions the deployment's functions run in, by AWS's names, in
/// the order given and each once. Vercel's names (`iad1`, `fra1`) and AWS's (`us-east-1`) are both
/// taken; one forkrail doesn't know, or an empty list, is refused, as Vercel refuses them. None
/// without the key.
pub fn regions(config: Option<&Value>) -> Result<Option<Vec<String>>> {
    let Some(value) = config.and_then(Value::as_object).and_then(|top| top.get("regions")) else { return Ok(None) };
    let Some(list) = value.as_array() else { bail!("vercel.json's `regions` must be an array of region names, such as [\"iad1\"]") };
    let names: Vec<&str> = list.iter().map(|v| v.as_str().with_context(|| format!("vercel.json's `regions`: {v} isn't a region name"))).collect::<Result<_>>()?;
    crate::regions::parse(&names).map(Some).map_err(|e| anyhow::anyhow!("vercel.json's `regions`: {e:#}"))
}

fn entry_from_builds(builds: &Value, warnings: &mut Vec<String>) -> Result<String> {
    let Some(builds) = builds.as_array() else { bail!("`builds` must be an array") };
    let [build] = builds.as_slice() else {
        bail!("`builds` has {} entries; forkrail deploys one function per project so far", builds.len())
    };
    let Some(build) = build.as_object() else { bail!("`builds[0]` must be an object") };
    let src = file_path(build.get("src"), "builds[0].src")?;
    let wanted = match Language::of(&src)? {
        Language::Python => "@vercel/python",
        Language::Node | Language::Go | Language::Rust => "@vercel/node",
    };
    match build.get("use").and_then(Value::as_str) {
        None => {}
        Some(builder) if builder == wanted || builder.starts_with(&format!("{wanted}@")) => {}
        Some(builder) => bail!("builds[0].use is {builder}; for {src} forkrail builds as {wanted}"),
    }
    if let Some(config) = build.get("config").and_then(Value::as_object) {
        for key in config.keys().filter(|k| *k != "maxLambdaSize") {
            warnings.push(format!("builds[0].config.{key} is ignored"));
        }
    }
    if let Some(key) = build.keys().find(|k| !["src", "use", "config"].contains(&k.as_str())) {
        bail!("builds[0].{key} isn't supported");
    }
    Ok(src)
}

fn entry_from_functions(functions: &Value) -> Result<(String, Option<u32>, Option<u64>)> {
    let Some(functions) = functions.as_object() else { bail!("`functions` must be an object") };
    let mut entries = functions.iter();
    let (Some((path, settings)), None) = (entries.next(), entries.next()) else {
        bail!("`functions` has {} entries; forkrail deploys one function per project so far", functions.len())
    };
    let entry = file_path(Some(&Value::String(path.clone())), "functions")?;
    let empty = Map::new();
    let settings = match settings {
        Value::Object(settings) => settings,
        Value::Null => &empty,
        _ => bail!("functions.{path} must be an object"),
    };
    if let Some(key) = settings.keys().find(|k| !FUNCTION_KEYS.contains(&k.as_str())) {
        bail!("functions.{path}.{key} isn't supported yet");
    }
    let memory = settings.get("memory").map(|m| int(m, &format!("functions.{path}.memory"), 128, 10240)).transpose()?;
    let max_duration =
        settings.get("maxDuration").map(|d| int(d, &format!("functions.{path}.maxDuration"), 1, 900)).transpose()?;
    Ok((entry, memory.map(|m| m as u32), max_duration))
}

/// A relative file path, not a pattern.
fn file_path(value: Option<&Value>, what: &str) -> Result<String> {
    let Some(path) = value.and_then(Value::as_str) else { bail!("{what} must be a file path") };
    let path = path.trim_start_matches("./").trim_start_matches('/');
    if path.is_empty() || path.contains(['*', '?', '[', '{']) || path.split('/').any(|p| p == "..") {
        bail!("{what}: {path:?} must name one file in the project; patterns aren't supported yet");
    }
    Ok(path.to_owned())
}

fn int(value: &Value, what: &str, min: u64, max: u64) -> Result<u64> {
    match value.as_u64() {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        _ => bail!("{what} must be a whole number from {min} to {max}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(files: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |f| files.contains(&f)
    }

    const API: &[&str] = &["api/main.py", "requirements.txt"];

    fn project(files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-spec-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    #[test]
    fn a_next_project_is_its_own_server() {
        let app = project(&[("package.json", r#"{"scripts": {"build": "next build"}, "dependencies": {"next": "16.4.6"}, "engines": {"node": "22.x"}}"#)]);
        let spec = read(&app).unwrap();
        assert_eq!((spec.entry.as_str(), spec.language, spec.runtime().as_str()), (crate::next::ENTRY, Language::Node, "nodejs22.x"));
        assert!(!spec.static_only() && spec.framework.as_ref().unwrap().next());
        assert_eq!(spec.describe(), "Next.js (Node 22)");
        // Requests go to the files first, then Next's server: no routing program of forkrail's own.
        assert_eq!(spec.routing, None);
        // `functions` names its routes' source files (next.rs reads their settings): the project
        // stays a Next app, though the one file named is there.
        let settings = project(&[
            ("package.json", r#"{"dependencies": {"next": "16.4.6"}}"#),
            ("pages/api/slow.js", "export default function handler() {}"),
            ("vercel.json", r#"{"functions": {"pages/api/slow.js": {"maxDuration": 60, "memory": 1024}}, "crons": [{"path": "/api/slow", "schedule": "0 5 * * *"}], "regions": ["iad1"]}"#),
        ]);
        let spec = read(&settings).unwrap();
        assert_eq!((spec.framework.as_ref().map(|f| f.name), spec.entry.as_str(), spec.schedules.len()), (Some("Next.js"), crate::next::ENTRY, 1));
        // The project's setting picks it too; `"framework": null` in vercel.json still wins.
        let named = project(&[("package.json", r#"{"scripts": {"build": "next build"}}"#)]);
        assert_eq!(read_as(&named, Some("nextjs")).unwrap().framework.unwrap().name, "Next.js");
        // `outputDirectory` is accepted, as on Vercel: the build reads Next's own folder anyway.
        let named_dir = project(&[("package.json", r#"{"dependencies": {"next": "16"}}"#), ("vercel.json", r#"{"outputDirectory": "apps/.next"}"#)]);
        assert_eq!(read(&named_dir).unwrap().framework.map(|f| f.name), Some("Next.js"));
    }

    #[test]
    fn presets_from_the_project_and_its_vercel_json() {
        // Found: a Flask app, on Python 3.13 by default.
        let flask = project(&[("requirements.txt", "flask\n"), ("app.py", "app = Flask(__name__)\n")]);
        let spec = read(&flask).unwrap();
        assert_eq!((spec.entry.as_str(), spec.language, spec.runtime(), spec.backend.as_ref().map(|b| b.preset.slug)), ("app.py", Language::Python, "python3.13".to_owned(), Some("flask")));
        assert_eq!(spec.describe(), "app.py (Flask, Python 3.13)");
        std::fs::write(flask.join(".python-version"), "3.12\n").unwrap();
        assert_eq!(read(&flask).unwrap().describe(), "app.py (Flask, Python 3.12)");
        // The project's setting picks a preset the files alone wouldn't say...
        let plain = project(&[("requirements.txt", "falcon\n"), ("server.py", "application = make()\n")]);
        assert_eq!(read_as(&plain, Some("flask")).unwrap().backend.unwrap().preset.slug, "flask");
        // ...and vercel.json's `framework` wins over the setting.
        let both = project(&[("requirements.txt", "flask\n"), ("app.py", "app = 1\n"), ("vercel.json", r#"{"framework": "fastapi"}"#)]);
        assert_eq!(read_as(&both, Some("flask")).unwrap().backend.unwrap().preset.slug, "fastapi");
        // A Node backend's version from engines.node, and a range Lambda can't run fails.
        let hono = project(&[("package.json", r#"{"engines": {"node": "22.x"}, "dependencies": {"hono": "4"}}"#), ("src/index.ts", "import { Hono } from 'hono'\n")]);
        assert_eq!(read(&hono).unwrap().runtime(), "nodejs22.x");
        let old = project(&[("package.json", r#"{"engines": {"node": "18.x"}, "dependencies": {"hono": "4"}}"#), ("src/index.ts", "import { Hono } from 'hono'\n")]);
        assert!(format!("{:#}", read(&old).unwrap_err()).contains("end of life"));
        // A static preset chosen in the settings still builds the site.
        let site = project(&[("package.json", r#"{"scripts": {"build": "vite build"}}"#)]);
        assert_eq!(read_as(&site, Some("vite")).unwrap().framework.unwrap().name, "Vite");
    }

    #[test]
    fn go_functions_beside_a_site() {
        let dir = project(&[
            ("package.json", r#"{"scripts": {"build": "vite build"}, "devDependencies": {"vite": "7"}}"#),
            ("go.mod", "module shop\n\ngo 1.25\n"),
            ("api/index.go", "package handler\n\nimport \"net/http\"\n\nfunc Handler(w http.ResponseWriter, r *http.Request) {}\n"),
            ("api/[id].go", "package handler\n\nimport \"net/http\"\n\nfunc Item(w http.ResponseWriter, r *http.Request) {}\n"),
        ]);
        let spec = read(&dir).unwrap();
        assert!(!spec.static_only());
        assert_eq!((spec.language, spec.entry.as_str(), spec.runtime()), (Language::Go, crate::functions::MANIFEST, "provided.al2023".to_owned()));
        assert_eq!(spec.framework.as_ref().map(|f| f.name), Some("Vite"));
        assert_eq!(spec.file_functions().unwrap().files(), ["api/[id].go", "api/index.go"]);
        let program = spec.routing.unwrap();
        assert!(program.function.contains(&"/api".to_owned()) && program.function.contains(&"/api/[id]".to_owned()));
        assert!(!program.fallback);
        // Vercel's Go preset is a server, not functions.
        let server = project(&[("go.mod", "module x\n"), ("main.go", "package main\n\nfunc main() {}\n"), ("vercel.json", r#"{"framework": "go"}"#), ("api/index.go", "package handler\n")]);
        assert_eq!(read(&server).unwrap().entry, ".forkrail/x");
    }

    const FASTAPI_APP: &str = "from fastapi import FastAPI\napp = FastAPI()\n";
    const HANDLER: &str = "from http.server import BaseHTTPRequestHandler\n\nclass handler(BaseHTTPRequestHandler):\n    pass\n";
    const NODE_FN: &str = "export default function handler(req, res) { res.end('hi') }\n";

    /// Which projects run one function, as forkrail always has, and which serve every file in
    /// `api/` as its own.
    #[test]
    fn one_function_or_functions_in_api() {
        let kind = |files: &[(&str, &str)]| -> String {
            let spec = read(&project(files)).unwrap();
            match spec.file_functions() {
                Some(f) => format!("{} functions: {}", f.language().name(), f.files().join(" ")),
                None => format!("one: {}", spec.entry),
            }
        };
        // The live APIs: `functions` names api/main.py, routes send everything to it; JavaScript
        // files beside it (bayut's) don't make it several functions.
        let live = r#"{"version": 2, "functions": {"api/main.py": {"memory": 768}}, "routes": [{"src": "/(.*)", "dest": "api/main.py"}]}"#;
        assert_eq!(kind(&[("vercel.json", live), ("requirements.txt", "fastapi\n"), ("api/main.py", FASTAPI_APP), ("api/auth.js", NODE_FN), ("api/index.js", NODE_FN)]), "one: api/main.py");
        // forkrail's own api/main.py, and a helper module beside it (not a function).
        assert_eq!(kind(&[("requirements.txt", "fastapi\n"), ("api/main.py", FASTAPI_APP), ("api/helpers.py", "def f():\n    pass\n")]), "one: api/main.py");
        // Two Python functions are two functions.
        assert_eq!(kind(&[("api/main.py", FASTAPI_APP), ("api/other.py", HANDLER)]), "Python functions: api/main.py api/other.py");
        // `functions` naming one JavaScript file that's one of several in api/: Vercel's functions.
        assert_eq!(kind(&[("vercel.json", r#"{"functions": {"api/cron.js": {"maxDuration": 60}}}"#), ("api/cron.js", NODE_FN), ("api/hello.js", NODE_FN)]), "Node functions: api/cron.js api/hello.js");
        // TypeScript was never a single entry.
        assert_eq!(kind(&[("vercel.json", r#"{"functions": {"api/server.ts": {"maxDuration": 60}}}"#), ("api/server.ts", NODE_FN)]), "Node functions: api/server.ts");
        // A Python app found from the dependencies is the preset, as Vercel's import picks it...
        assert_eq!(kind(&[("requirements.txt", "fastapi\n"), ("api/index.py", FASTAPI_APP)]), "one: api/index.py");
        // ...unless api/ was laid out for functions, or vercel.json says "Other".
        assert_eq!(kind(&[("requirements.txt", "fastapi\n"), ("api/index.py", HANDLER), ("api/items/[id].py", FASTAPI_APP)]), "Python functions: api/index.py api/items/[id].py");
        assert_eq!(kind(&[("requirements.txt", "fastapi\n"), ("api/index.py", FASTAPI_APP), ("vercel.json", r#"{"framework": null}"#)]), "Python functions: api/index.py");
        // Beside a static site, a Python app in api/ is a function, and the site is built.
        let vite = r#"{"scripts": {"build": "vite build"}, "devDependencies": {"vite": "7"}}"#;
        assert_eq!(kind(&[("package.json", vite), ("requirements.txt", "fastapi\n"), ("api/index.py", FASTAPI_APP)]), "Python functions: api/index.py");
        assert_eq!(kind(&[("package.json", vite), ("api/hello.ts", NODE_FN), ("api/users/[id].ts", NODE_FN), ("api/_lib/db.ts", "")]), "Node functions: api/hello.ts api/users/[id].ts");
        // forkrail's own server.mjs serves everything, as Vercel's Node.js preset would.
        assert_eq!(kind(&[("server.mjs", "import http from 'node:http'\n"), ("api/hello.js", NODE_FN)]), "one: server.mjs");
        // `builds` names its one function.
        assert_eq!(kind(&[("vercel.json", r#"{"builds": [{"src": "api/main.py", "use": "@vercel/python"}]}"#), ("api/main.py", FASTAPI_APP), ("api/b.py", HANDLER)]), "one: api/main.py");
    }

    #[test]
    fn what_functions_in_api_refuse() {
        let error = |files: &[(&str, &str)]| format!("{:#}", read(&project(files)).unwrap_err());
        let mixed = error(&[("api/a.ts", NODE_FN), ("api/b.py", HANDLER)]);
        assert!(mixed.contains("more than one language") && mixed.contains("Node (api/a.ts)") && mixed.contains("Python (api/b.py)"), "{mixed}");
        let preset = error(&[("vercel.json", r#"{"framework": "fastapi"}"#), ("requirements.txt", "fastapi\n"), ("app.py", FASTAPI_APP), ("api/a.ts", NODE_FN)]);
        assert!(preset.contains("api/a.ts") && preset.contains("FastAPI"), "{preset}");
        let express = error(&[("package.json", r#"{"dependencies": {"express": "5"}}"#), ("index.js", "import express from 'express'\nexport default express()\n"), ("api/hello.ts", NODE_FN)]);
        assert!(express.contains("beside the Express server aren't supported yet"), "{express}");
        let unused = error(&[("vercel.json", r#"{"functions": {"api/*.js": {"memory": 512}, "api/**/*.py": {"memory": 512}}}"#), ("api/a.js", NODE_FN), ("api/b.js", NODE_FN)]);
        assert!(unused.contains(r#"The pattern "api/**/*.py" defined in `functions` doesn't match any Serverless Functions inside the `api` directory"#), "{unused}");
        let key = error(&[("vercel.json", r#"{"functions": {"api/*.js": {"regions": ["iad1"]}}}"#), ("api/a.js", NODE_FN)]);
        assert!(key.contains("functions.api/*.js.regions isn't supported yet"), "{key}");
        let runtime = error(&[("vercel.json", r#"{"functions": {"api/*.js": {"runtime": "vercel-php@0.7.4"}}}"#), ("api/a.js", NODE_FN)]);
        assert!(runtime.contains("community runtimes aren't supported"), "{runtime}");
        let names = error(&[("api/[user].ts", NODE_FN), ("api/[team]/[id].ts", NODE_FN)]);
        assert!(names.contains("[team] and [user]"), "{names}");
        let same = error(&[("api/users.ts", NODE_FN), ("api/users/index.js", NODE_FN)]);
        assert!(same.contains("same path"), "{same}");
    }

    /// `functions`: the first pattern that matches a file wins; the shared function gets the
    /// largest settings (a floor when some functions set none); each function's own maxDuration is
    /// the router's limit for its paths, from `functions` or the file's own `config`.
    #[test]
    fn functions_settings_by_pattern() {
        let dir = project(&[
            ("vercel.json", r#"{"functions": {"api/cron.ts": {"maxDuration": 30}, "api/**/*.ts": {"memory": 1024, "maxDuration": 10, "excludeFiles": "data/**"}, "api/users/*": {"memory": 3009}}}"#),
            ("api/cron.ts", NODE_FN),
            ("api/users/[id].ts", NODE_FN),
            ("api/slow.js", "export const config = { maxDuration: 'max' }\nexport default (req, res) => res.end()\n"),
            ("api/plain.js", NODE_FN),
        ]);
        let spec = read(&dir).unwrap();
        let by_file = |f: &str| spec.functions.iter().find(|s| s.file == f).cloned().unwrap();
        assert_eq!((by_file("api/cron.ts").memory_mb, by_file("api/cron.ts").max_duration_s), (None, Some(30)), "the first pattern, alone");
        assert_eq!((by_file("api/users/[id].ts").memory_mb, by_file("api/users/[id].ts").max_duration_s), (Some(1024), Some(10)), "not merged with a later one");
        assert_eq!(by_file("api/users/[id].ts").exclude_files.as_deref(), Some("data/**"));
        assert_eq!(by_file("api/slow.js").max_duration_s, Some(900), "from the file's config");
        assert_eq!(by_file("api/plain.js").max_duration_s, None);
        // Not all set them: a floor under the project's settings, not the deployment's own.
        assert_eq!((spec.memory_mb, spec.max_duration_s), (None, None));
        assert_eq!(spec.floor, Some(crate::state::Floor { memory_mb: Some(1024), max_duration_s: Some(900) }));
        let limits = &spec.routing.as_ref().unwrap().max_duration;
        assert_eq!((limits.get("/api/cron"), limits.get("/api/cron.ts"), limits.get("/api/users/[id]"), limits.get("/api/slow"), limits.get("/api/plain")), (Some(&30), Some(&30), Some(&10), Some(&900), None));
        // All set them: the largest is the deployment's.
        let all = project(&[("vercel.json", r#"{"functions": {"api/*.js": {"memory": 512, "maxDuration": 20}, "api/b.js": {"memory": 2048}}}"#), ("api/a.js", NODE_FN), ("api/b.js", NODE_FN)]);
        let spec = read(&all).unwrap();
        assert_eq!((spec.memory_mb, spec.max_duration_s, spec.floor), (Some(512), Some(20), None));
        // Python's patterns may leave out the extension, as Vercel's Python builder allows.
        let py = project(&[("vercel.json", r#"{"functions": {"api/index": {"maxDuration": 15}}}"#), ("api/index.py", HANDLER), ("api/other.py", HANDLER)]);
        assert_eq!(read(&py).unwrap().functions[0].max_duration_s, Some(15));
    }

    #[test]
    fn settings_from_a_recorded_config() {
        let config: Value = serde_json::from_str(r#"{"fluid": false, "functions": {"api/main.py": {"memory": 512, "maxDuration": 60}}}"#).unwrap();
        assert_eq!(settings_of(Some(&config)), (Some(512), Some(60), Some(false)));
        let builds: Value = serde_json::from_str(r#"{"builds": [{"src": "api/main.py", "use": "@vercel/python"}]}"#).unwrap();
        assert_eq!(settings_of(Some(&builds)), (None, None, None));
        assert_eq!(settings_of(None), (None, None, None));
        assert_eq!((Language::from_runtime("python3.13"), Language::from_runtime("nodejs24.x"), Language::from_runtime("java21")), (Some(Language::Python), Some(Language::Node), None));
    }

    #[test]
    fn builds_with_catch_all_route() {
        // The usual shape: one Python app behind a catch-all route
        let spec = parse(
            r#"{"version": 2, "builds": [{"src": "api/main.py", "use": "@vercel/python", "config": {"maxLambdaSize": "30mb"}}],
                "routes": [{"src": "/(.*)", "dest": "api/main.py"}]}"#,
            has(API),
        )
        .unwrap();
        assert_eq!(spec.entry, "api/main.py");
        assert_eq!(spec.language, Language::Python);
        assert_eq!((spec.memory_mb, spec.max_duration_s), (None, None));
        assert!(spec.warnings.is_empty());
    }

    #[test]
    fn functions_with_memory_and_max_duration() {
        let spec = parse(
            r#"{"version": 2, "functions": {"api/main.py": {"memory": 512, "maxDuration": 10}},
                "routes": [{"src": "/(.*)", "dest": "api/main.py"}]}"#,
            has(API),
        )
        .unwrap();
        assert_eq!((spec.memory_mb, spec.max_duration_s), (Some(512), Some(10)));
        // Memory only
        let spec = parse(r#"{"functions": {"api/main.py": {"memory": 512}}, "routes": [{"src": "/(.*)", "dest": "/api/main.py"}]}"#, has(API))
            .unwrap();
        assert_eq!((spec.memory_mb, spec.max_duration_s), (Some(512), None));
    }

    #[test]
    fn regions_and_rewrites_are_accepted() {
        let spec = parse(
            r#"{"regions": ["iad1"], "builds": [{"src": "api/main.py", "use": "@vercel/python"}],
                "rewrites": [{"source": "/(.*)", "destination": "/api/main.py"}]}"#,
            has(API),
        )
        .unwrap();
        assert!(spec.warnings.is_empty());
        assert_eq!(spec.routing, None);
        assert_eq!(regions(spec.config.as_ref()).unwrap(), Some(vec!["us-east-1".to_owned()]));
    }

    #[test]
    fn function_regions() {
        let read = |json: &str| regions(Some(&serde_json::from_str::<Value>(json).unwrap()));
        assert_eq!(read(r#"{}"#).unwrap(), None);
        assert_eq!(read(r#"{"regions": ["fra1", "iad1", "fra1"]}"#).unwrap(), Some(vec!["eu-central-1".to_owned(), "us-east-1".to_owned()]));
        // AWS's names too, as forkrail shows them.
        assert_eq!(read(r#"{"regions": ["ap-southeast-2"]}"#).unwrap(), Some(vec!["ap-southeast-2".to_owned()]));
        for (json, says) in [
            (r#"{"regions": "iad1"}"#, "must be an array"),
            (r#"{"regions": [1]}"#, "isn't a region name"),
            (r#"{"regions": []}"#, "at least one"),
            (r#"{"regions": ["iad1", "xyz9"]}"#, "\"xyz9\" isn't a region"),
        ] {
            let e = read(json).unwrap_err().to_string();
            assert!(e.contains(says) && e.contains("regions"), "{json}: {e}");
        }
        assert_eq!(regions(None).unwrap(), None);
    }

    #[test]
    fn fluid() {
        // An API that needs each concurrent request on its own Lambda environment and IP
        let spec = parse(
            r#"{"version": 2, "fluid": false, "functions": {"api/main.py": {"memory": 512, "maxDuration": 60}},
                "routes": [{"src": "/(.*)", "dest": "api/main.py"}]}"#,
            has(API),
        )
        .unwrap();
        assert_eq!(spec.concurrent, Some(false));
        assert_eq!(parse(r#"{"fluid": true}"#, has(API)).unwrap().concurrent, Some(true));
        assert_eq!(parse("{}", has(API)).unwrap().concurrent, None);
        let err = parse(r#"{"fluid": "off"}"#, has(API)).unwrap_err();
        assert!(format!("{err:#}").contains("true or false"), "{err:#}");
    }

    #[test]
    fn no_route_warns() {
        let spec = parse(r#"{"functions": {"api/main.py": {}}}"#, has(API)).unwrap();
        assert!(spec.warnings[0].contains("/api/main"), "{:?}", spec.warnings);
    }

    #[test]
    fn other_rules_are_refused() {
        let cases = [
            // bayut: two functions and a specific route
            (r#"{"functions": {"api/main.py": {"memory": 512}, "api/auth.js": {"memory": 1024}},
                 "routes": [{"src": "/api/auth", "dest": "api/auth.js"}, {"src": "/(.*)", "dest": "api/main.py"}]}"#, "2 entries"),
            (r#"{"builds": [{"src": "api/main.py"}], "routes": [{"src": "/(", "dest": "api/main.py"}]}"#, "isn't one forkrail can run"),
            (r#"{"builds": [{"src": "api/main.py"}], "redirects": [{"source": "/a/:x", "destination": "/:y"}]}"#, "\":y\""),
            (r#"{"builds": [{"src": "api/main.py"}], "crons": {}}"#, "must be a list"),
            (r#"{"builds": [{"src": "api/main.py"}], "crons": [{"path": "api/x", "schedule": "* * * * *"}]}"#, "single /"),
            (r#"{"builds": [{"src": "api/main.py"}], "crons": [{"path": "/x", "schedule": "0 9 * * MON"}]}"#, "no names"),
            (r#"{"builds": [{"src": "api/main.py"}], "crons": [{"path": "/x", "schedule": "0 9 1 * 1"}]}"#, "not both"),
            (r#"{"builds": [{"src": "api/main.py"}], "crons": [{"path": "/x", "schedule": "0 0 31 2 *"}]}"#, "never runs"),
            (r#"{"builds": [{"src": "api/main.py"}], "crons": [{"path": "/x", "schedule": "* * * * *"}, {"path": "/x", "schedule": "* * * * *"}]}"#, "listed twice"),
            (r#"{"builds": [{"src": "api/main.py"}], "crons": [{"path": "/x", "schedule": "* * * * *", "method": "POST"}]}"#, "isn't a cron job key"),
            (r#"{"builds": [{"src": "api/main.py"}], "schedules": {"a": {"schedule": "0 * * * *", "function": "api/a"}}}"#, "`function` targets aren't supported yet"),
            (r#"{"builds": [{"src": "api/main.py"}], "schedules": {"a": {"schedule": "0 * * * *", "topic": "t"}}}"#, "`topic` targets"),
            (r#"{"builds": [{"src": "api/main.py"}], "schedules": {"a": {"schedule": "0 * * * *"}}}"#, "needs a `path`"),
            (r#"{"builds": [{"src": "api/main.py"}], "schedules": {"a": {"schedule": "0 * * * *", "path": "/a", "timezone": "Mars/Base"}}}"#, "IANA"),
            (r#"{"builds": [{"src": "api/main.py"}], "schedules": {"a": {"schedule": "*/5 * * * *", "path": "/a", "jitter": 6}}}"#, "longer than the 5 minutes"),
            (r#"{"builds": [{"src": "api/main.py"}], "schedules": {"a": {"schedule": "0 * * * *", "path": "/a", "jitter": 16}}}"#, "1 to 15"),
            (r#"{"builds": [{"src": "api/main.py"}], "schedules": {"a": {"schedule": "0 * * * *", "path": "/a", "requestMethod": "PUT"}}}"#, "\"GET\" or \"POST\""),
            (r#"{"builds": [{"src": "api/main.py"}], "schedules": {"-a": {"schedule": "0 * * * *", "path": "/a"}}}"#, "a name is"),
            (r#"{"builds": [{"src": "api/main.py"}], "outputDirectory": "dist"}"#, "isn't supported for an API"),
            (r#"{"builds": [{"src": "api/main.py"}], "public": true}"#, "Vercel no longer supports"),
            (r#"{"builds": [{"src": "api/main.py"}], "functions": {"api/main.py": {}}}"#, "together"),
            (r#"{"functions": {"api/*.py": {}}}"#, "patterns"),
            (r#"{"functions": {"api/main.py": {"includeFiles": "x/**"}}}"#, "includeFiles"),
            (r#"{"functions": {"api/main.py": {"memory": 64}}}"#, "128 to 10240"),
            (r#"{"builds": [{"src": "api/main.py", "use": "@vercel/node"}]}"#, "@vercel/python"),
            (r#"{"builds": [{"src": "api/missing.py"}]}"#, "doesn't exist"),
            (r#"{"builds": [{"src": "api/main.ts"}]}"#, "only Python"),
            (r#"{"version": 3}"#, "must be 2"),
        ];
        for (json, expect) in cases {
            let err = parse(json, has(API)).unwrap_err();
            assert!(format!("{err:#}").contains(expect), "{json}: {err:#}");
        }
    }

    #[test]
    fn schedules_are_read() {
        let spec = parse(
            r#"{"builds": [{"src": "api/main.py"}],
                "crons": [{"path": "/api/sync", "schedule": "0 5 * * *"}, {"path": "/api/sync", "schedule": "*/10 * * * *"}],
                "schedules": {
                    "daily-report": {"schedule": "0 9 * * 1-5", "path": "/api/report?full=1", "requestMethod": "GET", "timezone": "America/New_York", "jitter": 5},
                    "cleanup": {"schedule": "0 * * * *", "path": "/api/cleanup", "timezone": "UTC"}
                }}"#,
            has(API),
        )
        .unwrap();
        let s = &spec.schedules;
        assert_eq!(s.len(), 4);
        assert_eq!((s[0].path.as_str(), s[0].method.as_str(), s[0].name.as_deref(), s[0].jitter), ("/api/sync", "GET", None, None));
        assert!(s[0].key.starts_with("c-") && s[0].key != s[1].key);
        // `schedules` in the object's order: serde_json keeps keys sorted.
        assert_eq!((s[2].key.as_str(), s[2].method.as_str(), s[2].timezone.as_deref(), s[2].jitter), ("cleanup", "POST", None, Some(1)));
        assert_eq!((s[3].key.as_str(), s[3].path.as_str(), s[3].method.as_str()), ("daily-report", "/api/report?full=1", "GET"));
        assert_eq!((s[3].timezone.as_deref(), s[3].jitter), (Some("America/New_York"), Some(5)));
        // A static site may have them: its build may write a Build Output with functions to call
        // (build.rs refuses them when it doesn't).
        assert!(static_spec(framework::other(Path::new("/nonexistent"), None).unwrap(), Some(serde_json::json!({"crons": []}))).is_ok());
        let many: Vec<Value> = (0..101).map(|i| serde_json::json!({"path": format!("/c/{i}"), "schedule": "0 * * * *"})).collect();
        let json = serde_json::json!({"builds": [{"src": "api/main.py"}], "crons": many}).to_string();
        assert!(format!("{:#}", parse(&json, has(API)).unwrap_err()).contains("at most 100"));
    }

    #[test]
    fn routing_keys_make_a_program() {
        // Every API from Vercel has this one route: it routes as no program does.
        let spec = parse(r#"{"functions": {"api/main.py": {"memory": 768}}, "routes": [{"src": "/(.*)", "dest": "api/main.py"}]}"#, has(API)).unwrap();
        assert_eq!(spec.routing, None);
        let spec = parse(
            r#"{"builds": [{"src": "api/main.py"}], "redirects": [{"source": "/v1/:p*", "destination": "/v2/:p*"}],
                "headers": [{"source": "/(.*)", "headers": [{"key": "x-api", "value": "1"}]}]}"#,
            has(API),
        )
        .unwrap();
        let program = spec.routing.unwrap();
        assert_eq!((program.rule_count(), program.fallback, program.function.as_slice()), (2, true, ["/api/main.py".to_owned(), "/api/main".to_owned()].as_slice()));
    }

    fn folder(files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-vercel-{}", forkrail_common::random_hex(6)));
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    #[test]
    fn static_sites() {
        // A folder of files: no function.
        let site = read(&folder(&[("index.html", "<p>"), ("vercel.json", r#"{"cleanUrls": true, "redirects": [{"source": "/old", "destination": "/"}]}"#)])).unwrap();
        assert!(site.static_only());
        assert_eq!(site.describe(), "a static site (Other)");
        assert!(site.routing.as_ref().is_some_and(|p| p.clean_urls && !p.fallback));
        // A framework's build.
        let vite = read(&folder(&[("package.json", r#"{"scripts": {"build": "vite build"}, "devDependencies": {"vite": "7"}}"#), ("vercel.json", r#"{"rewrites": [{"source": "/(.*)", "destination": "/index.html"}]}"#)])).unwrap();
        assert_eq!((vite.describe().as_str(), vite.framework.as_ref().unwrap().output.as_str()), ("a static site (Vite)", "dist"));
        // A static site's keys are checked as such.
        let err = read(&folder(&[("index.html", "<p>"), ("vercel.json", r#"{"fluid": false}"#)])).unwrap_err();
        assert!(format!("{err:#}").contains("isn't supported for a static site"), "{err:#}");
        // An API keeps its public folder, and an API entry beside a Vite site makes it an API.
        let api = read(&folder(&[("api/main.py", "app = 1"), ("public/favicon.ico", "x"), ("package.json", r#"{"devDependencies": {"vite": "7"}}"#)])).unwrap();
        assert_eq!((api.entry.as_str(), api.public.as_deref(), api.static_only()), ("api/main.py", Some("public"), false));
        // A Nitro server takes routing keys too; its routes come with its Build Output, after the
        // build, which its dispatcher serves.
        let web = read(&folder(&[
            ("package.json", r#"{"scripts": {"build": "vite build"}, "dependencies": {"@tanstack/react-start": "1", "nitro": "3"}}"#),
            ("vercel.json", r#"{"redirects": [{"source": "/old", "destination": "/"}]}"#),
        ]))
        .unwrap();
        assert!(web.routing.is_none() && web.entry == crate::build_output::DISPATCHER && !web.static_only());
        // Built for node-server instead, its server is the fallback around vercel.json's routes.
        let mut server = web.clone();
        let fw = server.framework.clone().unwrap();
        let node = crate::framework::Framework { output: ".output".into(), entry: "server/index.mjs", public: "public", find: crate::framework::Output::Fixed, ..fw };
        use_server(&mut server, node).unwrap();
        assert!(server.routing.as_ref().is_some_and(|p| p.fallback) && server.entry == "server/index.mjs");
    }

    #[test]
    fn other_configuration_files() {
        let toml = read(&folder(&[("index.html", "<p>"), ("vercel.toml", "cleanUrls = true\n[[redirects]]\nsource = \"/old\"\ndestination = \"/\"\n")])).unwrap();
        assert!(toml.routing.as_ref().is_some_and(|p| p.clean_urls && p.rule_count() > 0));
        let err = read(&folder(&[("index.html", "<p>"), ("vercel.toml", ""), ("vercel.json", "{}")])).unwrap_err();
        assert!(format!("{err:#}").contains("keep one"), "{err:#}");
        // vercel.ts runs with Node, its types stripped; `config` may be computed, even awaited.
        let ts = "type C = { cleanUrls: boolean; redirects: { source: string; destination: string }[] };\n\
                  const from: string = process.env.FORKRAIL_TEST_FROM ?? '/old';\n\
                  export const config: Promise<C> = Promise.resolve({ cleanUrls: true, redirects: [{ source: from, destination: '/' }] });\n";
        let spec = read(&folder(&[("index.html", "<p>"), ("vercel.ts", ts)])).unwrap();
        assert!(spec.routing.as_ref().is_some_and(|p| p.clean_urls && p.rule_count() > 0));
        let err = read(&folder(&[("index.html", "<p>"), ("vercel.mjs", "throw new Error('nope')")])).unwrap_err();
        assert!(format!("{err:#}").contains("nope"), "{err:#}");
    }

    /// Installs `@vercel/config` from npm: `cargo test -p forkrail-cli -- --ignored vercel_config_helpers`.
    #[test]
    #[ignore]
    fn vercel_config_helpers() {
        let ts = "import { routes, type VercelConfig } from '@vercel/config/v1';\n\
                  export const config: VercelConfig = { cleanUrls: true,\n\
                  redirects: [routes.redirect('/old-docs', '/docs', { permanent: true })],\n\
                  rewrites: [routes.rewrite('/api/(.*)', 'https://backend.example.com/$1')],\n\
                  headers: [routes.header('/(.*)', [{ key: 'x-a', value: '1' }])] };\n";
        let (_, config) = load_config(&folder(&[("index.html", "<p>"), ("vercel.ts", ts)])).unwrap().unwrap();
        println!("{config:#}");
        let spec = read(&folder(&[("index.html", "<p>"), ("vercel.ts", ts)])).unwrap();
        assert!(spec.routing.as_ref().is_some_and(|p| p.rule_count() >= 3));
    }

    /// Deciding a push happens where the commit is fetched, maybe a person's own machine: the
    /// repository's `vercel.ts` isn't run there, whatever it would say.
    #[test]
    fn a_push_is_decided_without_running_the_repository() {
        let dir = folder(&[("index.html", "<p>")]);
        let marker = dir.join("ran");
        let js = format!("require('fs').writeFileSync({:?}, 'x'); module.exports = {{ git: {{ deploymentEnabled: false }}, github: {{ autoAlias: false }} }};", marker.display().to_string());
        std::fs::write(dir.join("vercel.cjs"), js).unwrap();
        assert!(push_deploys(&dir, "main"));
        assert!(github_keys(&dir).auto_alias);
        assert!(load_static_config(&dir).is_err());
        assert!(!marker.exists(), "vercel.cjs ran");
        // A vercel.json still decides.
        let dir = folder(&[("index.html", "<p>"), ("vercel.json", r#"{"git": {"deploymentEnabled": false}}"#)]);
        assert!(!push_deploys(&dir, "main"));
    }

    #[test]
    fn which_pushes_deploy() {
        let config = |text: &str| serde_json::from_str::<Value>(text).unwrap();
        let deploys = |text: &str, branch: &str| deploys_branch(Some(&config(text)), branch);
        assert!(deploys_branch(None, "main") && deploys("{}", "main") && deploys(r#"{"git": {}}"#, "main"));
        assert!(!deploys(r#"{"git": {"deploymentEnabled": false}}"#, "main"));
        assert!(deploys(r#"{"git": {"deploymentEnabled": true}}"#, "main"));
        // Vercel's older spelling.
        assert!(!deploys(r#"{"github": {"enabled": false}}"#, "feature/x") && deploys(r#"{"github": {"silent": true}}"#, "feature/x"));
        // By branch, and by pattern: off when a rule that matches says so and none says on.
        let rules = r#"{"git": {"deploymentEnabled": {"dev": false, "internal-*": false, "internal-keep": true, "wip/**": false, "release/*": false}}}"#;
        for (branch, on) in [
            ("main", true),
            ("dev", false),
            ("develop", true),
            ("internal-tools", false),
            ("internal-keep", true),
            ("wip/a/b", false),
            ("release/1.2", false),
            ("release/1.2/hotfix", true),
        ] {
            assert_eq!(deploys(rules, branch), on, "{branch}");
        }
        assert!(glob("feat?", "feat1") && !glob("feat?", "feat/") && glob("**", "a/b") && !glob("*", "a/b") && glob("a*c", "abc") && !glob("a*c", "abd"));

        // Both keys are read from an API's vercel.json and a framework's, and checked for shape.
        let spec = parse(r#"{"git": {"deploymentEnabled": {"dev": false}}, "ignoreCommand": " exit 0 "}"#, has(API)).unwrap();
        assert_eq!(spec.ignore_command.as_deref(), Some("exit 0"));
        assert_eq!(parse(r#"{"ignoreCommand": null}"#, has(API)).unwrap().ignore_command, None);
        for (json, expect) in [
            (r#"{"git": false}"#.to_owned(), "must be an object"),
            (r#"{"git": {"deploymentEnabled": "no"}}"#.to_owned(), "true, false"),
            (r#"{"git": {"deploymentEnabled": {"dev": "no"}}}"#.to_owned(), "true, false"),
            (r#"{"git": {"exclude": []}}"#.to_owned(), "git.exclude"),
            (r#"{"ignoreCommand": 1}"#.to_owned(), "as text"),
            (format!(r#"{{"ignoreCommand": "{}"}}"#, "x".repeat(257)), "256"),
        ] {
            let err = parse(&json, has(API)).unwrap_err();
            assert!(format!("{err:#}").contains(expect), "{json}: {err:#}");
        }
    }

    #[test]
    fn detection_without_vercel_json() {
        assert_eq!(detect(has(API)).unwrap().entry, "api/main.py");
        let node = detect(has(&["server.mjs"])).unwrap();
        assert_eq!((node.entry.as_str(), node.language), ("server.mjs", Language::Node));
        assert!(detect(has(&["index.html"])).is_err());
        // An empty vercel.json falls back to detection too.
        assert_eq!(parse("{}", has(API)).unwrap().entry, "api/main.py");
    }
}
