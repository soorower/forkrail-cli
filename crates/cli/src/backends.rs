//! Backend framework presets, as Vercel finds and runs them: one server per project, its entry found
//! from the project's files with no configuration.
//!
//! - **Python** (Vercel's `packages/python`): FastAPI, Flask, Django, FastHTML, and "Python" for any
//!   other ASGI or WSGI app. The entry is the first of `app.py`, `index.py`, `server.py`, `main.py`,
//!   `wsgi.py`, `asgi.py` at the top, then in `src/`, `app/` and `api/`, with a top-level `app`,
//!   `application` or `handler`; or `[tool.vercel] entrypoint = "module:object"`, or
//!   `[project.scripts] app = "module:object"` last. Django's comes from `manage.py`'s settings
//!   module: its `ASGI_APPLICATION`, else `WSGI_APPLICATION`, and its static files are collected at
//!   build time and served by the router (package.rs).
//! - **Node** (Vercel's `@vercel/express`, `@vercel/hono`, … builders): Express, Hono, Fastify, Koa,
//!   NestJS, Elysia, H3, and "Node.js" for a `server.*` file. The entry is the first of `app`,
//!   `index`, `server`, `src/app`, `src/index`, `src/server` (NestJS: `src/main` first) with a
//!   JavaScript or TypeScript extension that imports the framework, else `package.json`'s `main`.
//!   It may listen on `PORT` itself or export its app (Vercel's two ways): a launcher the build
//!   writes serves an exported one (shim.mjs). TypeScript is compiled with esbuild.
//! - **Go** and **Rust** web servers (native.rs).
//!
//! A preset is chosen by `vercel.json`'s `framework`, else the project's setting, else found.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::functions::{self, RustFunction};
use crate::native::{self, Server};
use crate::vercel::Language;

/// A preset: Vercel's slug, its name, and its language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub slug: &'static str,
    pub name: &'static str,
    pub language: Language,
    /// What it's detected by: a package in the project's dependencies (PyPI or npm names).
    package: &'static str,
}

/// Every backend preset, in the order they're detected.
pub const PRESETS: [Preset; 15] = [
    Preset { slug: "django", name: "Django", language: Language::Python, package: "django" },
    Preset { slug: "fastapi", name: "FastAPI", language: Language::Python, package: "fastapi" },
    Preset { slug: "flask", name: "Flask", language: Language::Python, package: "flask" },
    Preset { slug: "fasthtml", name: "FastHTML", language: Language::Python, package: "python-fasthtml" },
    Preset { slug: "python", name: "Python", language: Language::Python, package: "" },
    Preset { slug: "nestjs", name: "NestJS", language: Language::Node, package: "@nestjs/core" },
    Preset { slug: "express", name: "Express", language: Language::Node, package: "express" },
    Preset { slug: "hono", name: "Hono", language: Language::Node, package: "hono" },
    Preset { slug: "fastify", name: "Fastify", language: Language::Node, package: "fastify" },
    Preset { slug: "koa", name: "Koa", language: Language::Node, package: "koa" },
    Preset { slug: "elysia", name: "Elysia", language: Language::Node, package: "elysia" },
    Preset { slug: "h3", name: "H3", language: Language::Node, package: "h3" },
    Preset { slug: "node", name: "Node.js", language: Language::Node, package: "" },
    Preset { slug: "go", name: "Go", language: Language::Go, package: "" },
    Preset { slug: "rust", name: "Rust", language: Language::Rust, package: "" },
];

pub fn preset(slug: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.slug == slug)
}

/// Whether `name` (as a deployment records it) is a backend preset's.
pub fn is_preset_name(name: &str) -> bool {
    PRESETS.iter().any(|p| p.name == name)
}

/// A backend found in a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    pub preset: &'static Preset,
    /// The file the runtime starts, relative to the project, with `:object` when it isn't `app`
    /// (`mysite/wsgi.py:application`). For Go and Rust, the program (native.rs).
    pub entry: String,
    /// What found it, for the build log: `app.py (app)`, `manage.py → mysite.settings`.
    pub found: String,
    /// Go or Rust.
    pub server: Option<Server>,
    /// Django: the settings module, for collecting its static files.
    pub django_settings: Option<String>,
    /// Per-file functions (Vercel's `api/*.go`, `api/*.rs`), instead of one server.
    pub files: Option<FileFunctions>,
}

/// A project's per-file functions (functions.rs, nodefns.rs, pyfns.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileFunctions {
    Go(Vec<String>),
    Rust(Vec<RustFunction>),
    Node(Vec<String>),
    Python(Vec<crate::pyfns::PyFunction>),
}

impl FileFunctions {
    /// The function files, relative to the project.
    pub fn files(&self) -> Vec<String> {
        match self {
            FileFunctions::Go(files) | FileFunctions::Node(files) => files.clone(),
            FileFunctions::Rust(functions) => functions.iter().map(|f| f.file.clone()).collect(),
            FileFunctions::Python(functions) => functions.iter().map(|f| f.file.clone()).collect(),
        }
    }

    pub fn language(&self) -> Language {
        match self {
            FileFunctions::Go(_) => Language::Go,
            FileFunctions::Rust(_) => Language::Rust,
            FileFunctions::Node(_) => Language::Node,
            FileFunctions::Python(_) => Language::Python,
        }
    }
}

/// Every function file in a project's `api/`, by language, as Vercel's zero-config detection finds
/// them: Node's `.js`, `.mjs`, `.ts`, `.tsx`; the `.py` files that serve something; Go's `.go`;
/// Rust's `.rs`. Not yet checked against each other or `Cargo.toml` (`one_language`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    pub node: Vec<String>,
    pub python: Vec<crate::pyfns::PyFunction>,
    pub go: Vec<String>,
    pub rust: Vec<String>,
}

pub fn discover_functions(dir: &Path) -> Found {
    Found { node: crate::nodefns::discover(dir), python: crate::pyfns::discover(dir), go: functions::discover(dir, "go"), rust: functions::discover(dir, "rs") }
}

impl Found {
    pub fn is_empty(&self) -> bool {
        self.node.is_empty() && self.python.is_empty() && self.go.is_empty() && self.rust.is_empty()
    }

    /// Every function file, of every language, sorted.
    pub fn files(&self) -> Vec<String> {
        let mut all: Vec<String> = self.node.iter().chain(self.go.iter()).chain(self.rust.iter()).cloned().chain(self.python.iter().map(|f| f.file.clone())).collect();
        all.sort();
        all
    }

    /// The languages that have functions, with their files, in Vercel's builder order.
    fn by_language(&self) -> Vec<(Language, Vec<String>)> {
        let python: Vec<String> = self.python.iter().map(|f| f.file.clone()).collect();
        [(Language::Node, self.node.clone()), (Language::Go, self.go.clone()), (Language::Python, python), (Language::Rust, self.rust.clone())].into_iter().filter(|(_, files)| !files.is_empty()).collect()
    }

    /// The functions of a language (for a file's own: `.py` Python, `.js`/`.mjs`/`.cjs`/`.ts`... Node).
    pub fn of(&self, language: Language) -> Vec<String> {
        self.by_language().into_iter().find(|(l, _)| *l == language).map(|(_, f)| f).unwrap_or_default()
    }

    /// Whether `entry`, a single function the project names (`functions` with one file, or forkrail's
    /// own `api/main.py` / `server.mjs`), is the only function of its language here: then the project
    /// is one function, as forkrail has always run it, whatever else `api/` holds.
    pub fn alone(&self, entry: &str) -> bool {
        let language = match entry.rsplit_once('.').map(|(_, ext)| ext) {
            Some("py") => Language::Python,
            Some("js" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts") => Language::Node,
            _ => return true,
        };
        self.of(language).iter().all(|f| f == entry)
    }

    /// The functions, which must all be of one language: a project's functions run in one Lambda
    /// function, on that language's runtime. Their paths are checked against each other's, of
    /// every language, as Vercel checks them; Rust's are each a `[[bin]]` of `Cargo.toml`.
    pub fn one_language(&self, dir: &Path) -> Result<Option<FileFunctions>> {
        if self.is_empty() {
            return Ok(None);
        }
        functions::check_paths(&self.files())?;
        let languages = self.by_language();
        if languages.len() > 1 {
            let list: Vec<String> = languages.iter().map(|(l, files)| format!("{} ({})", l.name(), files.join(", "))).collect();
            bail!(
                "api/ has functions in more than one language: {}. forkrail runs a project's functions in one Lambda function, on one language's runtime, so far: move one language's into a project of its own",
                list.join("; ")
            );
        }
        Ok(Some(match languages[0].0 {
            Language::Node => FileFunctions::Node(self.node.clone()),
            Language::Python => FileFunctions::Python(self.python.clone()),
            Language::Go => FileFunctions::Go(self.go.clone()),
            Language::Rust => FileFunctions::Rust(functions::rust_bins(dir, &self.rust)?),
        }))
    }
}

/// The backend serving a project's per-file functions in `api/`, all of one language.
pub fn file_functions_backend(files: FileFunctions) -> Backend {
    let (preset, entry) = match files.language() {
        Language::Go => (preset("go"), functions::MANIFEST),
        Language::Rust => (preset("rust"), functions::MANIFEST),
        Language::Node => (preset("node"), crate::nodefns::ENTRY),
        Language::Python => (preset("python"), crate::pyfns::ENTRY),
    };
    let list = files.files();
    Backend {
        preset: preset.expect("a preset per language"),
        entry: entry.to_owned(),
        found: format!("{} function{} in api/ ({})", list.len(), if list.len() == 1 { "" } else { "s" }, list.join(", ")),
        server: None,
        django_settings: None,
        files: Some(files),
    }
}

/// The project's functions in `api/`, if it has any, all of one language (`Found::one_language`).
pub fn file_functions(dir: &Path) -> Result<Option<Backend>> {
    Ok(discover_functions(dir).one_language(dir)?.map(file_functions_backend))
}

impl Backend {
    pub fn language(&self) -> Language {
        self.preset.language
    }

    /// Whether the build compiles the entry first (TypeScript).
    pub fn typescript(&self) -> bool {
        self.preset.language == Language::Node && ["ts", "mts", "cts", "tsx"].iter().any(|ext| self.entry.ends_with(&format!(".{ext}")))
    }
}

/// The backend in `dir`: the preset `slug` names (from `vercel.json` or the project's settings),
/// or the first one found. `Ok(None)` when it has none (a static site, or nothing to serve).
pub fn detect(dir: &Path, slug: Option<&str>) -> Result<Option<Backend>> {
    let chosen = match slug {
        Some(slug) => match preset(slug) {
            Some(p) => Some(p),
            None => return Ok(None),
        },
        None => None,
    };
    match chosen.map(|p| p.language) {
        Some(Language::Python) => python(dir, chosen).and_then(|b| b.ok_or_else(|| no_entry(chosen.unwrap()))).map(Some),
        Some(Language::Node) => node(dir, chosen).and_then(|b| b.ok_or_else(|| no_entry(chosen.unwrap()))).map(Some),
        Some(Language::Go) | Some(Language::Rust) => {
            let wanted = chosen.unwrap();
            match native::detect(dir)? {
                Some(server) if server.language == wanted.language => Ok(Some(native_backend(server))),
                _ => Err(no_entry(wanted)),
            }
        }
        None => {
            if let Some(backend) = python(dir, None)? {
                return Ok(Some(backend));
            }
            if let Some(backend) = node(dir, None)? {
                return Ok(Some(backend));
            }
            Ok(native::detect(dir)?.map(native_backend))
        }
    }
}

fn native_backend(server: Server) -> Backend {
    let preset = PRESETS.iter().find(|p| p.language == server.language).expect("a preset per language");
    let found = match server.language {
        Language::Go => format!("a main package ({}) that listens on PORT", server.target),
        _ => format!("binary {} that listens on PORT", server.target),
    };
    Backend { preset, entry: server.entry(), found, server: Some(server), django_settings: None, files: None }
}

fn no_entry(preset: &Preset) -> anyhow::Error {
    match preset.language {
        Language::Python => anyhow::anyhow!(
            "{}: no entry found. Vercel looks for app.py, index.py, server.py, main.py, wsgi.py or asgi.py (at the top, or in src/, app/ or api/) with a top-level `app`, `application` or `handler`; or set `[tool.vercel] entrypoint = \"module:app\"` in pyproject.toml",
            preset.name
        ),
        Language::Node if preset.package.is_empty() => anyhow::anyhow!("{}: no server.js, server.mjs, server.ts (or under src/) to run", preset.name),
        Language::Node => anyhow::anyhow!(
            "{}: no entry found. Vercel looks for app, index or server (at the top or in src/) with a .js, .mjs, .cjs, .ts, .mts or .cts extension that imports {}, else package.json's `main`",
            preset.name,
            preset.package
        ),
        Language::Go => anyhow::anyhow!("Go: needs a go.mod at the project's top and main.go, cmd/api/main.go or cmd/server/main.go"),
        Language::Rust => anyhow::anyhow!("Rust: needs a Cargo.toml with a binary (src/main.rs)"),
    }
}

// ---- Python ----

const PYTHON_FILES: [&str; 6] = ["app", "index", "server", "main", "wsgi", "asgi"];
const PYTHON_DIRS: [&str; 4] = ["", "src", "app", "api"];
const PYTHON_OBJECTS: [&str; 3] = ["app", "application", "handler"];

/// The project's Python packages, by their PyPI names (lowercase, `-`), from whichever manifest it
/// has: `requirements.txt` (at the top or beside `api/`), `pyproject.toml`'s dependencies, or a
/// `Pipfile`.
pub fn python_packages(dir: &Path) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut any = false;
    for file in ["requirements.txt", "api/requirements.txt"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(file)) {
            any = true;
            names.extend(text.lines().filter_map(crate::framework::requirement_name));
        }
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("pyproject.toml")) {
        any = true;
        if let Ok(table) = toml::from_str::<toml::Table>(&text) {
            for list in [table.get("project").and_then(|p| p.get("dependencies")), table.get("tool").and_then(|t| t.get("poetry")).and_then(|p| p.get("dependencies"))] {
                match list {
                    Some(toml::Value::Array(items)) => names.extend(items.iter().filter_map(|i| i.as_str()).filter_map(crate::framework::requirement_name)),
                    Some(toml::Value::Table(items)) => names.extend(items.keys().filter_map(|k| crate::framework::requirement_name(k))),
                    _ => {}
                }
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("Pipfile")) {
        any = true;
        if let Ok(table) = toml::from_str::<toml::Table>(&text)
            && let Some(packages) = table.get("packages").and_then(|p| p.as_table())
        {
            names.extend(packages.keys().filter_map(|k| crate::framework::requirement_name(k)));
        }
    }
    any.then_some(names)
}

fn python(dir: &Path, chosen: Option<&'static Preset>) -> Result<Option<Backend>> {
    let packages = python_packages(dir);
    let manage = dir.join("manage.py");
    let django_project = std::fs::read_to_string(&manage).ok().and_then(|t| django_settings_module(&t));
    // Without a manifest it isn't a Python project, unless it's chosen, or is a Django one.
    if packages.is_none() && chosen.is_none() && django_project.is_none() {
        return Ok(None);
    }
    let packages = packages.unwrap_or_default();
    let found_preset = |p: &Preset| !p.package.is_empty() && packages.iter().any(|n| n == p.package);
    let preset = match chosen {
        Some(p) => p,
        None if django_project.is_some() => preset("django").expect("django"),
        None => PRESETS.iter().filter(|p| p.language == Language::Python).find(|p| found_preset(p)).unwrap_or(preset("python").expect("python")),
    };
    if preset.slug == "django" {
        if let Some(settings) = &django_project {
            let (entry, found) = django_entry(dir, settings);
            return Ok(Some(Backend { preset, entry, found, server: None, django_settings: Some(settings.clone()), files: None }));
        }
        if chosen.is_some() {
            bail!("Django: no manage.py with DJANGO_SETTINGS_MODULE at the project's top (set the project's root directory to the folder that has it)");
        }
    }
    if let Some((entry, found)) = python_entry(dir)? {
        return Ok(Some(Backend { preset, entry, found, server: None, django_settings: None, files: None }));
    }
    Ok(None)
}

/// `[tool.vercel] entrypoint`, the file search, then `[project.scripts] app`.
fn python_entry(dir: &Path) -> Result<Option<(String, String)>> {
    let pyproject = std::fs::read_to_string(dir.join("pyproject.toml")).ok().and_then(|t| toml::from_str::<toml::Table>(&t).ok());
    let configured = pyproject.as_ref().and_then(|t| t.get("tool")).and_then(|t| t.get("vercel")).and_then(|v| v.get("entrypoint")).and_then(|e| e.as_str());
    if let Some(target) = configured {
        let Some(entry) = module_file(dir, target) else { bail!("pyproject.toml's [tool.vercel] entrypoint {target:?} names no module in the project") };
        return Ok(Some((entry, format!("[tool.vercel] entrypoint {target:?}"))));
    }
    for folder in PYTHON_DIRS {
        for name in PYTHON_FILES {
            let rel = if folder.is_empty() { format!("{name}.py") } else { format!("{folder}/{name}.py") };
            let Ok(text) = std::fs::read_to_string(dir.join(&rel)) else { continue };
            if let Some(object) = top_level_object(&text) {
                let entry = if object == "app" { rel.clone() } else { format!("{rel}:{object}") };
                return Ok(Some((entry, format!("{rel} ({object})"))));
            }
        }
    }
    let script = pyproject.as_ref().and_then(|t| t.get("project")).and_then(|p| p.get("scripts")).and_then(|s| s.get("app")).and_then(|a| a.as_str());
    if let Some(target) = script
        && let Some(entry) = module_file(dir, target)
    {
        return Ok(Some((entry, format!("[project.scripts] app {target:?}"))));
    }
    Ok(None)
}

/// `module.path:object` as the entry the shim takes: `module/path.py:object` (or its package's
/// `__init__.py`), when that file is in the project.
fn module_file(dir: &Path, target: &str) -> Option<String> {
    let (module, object) = target.split_once(':').unwrap_or((target, "app"));
    let path = module.trim().replace('.', "/");
    let file = [format!("{path}.py"), format!("{path}/__init__.py"), format!("src/{path}.py"), format!("src/{path}/__init__.py")].into_iter().find(|f| dir.join(f).is_file())?;
    let object = object.trim();
    Some(if object == "app" { file } else { format!("{file}:{object}") })
}

/// The first of `app`, `application` and `handler` the module defines at its top level: assigned,
/// defined, or imported under that name.
fn top_level_object(text: &str) -> Option<&'static str> {
    let mut found: Option<usize> = None;
    let mut in_string: Option<&str> = None;
    for line in text.lines() {
        // Skip the insides of triple-quoted strings (docstrings).
        if let Some(quote) = in_string {
            if line.contains(quote) {
                in_string = None;
            }
            continue;
        }
        for quote in ["\"\"\"", "'''"] {
            if line.matches(quote).count() == 1 {
                in_string = Some(quote);
            }
        }
        if line.starts_with([' ', '\t', '#']) {
            continue;
        }
        for (i, name) in PYTHON_OBJECTS.iter().enumerate() {
            if defines(line, name) {
                found = Some(found.map_or(i, |f| f.min(i)));
            }
        }
    }
    found.map(|i| PYTHON_OBJECTS[i])
}

/// Whether a top-level line binds `name`.
fn defines(line: &str, name: &str) -> bool {
    let word_end = |rest: &str| rest.is_empty() || !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_');
    // `app = ...`, `app: Flask = ...`
    if let Some(rest) = line.strip_prefix(name)
        && word_end(rest)
    {
        let rest = rest.trim_start();
        if (rest.starts_with('=') && !rest.starts_with("==")) || rest.starts_with(':') {
            return true;
        }
    }
    // `def app(`, `async def app(`, `class app`
    for keyword in ["def ", "async def ", "class "] {
        if let Some(rest) = line.strip_prefix(keyword).and_then(|r| r.strip_prefix(name))
            && word_end(rest)
        {
            return true;
        }
    }
    // `from x import app`, `from x import create as app`, `import x as app`
    if line.starts_with("from ") || line.starts_with("import ") {
        let imported = line.split_once(" import ").map_or(line.trim_start_matches("import "), |(_, names)| names);
        return imported.trim_matches(|c| c == '(' || c == ')').split(',').any(|part| {
            let part = part.trim();
            part == name || part.rsplit_once(" as ").is_some_and(|(_, alias)| alias.trim() == name)
        });
    }
    false
}

/// `DJANGO_SETTINGS_MODULE` as `manage.py` sets it: `os.environ.setdefault("DJANGO_SETTINGS_MODULE", "mysite.settings")`.
pub fn django_settings_module(manage: &str) -> Option<String> {
    let at = manage.find("DJANGO_SETTINGS_MODULE")?;
    let rest = &manage[at + "DJANGO_SETTINGS_MODULE".len()..];
    let rest = rest.trim_start_matches(|c: char| c == '"' || c == '\'');
    let rest = rest.trim_start().strip_prefix(',').or_else(|| rest.trim_start().strip_prefix(']').and_then(|r| r.trim_start().strip_prefix('=')))?;
    let rest = rest.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let end = rest[1..].find(quote)?;
    let module = &rest[1..1 + end];
    module.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_')).then(|| module.to_owned())
}

/// Django's entry from its settings: `ASGI_APPLICATION` over `WSGI_APPLICATION`, as Vercel takes it,
/// read from the settings file's text; else the settings package's `wsgi.py`.
fn django_entry(dir: &Path, settings: &str) -> (String, String) {
    let path = settings.replace('.', "/");
    let text = [format!("{path}.py"), format!("{path}/__init__.py"), format!("{path}/base.py")].iter().filter_map(|f| std::fs::read_to_string(dir.join(f)).ok()).collect::<Vec<_>>().join("\n");
    for key in ["ASGI_APPLICATION", "WSGI_APPLICATION"] {
        if let Some(dotted) = setting_string(&text, key)
            && let Some((module, object)) = dotted.rsplit_once('.')
            && let Some(file) = module_file(dir, &format!("{module}:{object}"))
        {
            return (file, format!("manage.py → {settings} → {key} {dotted:?}"));
        }
    }
    let package = settings.split('.').next().unwrap_or(settings);
    (format!("{package}/wsgi.py:application"), format!("manage.py → {settings} → {package}/wsgi.py"))
}

/// `KEY = "value"` at a settings file's top level.
fn setting_string(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.trim_start().strip_prefix('=')?.trim();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let end = rest[1..].find(quote)?;
        Some(rest[1..1 + end].to_owned())
    })
}

// ---- Node ----

const NODE_EXTENSIONS: [&str; 6] = ["js", "cjs", "mjs", "ts", "cts", "mts"];
const NODE_FILES: [&str; 6] = ["app", "index", "server", "src/app", "src/index", "src/server"];
const NEST_FILES: [&str; 8] = ["src/main", "src/app", "src/index", "src/server", "main", "app", "index", "server"];

fn node(dir: &Path, chosen: Option<&'static Preset>) -> Result<Option<Backend>> {
    let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
        // forkrail's own convention, before presets: a server.* with no package.json.
        return Ok(server_file(dir).filter(|_| chosen.is_none_or(|p| p.slug == "node")).map(|entry| Backend {
            preset: preset("node").expect("node"),
            found: entry.clone(),
            entry,
            server: None,
            django_settings: None,
            files: None,
        }));
    };
    let package: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let has = |name: &str| ["dependencies", "devDependencies"].iter().any(|k| package.get(k).and_then(|d| d.get(name)).is_some());
    let candidates: Vec<&'static Preset> = match chosen {
        Some(p) => vec![p],
        None => PRESETS.iter().filter(|p| p.language == Language::Node && !p.package.is_empty() && has(p.package)).collect(),
    };
    for preset in candidates {
        if preset.package.is_empty() {
            continue;
        }
        let files: &[&str] = if preset.slug == "nestjs" { &NEST_FILES } else { &NODE_FILES };
        for stem in files {
            for ext in NODE_EXTENSIONS {
                let rel = format!("{stem}.{ext}");
                if let Ok(code) = std::fs::read_to_string(dir.join(&rel))
                    && imports(&code, preset.package)
                {
                    return Ok(Some(Backend { preset, found: format!("{rel} (imports {})", preset.package), entry: rel, server: None, django_settings: None, files: None }));
                }
            }
        }
        if let Some(main) = package.get("main").and_then(Value::as_str).map(|m| m.trim_start_matches("./").to_owned())
            && std::fs::read_to_string(dir.join(&main)).is_ok_and(|code| imports(&code, preset.package))
        {
            return Ok(Some(Backend { preset, found: format!("package.json main {main:?}"), entry: main, server: None, django_settings: None, files: None }));
        }
    }
    if chosen.is_none_or(|p| p.slug == "node")
        && let Some(entry) = server_file(dir)
    {
        return Ok(Some(Backend { preset: preset("node").expect("node"), found: entry.clone(), entry, server: None, django_settings: None, files: None }));
    }
    Ok(None)
}

/// `server.*` or `src/server.*`: Vercel's generic Node server.
fn server_file(dir: &Path) -> Option<String> {
    ["server", "src/server"].iter().flat_map(|stem| NODE_EXTENSIONS.iter().map(move |ext| format!("{stem}.{ext}"))).find(|f| dir.join(f).is_file())
}

/// Whether the code imports or requires `package` (Vercel's import regex).
fn imports(code: &str, package: &str) -> bool {
    for quote in ['"', '\''] {
        let quoted = format!("{quote}{package}{quote}");
        let mut from = 0;
        while let Some(at) = code[from..].find(&quoted) {
            let at = from + at;
            let before = code[..at].trim_end();
            let before = before.strip_suffix('(').map(str::trim_end).unwrap_or(before);
            if before.ends_with("from") || before.ends_with("require") || before.ends_with("import") {
                return true;
            }
            from = at + quoted.len();
        }
    }
    false
}

/// Every framework preset a project's settings may choose, by Vercel's slug, its name, and what it
/// makes: the backend presets here, then the frameworks that build (framework.rs).
pub fn all() -> Vec<(&'static str, &'static str, &'static str)> {
    let built = crate::framework::slugs().into_iter().map(|(slug, name, server)| (slug, name, if server { "Node.js server" } else { "Static site" }));
    PRESETS.iter().map(|p| (p.slug, p.name, p.language.name())).chain(built).collect()
}

/// Whether a project's settings may choose `slug`.
pub fn choosable(slug: &str) -> bool {
    preset(slug).is_some() || crate::framework::known_slug(slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-be-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn found(files: &[(&str, &str)]) -> Option<(&'static str, String)> {
        detect(&project(files), None).unwrap().map(|b| (b.preset.slug, b.entry))
    }

    #[test]
    fn python_entries_as_vercel_finds_them() {
        assert_eq!(found(&[("requirements.txt", "flask\n"), ("app.py", "from flask import Flask\napp = Flask(__name__)\n")]), Some(("flask", "app.py".into())));
        // The top's files before src/, app/ and api/; each folder through every name.
        assert_eq!(
            found(&[("requirements.txt", "fastapi\n"), ("src/main.py", "app = FastAPI()"), ("main.py", "def helper(): pass\napp: FastAPI = FastAPI()\n")]),
            Some(("fastapi", "main.py".into()))
        );
        assert_eq!(found(&[("requirements.txt", "fastapi\n"), ("api/main.py", "app = FastAPI()")]), Some(("fastapi", "api/main.py".into())));
        assert_eq!(found(&[("pyproject.toml", "[project]\ndependencies = [\"falcon\"]\n"), ("wsgi.py", "import falcon\napplication = falcon.App()\n")]), Some(("python", "wsgi.py:application".into())));
        assert_eq!(found(&[("Pipfile", "[packages]\nflask = \"*\"\n"), ("index.py", "from server import create_app as app\n")]), Some(("flask", "index.py".into())));
        // An indented, commented or docstring `app` isn't the module's.
        assert_eq!(
            found(&[("requirements.txt", "flask\n"), ("app.py", "\"\"\"\napp = 1\n\"\"\"\n# app = 2\ndef make():\n    app = 3\n"), ("server.py", "handler = make()\n")]),
            Some(("flask", "server.py:handler".into()))
        );
        // [tool.vercel] entrypoint, then [project.scripts] app.
        assert_eq!(
            found(&[("pyproject.toml", "[project]\ndependencies = [\"fastapi\"]\n[tool.vercel]\nentrypoint = \"service.web:api\"\n"), ("service/web.py", "api = 1")]),
            Some(("fastapi", "service/web.py:api".into()))
        );
        assert_eq!(found(&[("pyproject.toml", "[project]\ndependencies = []\n[project.scripts]\napp = \"pkg.serve:app\"\n"), ("pkg/serve.py", "x")]), Some(("python", "pkg/serve.py".into())));
        // No manifest, no Python project; a manifest with no entry isn't one either.
        assert_eq!(found(&[("app.py", "app = 1")]), None);
        assert_eq!(found(&[("requirements.txt", "mkdocs\n"), ("docs/index.md", "# hi")]), None);
        // Chosen but not found says what Vercel looks for.
        assert!(detect(&project(&[("requirements.txt", "")]), Some("flask")).unwrap_err().to_string().contains("app.py, index.py"));
    }

    #[test]
    fn django_projects() {
        let manage = "#!/usr/bin/env python\nimport os\nos.environ.setdefault('DJANGO_SETTINGS_MODULE', 'mysite.settings')\n";
        let wsgi = found(&[("manage.py", manage), ("requirements.txt", "Django==5.2\n"), ("mysite/settings.py", "WSGI_APPLICATION = 'mysite.wsgi.application'\n"), ("mysite/wsgi.py", "application = 1")]);
        assert_eq!(wsgi, Some(("django", "mysite/wsgi.py:application".into())));
        // ASGI over WSGI, as Vercel takes it.
        let asgi = found(&[
            ("manage.py", manage),
            ("requirements.txt", "django\n"),
            ("mysite/settings.py", "WSGI_APPLICATION = \"mysite.wsgi.application\"\nASGI_APPLICATION = \"mysite.asgi.application\"\n"),
            ("mysite/wsgi.py", ""),
            ("mysite/asgi.py", ""),
        ]);
        assert_eq!(asgi, Some(("django", "mysite/asgi.py:application".into())));
        // Split settings without the key: the package's wsgi.py.
        assert_eq!(found(&[("manage.py", manage), ("mysite/settings/__init__.py", "from .base import *\n")]), Some(("django", "mysite/wsgi.py:application".into())));
        assert_eq!(django_settings_module("os.environ[\"DJANGO_SETTINGS_MODULE\"] = \"conf.settings.prod\""), Some("conf.settings.prod".into()));
        assert_eq!(django_settings_module("print('hi')"), None);
    }

    #[test]
    fn node_entries_as_vercel_finds_them() {
        let express = r#"{"dependencies": {"express": "5"}}"#;
        assert_eq!(found(&[("package.json", express), ("index.js", "const express = require('express')\n")]), Some(("express", "index.js".into())));
        // It must import the framework.
        assert_eq!(found(&[("package.json", express), ("app.js", "console.log(1)"), ("src/index.ts", "import express from \"express\"\nexport default express()\n")]), Some(("express", "src/index.ts".into())));
        assert_eq!(found(&[("package.json", r#"{"dependencies": {"hono": "4"}}"#), ("src/index.ts", "import { Hono } from 'hono'\n")]), Some(("hono", "src/index.ts".into())));
        // NestJS: src/main first.
        assert_eq!(
            found(&[("package.json", r#"{"dependencies": {"@nestjs/core": "11", "express": "5"}}"#), ("src/main.ts", "import { NestFactory } from '@nestjs/core'\n"), ("index.js", "require('express')")]),
            Some(("nestjs", "src/main.ts".into()))
        );
        // package.json's main, then a server file.
        assert_eq!(found(&[("package.json", r#"{"main": "./lib/start.js", "dependencies": {"koa": "2"}}"#), ("lib/start.js", "const Koa = require( 'koa' )")]), Some(("koa", "lib/start.js".into())));
        assert_eq!(found(&[("package.json", "{}"), ("server.mjs", "import http from 'node:http'")]), Some(("node", "server.mjs".into())));
        assert_eq!(found(&[("server.mjs", "x")]), Some(("node", "server.mjs".into())));
        assert_eq!(found(&[("package.json", r#"{"dependencies": {"react": "19"}}"#), ("index.js", "x")]), None);
        assert!(imports("import {serve} from \"@hono/node-server\"\nimport { Hono } from \"hono\"", "hono"));
        assert!(!imports("import x from \"hono/jsx\"", "hono"));
    }

    #[test]
    fn python_before_node_and_native() {
        // A Python app with a package.json for its styles is a Python app.
        assert_eq!(found(&[("requirements.txt", "fastapi"), ("main.py", "app = 1"), ("package.json", r#"{"dependencies": {"express": "5"}}"#), ("index.js", "require('express')")]).unwrap().0, "fastapi");
        assert_eq!(found(&[("go.mod", "module x\n"), ("main.go", "package main\n\nfunc main() {}\n")]).unwrap().0, "go");
        // A chosen preset of another language than the project's is refused.
        assert!(detect(&project(&[("go.mod", "module x\n"), ("main.go", "package main\nfunc main() {}\n")]), Some("rust")).is_err());
        assert_eq!(detect(&project(&[]), Some("vite")).unwrap(), None);
    }
}
