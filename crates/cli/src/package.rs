//! Build a project's Lambda package: our runtime as `bootstrap`, the app in `app/`, and for Python
//! its requirements in `python/` as Linux arm64 wheels, everything compiled to bytecode with the
//! Python version it runs on (runtimes.rs). A Go or Rust program goes in `app/` beside the
//! project's files (native.rs). A Django app's static files are collected here too, since that
//! needs its requirements.
//!
//! This is `package_python` from `deploy/aws/test-env.sh`, ported. The zip is deterministic (sorted
//! entries, fixed timestamps and modes), so unchanged code gives the same hash and uploads nothing.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use base64::Engine;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::buildlog::{self, BuildLog};
use crate::vercel::{Language, Spec};

/// Lambda's limit on a function's unzipped code.
pub const MAX_UNZIPPED: u64 = 262_144_000;
/// Names never packaged: version control, local state, caches, virtualenvs, secrets.
const EXCLUDED_NAMES: [&str; 8] = [".git", ".vercel", "__pycache__", ".venv", "venv", ".DS_Store", ".vercelignore", ".pytest_cache"];
/// Added to every Python app: the shim serves the app with uvicorn, with uvloop and httptools, and
/// a WSGI app (Flask, Django) through a2wsgi.
const PYTHON_SERVER: [&str; 4] = ["uvicorn", "uvloop", "httptools", "a2wsgi"];
/// The Python version of a build from before versions were read.
const PYTHON_VERSION: &str = "3.13";

/// A file of the project, with its path inside `app/`.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub rel: String,
    pub abs: PathBuf,
}

/// The project's files, sorted, without the excluded names, `.env*` and what `.vercelignore` lists.
/// Python, Go and Rust apps also leave out `node_modules`, and Rust its build's `target/`.
pub fn collect(dir: &Path, language: Language) -> Result<Vec<SourceFile>> {
    let python = language != Language::Node;
    let rust = language == Language::Rust;
    let walker = WalkBuilder::new(dir)
        .standard_filters(false)
        .add_custom_ignore_filename(".vercelignore")
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            let top = entry.depth() == 1;
            !(EXCLUDED_NAMES.contains(&name.as_ref()) || name.starts_with(".env") || (python && name == "node_modules") || (rust && top && name == "target"))
        })
        .build();
    // A link is followed only to a file inside the project's repository (its folder, if it isn't
    // in one): a committed link to ~/.aws/credentials would otherwise package the deployer's own.
    let inside = repository_of(dir);
    let mut files = Vec::new();
    for entry in walker {
        let entry = entry.context("listing the project's files")?;
        let path = entry.path();
        // Symlinks to files are packaged as the file; directories behind links are skipped.
        if !path.is_file() {
            continue;
        }
        if entry.path_is_symlink() && !path.canonicalize().is_ok_and(|target| inside.as_ref().is_some_and(|root| target.starts_with(root))) {
            eprintln!("warning: {} links outside the repository: left out", path.display());
            continue;
        }
        let rel = path.strip_prefix(dir).expect("walked from dir").to_string_lossy().replace('\\', "/");
        files.push(SourceFile { rel, abs: path.to_path_buf() });
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(files)
}

/// The repository `dir` is in (the nearest folder above it with a `.git`), else `dir` itself,
/// resolved; None if it can't be.
fn repository_of(dir: &Path) -> Option<PathBuf> {
    let dir = dir.canonicalize().ok()?;
    Some(dir.ancestors().find(|a| a.join(".git").exists()).unwrap_or(&dir).to_path_buf())
}

/// Hash of the project's files and their paths, recorded with each deployment.
pub fn source_hash(files: &[SourceFile]) -> Result<String> {
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.rel.as_bytes());
        hasher.update([0]);
        hasher.update(file_sha256(&file.abs)?);
    }
    Ok(forkrail_common::hex(&hasher.finalize()))
}

pub struct Options {
    /// The runtime binary for Linux arm64, packaged as `bootstrap`.
    pub bootstrap: PathBuf,
    /// Python 3.13, for pip and the bytecode.
    pub python: String,
    /// Install the requirements again even if they're cached.
    pub no_cache: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    /// Hex, for the S3 key and the record.
    pub sha256: String,
    pub zipped: u64,
    pub unzipped: u64,
    pub entries: usize,
    /// Whether the Python requirements came from the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deps_cached: Option<bool>,
    /// Whether its runtime opens sealed env files (forkrail_common::envfile): only then may its
    /// env vars go past Lambda's 4 KB.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reads_sealed_env: bool,
    /// Whether its runtime hands the app the project's own AWS credentials (`AWS_ACCESS_KEY_ID` …),
    /// which only a sealed file carries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub app_aws_keys: bool,
}

impl Package {
    /// The hash as Lambda reports a function's `CodeSha256`: base64.
    pub fn sha256_b64(&self) -> Result<String> {
        let bytes = (0..self.sha256.len())
            .step_by(2)
            .map(|i| self.sha256.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .collect::<Option<Vec<u8>>>()
            .filter(|b| b.len() == 32)
            .with_context(|| format!("{:?} isn't a SHA-256", self.sha256))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}

/// What a build adds to the package besides the project's files.
#[derive(Debug, Default)]
pub struct Extra {
    /// Files made by the build, by their path in `app/`: a Go or Rust program, compiled
    /// TypeScript, the launcher of a Node app that exports its server.
    pub files: Vec<(String, PathBuf)>,
    /// A Django app's static files are collected into this folder, at their `STATIC_URL`'s path.
    pub collect_static: Option<PathBuf>,
}

/// Build the package as the zip `path`. The project's commands (pip, the bytecode compiler) run
/// with `env`, their output in `log`.
pub fn build(
    dir: &Path,
    spec: &Spec,
    files: &[SourceFile],
    opts: &Options,
    path: &Path,
    env: &BTreeMap<String, String>,
    log: &dyn BuildLog,
) -> Result<Package> {
    build_with(dir, spec, files, &Extra::default(), opts, path, env, log)
}

/// `build`, with what the build made.
#[allow(clippy::too_many_arguments)]
pub fn build_with(
    dir: &Path,
    spec: &Spec,
    files: &[SourceFile],
    extra: &Extra,
    opts: &Options,
    path: &Path,
    env: &BTreeMap<String, String>,
    log: &dyn BuildLog,
) -> Result<Package> {
    check_bootstrap(&opts.bootstrap)?;
    let runtime = std::fs::read(&opts.bootstrap)?;
    let reads_sealed_env = forkrail_common::envfile::runtime_opens(&runtime);
    let app_aws_keys = forkrail_common::envfile::runtime_has(&runtime, forkrail_common::envfile::RUNTIME_AWS_KEYS_MARKER);
    drop(runtime);
    let stage = TempDir::new("stage")?;
    let app = stage.path().join("app");
    for file in files {
        let to = app.join(&file.rel);
        std::fs::create_dir_all(to.parent().expect("files have a parent"))?;
        std::fs::copy(&file.abs, &to).with_context(|| format!("copying {}", file.rel))?;
    }
    for (rel, from) in &extra.files {
        let to = app.join(rel);
        std::fs::create_dir_all(to.parent().expect("files have a parent"))?;
        std::fs::copy(from, &to).with_context(|| format!("copying {rel}"))?;
    }
    // Zip name → file on disk.
    let mut entries = BTreeMap::new();
    entries.insert("bootstrap".to_owned(), opts.bootstrap.clone());
    let mut deps_cached = None;
    if spec.language == Language::Python {
        log.step(buildlog::INSTALLING);
        let version = python_version_of(spec);
        let python = python_for(opts, &version)?;
        let (deps, cached) = python_deps(dir, spec, opts, env, log)?;
        deps_cached = Some(cached);
        if let (Some(settings), Some(into)) = (spec.backend.as_ref().and_then(|b| b.django_settings.as_deref()), &extra.collect_static) {
            log.step(buildlog::BUILDING);
            collect_static(&python, &deps, &app, settings, into, env, log)?;
        }
        log.step(buildlog::PACKAGING);
        // /var/task is read-only on Lambda, so Python would compile every module on every cold
        // start. Unchecked: Lambda's copy of each file has a different mtime.
        compile(&python, stage.path(), &app, env, log)?;
        add_tree(&mut entries, &deps, "python")?;
    } else {
        log.step(buildlog::PACKAGING);
    }
    add_tree(&mut entries, &app, "app")?;

    let unzipped = write_zip(path, &entries)?;
    if unzipped > MAX_UNZIPPED {
        let _ = std::fs::remove_file(path);
        bail!("the package is {} MB unzipped, over Lambda's 250 MB", unzipped >> 20);
    }
    Ok(Package {
        zipped: std::fs::metadata(path)?.len(),
        sha256: forkrail_common::hex(&file_sha256(path)?),
        unzipped,
        entries: entries.len(),
        deps_cached,
        reads_sealed_env,
        app_aws_keys,
    })
}

/// The bootstrap must be a Linux arm64 executable, or every worker would fail to start.
fn check_bootstrap(path: &Path) -> Result<()> {
    let mut head = [0u8; 20];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .with_context(|| format!("reading the runtime {}; build it with `cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.34 -p forkrail-runtime`, or set FORKRAIL_BOOTSTRAP", path.display()))?;
    // ELF, 64-bit, little-endian, machine 183 (AArch64).
    if &head[..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 || u16::from_le_bytes([head[18], head[19]]) != 183 {
        bail!("{} isn't a Linux arm64 executable", path.display());
    }
    Ok(())
}

/// Set to 1 in the builder's image, which is Lambda's own Amazon Linux 2023 and Python on arm64:
/// pip installs for the machine it runs on.
pub const NATIVE_PYTHON_ENV: &str = "FORKRAIL_NATIVE_PYTHON";

fn native_python() -> bool {
    std::env::var(NATIVE_PYTHON_ENV).is_ok_and(|v| v == "1")
}

/// A Python project's `requirements.txt`: at its root, else beside the entry.
pub fn requirements_file(dir: &Path, entry: &str) -> Option<PathBuf> {
    let entry_dir = Path::new(entry).parent().unwrap_or(Path::new(""));
    [dir.join("requirements.txt"), dir.join(entry_dir).join("requirements.txt")].into_iter().find(|p| p.is_file())
}

/// Pip's `--platform` tags for Lambda, when pip doesn't run on Lambda's own Linux: every glibc up
/// to Amazon Linux 2023's 2.34, since pip matches only the tags it's given.
fn lambda_platforms() -> Vec<String> {
    std::iter::once("manylinux2014_aarch64".to_owned()).chain((17..=34).map(|glibc| format!("manylinux_2_{glibc}_aarch64"))).collect()
}

/// What installed requirements are cached under: it changes when they would.
fn deps_key(version: &str, requirements: &Requirements, native: bool) -> String {
    let target = if native { "native".to_owned() } else { lambda_platforms().join(",") };
    let mut h = Sha256::new();
    for part in [version, &target, &PYTHON_SERVER.join(","), &requirements.text, requirements.constraints.as_deref().unwrap_or("")] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    forkrail_common::hex(&h.finalize())[..16].to_owned()
}

/// The Python version a spec runs (runtimes.rs), or the one builds used before versions were read.
fn python_version_of(spec: &Spec) -> String {
    spec.version.as_ref().map_or_else(|| PYTHON_VERSION.to_owned(), |v| v.version.clone())
}

/// The interpreter for Python `version`: `opts.python` if it's that version, else `pythonX.Y` on
/// the PATH (the builder's image has each version Lambda runs).
fn python_for(opts: &Options, version: &str) -> Result<String> {
    let wanted = format!("python{version}");
    if Path::new(&opts.python).file_name().is_some_and(|n| n.to_string_lossy() == wanted) {
        return Ok(opts.python.clone());
    }
    let reports = |program: &str| {
        Command::new(program)
            .args(["-c", "import sys; print('%d.%d' % sys.version_info[:2])"])
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == version)
    };
    if reports(&opts.python) {
        return Ok(opts.python.clone());
    }
    // Beside the given one first (the builder's /var/lang/bin), then the PATH.
    if let Some(beside) = Path::new(&opts.python).parent().map(|d| d.join(&wanted)).filter(|p| p.is_file()) {
        return Ok(beside.display().to_string());
    }
    if reports(&wanted) {
        return Ok(wanted);
    }
    bail!("Python {version} isn't installed here ({wanted} couldn't be run), and the project runs on it: install it, or build in the builder (deploy without --local)")
}

/// What pip installs: requirements in `requirements.txt`'s form, and constraints (pins from a lock
/// file) that pick their versions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requirements {
    /// The file they came from, for the log: `requirements.txt`, `pyproject.toml`.
    pub source: Option<String>,
    /// A `requirements.txt` read as it is, so its own `-r other.txt` lines resolve beside it.
    pub file: Option<PathBuf>,
    pub text: String,
    pub constraints: Option<String>,
}

/// The project's requirements from whichever manifest it has, in Vercel's order: `pyproject.toml`'s
/// `[project] dependencies` (pinned by a `uv.lock` beside it), `Pipfile.lock`, `Pipfile`, then
/// `requirements.txt` (at the top, else beside the entry).
pub fn requirements(dir: &Path, entry: &str) -> Result<Requirements> {
    if let Ok(text) = std::fs::read_to_string(dir.join("pyproject.toml")) {
        let table: toml::Table = toml::from_str(&text).context("pyproject.toml isn't valid TOML")?;
        if let Some(list) = table.get("project").and_then(|p| p.get("dependencies")).and_then(|d| d.as_array()) {
            let lines: Vec<&str> = list.iter().filter_map(|d| d.as_str()).collect();
            let constraints = std::fs::read_to_string(dir.join("uv.lock")).ok().map(|lock| uv_lock_pins(&lock)).transpose()?;
            let source = if constraints.is_some() { "pyproject.toml, pinned by uv.lock" } else { "pyproject.toml" };
            return Ok(Requirements { source: Some(source.to_owned()), file: None, text: lines.join("\n") + "\n", constraints });
        }
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("Pipfile.lock")) {
        let lock: serde_json::Value = serde_json::from_str(&text).context("Pipfile.lock isn't valid JSON")?;
        return Ok(Requirements { source: Some("Pipfile.lock".to_owned()), file: None, text: pipfile_lock_lines(&lock), constraints: None });
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("Pipfile")) {
        let table: toml::Table = toml::from_str(&text).context("Pipfile isn't valid TOML")?;
        return Ok(Requirements { source: Some("Pipfile".to_owned()), file: None, text: pipfile_lines(&table), constraints: None });
    }
    let path = requirements_file(dir, entry);
    let text = match &path {
        Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
        None => String::new(),
    };
    let source = path.as_ref().map(|p| p.strip_prefix(dir).unwrap_or(p).display().to_string());
    Ok(Requirements { source, file: path, text, constraints: None })
}

/// `name==version` for each package `uv.lock` takes from an index (not the project itself, nor a
/// path, git or URL source).
fn uv_lock_pins(lock: &str) -> Result<String> {
    let table: toml::Table = toml::from_str(lock).context("uv.lock isn't valid TOML")?;
    let mut pins = Vec::new();
    for package in table.get("package").and_then(|p| p.as_array()).into_iter().flatten() {
        let (Some(name), Some(version)) = (package.get("name").and_then(|n| n.as_str()), package.get("version").and_then(|v| v.as_str())) else { continue };
        let from_index = package.get("source").and_then(|s| s.as_table()).is_some_and(|s| s.contains_key("registry"));
        if from_index {
            pins.push(format!("{name}=={version}"));
        }
    }
    pins.sort();
    pins.dedup();
    Ok(pins.join("\n") + "\n")
}

/// `Pipfile.lock`'s `default` packages as pinned requirements.
fn pipfile_lock_lines(lock: &serde_json::Value) -> String {
    let mut lines = Vec::new();
    if let Some(packages) = lock.get("default").and_then(|d| d.as_object()) {
        for (name, info) in packages {
            let extras = info.get("extras").and_then(|e| e.as_array()).map(|e| e.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).filter(|e| !e.is_empty());
            let version = info.get("version").and_then(|v| v.as_str()).unwrap_or("");
            let markers = info.get("markers").and_then(|m| m.as_str());
            let mut line = name.clone();
            if let Some(extras) = extras {
                line.push_str(&format!("[{extras}]"));
            }
            line.push_str(version);
            if let Some(markers) = markers {
                line.push_str(&format!(" ; {markers}"));
            }
            lines.push(line);
        }
    }
    lines.join("\n") + "\n"
}

/// `Pipfile`'s `[packages]` as requirements: `name = "*"`, `name = ">=1"`, or a table with
/// `version` and `extras`.
fn pipfile_lines(table: &toml::Table) -> String {
    let mut lines = Vec::new();
    if let Some(packages) = table.get("packages").and_then(|p| p.as_table()) {
        for (name, spec) in packages {
            let (version, extras) = match spec {
                toml::Value::String(v) => (v.clone(), None),
                toml::Value::Table(t) => (
                    t.get("version").and_then(|v| v.as_str()).unwrap_or("*").to_owned(),
                    t.get("extras").and_then(|e| e.as_array()).map(|e| e.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")),
                ),
                _ => ("*".to_owned(), None),
            };
            let mut line = name.clone();
            if let Some(extras) = extras.filter(|e| !e.is_empty()) {
                line.push_str(&format!("[{extras}]"));
            }
            if version != "*" {
                line.push_str(&version);
            }
            lines.push(line);
        }
    }
    lines.join("\n") + "\n"
}

/// The folder on this machine that holds Python projects' installed requirements, one folder per
/// key, and the key of the project in `dir`: `<folder>/<key>` is complete once it holds
/// `PYTHON_DEPS_DONE`. The builder keeps that folder between a project's builds (cache.rs).
pub fn python_deps_cache(dir: &Path, spec: &Spec) -> Result<(PathBuf, String)> {
    let requirements = requirements(dir, spec.python_entry())?;
    Ok((cache_dir()?.join("python-deps"), deps_key(&python_version_of(spec), &requirements, native_python())))
}

/// The file that marks a folder of installed requirements as whole.
pub const PYTHON_DEPS_DONE: &str = ".complete";

/// `python/` with the app's requirements and the server, compiled, from the cache when the
/// requirements haven't changed. Returns the directory and whether it was cached.
fn python_deps(dir: &Path, spec: &Spec, opts: &Options, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<(PathBuf, bool)> {
    let version = python_version_of(spec);
    let python = python_for(opts, &version)?;
    let requirements = requirements(dir, spec.python_entry())?;
    if requirements.source.is_none() && dir.join("pyproject.toml").is_file() {
        log.info("warning: pyproject.toml has no [project] dependencies, and there's no requirements.txt: only the server is installed");
    }
    // On the builder pip installs for the machine it runs on, which is Lambda's, as Vercel's does,
    // so a release with no wheel builds from its source. Elsewhere it asks for Lambda's wheels,
    // which can't build anything and may settle for an older release that has one.
    let native = native_python();
    let platforms = if native { Vec::new() } else { lambda_platforms() };
    let key = deps_key(&version, &requirements, native);
    let cache = cache_dir()?.join("python-deps").join(&key);
    let done = cache.join(PYTHON_DEPS_DONE);
    if done.is_file() && !opts.no_cache {
        log.info("Python requirements unchanged since the last build: taken from the cache");
        return Ok((cache.join("python"), true));
    }
    let from = requirements.source.as_deref().map(|s| format!(" from {s}")).unwrap_or_default();
    log.info(&if native { format!("installing Python {version} requirements{from}") } else { format!("installing Python {version} requirements{from} for Linux arm64") });
    let venv = cache_dir()?.join(format!("python-build-{version}"));
    if !venv.join("bin/python").is_file() {
        buildlog::run(Command::new(&python).args(["-m", "venv"]).arg(&venv), "creating the build venv", env, log)?;
    }
    let building = cache.with_extension(format!("tmp-{}", forkrail_common::random_hex(4)));
    std::fs::create_dir_all(&building)?;
    let target = building.join("python");
    let mut pip = Command::new(venv.join("bin/python"));
    pip.args(["-m", "pip", "install", "--quiet", "--disable-pip-version-check", "--target"]).arg(&target);
    if !native {
        for platform in &platforms {
            pip.args(["--platform", platform]);
        }
        pip.args(["--implementation", "cp", "--python-version", &version, "--only-binary=:all:"]);
    }
    pip.arg("--no-compile");
    match &requirements.file {
        Some(path) => {
            pip.arg("-r").arg(path);
        }
        None if !requirements.text.trim().is_empty() => {
            let path = building.join("requirements.txt");
            std::fs::write(&path, &requirements.text)?;
            pip.arg("-r").arg(path);
        }
        None => {}
    }
    if let Some(constraints) = &requirements.constraints {
        let path = building.join("constraints.txt");
        std::fs::write(&path, constraints)?;
        pip.arg("-c").arg(path);
    }
    pip.args(PYTHON_SERVER);
    if let Err(e) = buildlog::run(&mut pip, "pip install", env, log) {
        let _ = std::fs::remove_dir_all(&building);
        return Err(e);
    }
    compile(&python, &building, &target, env, log)?;
    let _ = std::fs::remove_file(building.join("requirements.txt"));
    let _ = std::fs::remove_file(building.join("constraints.txt"));
    std::fs::write(building.join(PYTHON_DEPS_DONE), b"")?;
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::rename(&building, &cache).context("caching the Python requirements")?;
    Ok((cache.join("python"), false))
}

/// Run Django's `collectstatic` for the app in `app` (the package's copy), with its requirements
/// in `deps`, into `into` at its `STATIC_URL`'s path, as Vercel does: only when its settings set
/// `STATIC_ROOT` (or WhiteNoise's finders). A manifest storage's `staticfiles.json` goes back into
/// the app's own `STATIC_ROOT`, where it's read at run time.
#[allow(clippy::too_many_arguments)]
fn collect_static(python: &str, deps: &Path, app: &Path, settings: &str, into: &Path, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
    const SCRIPT: &str = r#"
import json, os, sys
into, app = sys.argv[1], sys.argv[2]
import django
from django.conf import settings
django.setup()
root, url = settings.STATIC_ROOT, settings.STATIC_URL or ""
finders = getattr(settings, "WHITENOISE_USE_FINDERS", False)
if not root and not finders:
    print(json.dumps({"skipped": "no STATIC_ROOT"})); sys.exit(0)
if "://" in url or url.startswith("//"):
    print(json.dumps({"skipped": "STATIC_URL is another host"})); sys.exit(0)
path = url.strip("/")
target = os.path.join(into, path) if path else into
settings.STATIC_ROOT = target
from django.core.management import call_command
call_command("collectstatic", interactive=False, verbosity=0)
rel = None
if root:
    try:
        rel = os.path.relpath(os.path.realpath(str(root)), os.path.realpath(app))
    except ValueError:
        rel = None
print(json.dumps({"path": path, "root": rel}))
"#;
    let mut command = Command::new(python);
    command.arg("-c").arg(SCRIPT).arg(into).arg(app).current_dir(app);
    let mut env = env.clone();
    env.insert("PYTHONPATH".to_owned(), format!("{}:{}", app.display(), deps.display()));
    env.insert("DJANGO_SETTINGS_MODULE".to_owned(), settings.to_owned());
    log.info(&format!("collecting Django's static files ({settings})"));
    let output = {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("AWS_") {
                command.env_remove(&key);
            }
        }
        command.envs(&env).stdin(std::process::Stdio::null()).output().context("running collectstatic")?
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines().filter(|l| !l.trim().is_empty()) {
        log.info(line);
    }
    if !output.status.success() {
        if native_python() {
            bail!("collecting Django's static files failed ({})", output.status);
        }
        // Away from the builder the requirements are Linux's, which may not load here.
        log.info("warning: Django's static files couldn't be collected on this machine (its requirements are built for Linux); deploy without --local to have them");
        return Ok(());
    }
    let answer: serde_json::Value = String::from_utf8_lossy(&output.stdout).lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap_or_default();
    if let Some(why) = answer.get("skipped").and_then(|s| s.as_str()) {
        log.info(&format!("no static files collected: {why}"));
        return Ok(());
    }
    let path = answer.get("path").and_then(|p| p.as_str()).unwrap_or("");
    let collected = if path.is_empty() { into.to_path_buf() } else { into.join(path) };
    if let Some(root) = answer.get("root").and_then(|r| r.as_str()).filter(|r| !r.starts_with("..") && !r.is_empty())
        && collected.join("staticfiles.json").is_file()
    {
        let keep = app.join(root);
        std::fs::create_dir_all(&keep)?;
        std::fs::copy(collected.join("staticfiles.json"), keep.join("staticfiles.json")).context("keeping staticfiles.json")?;
    }
    log.info(&format!("Django's static files collected for /{path}"));
    Ok(())
}

/// Compile `tree` to bytecode with paths as Lambda will see them (`/var/task/...` for `base`).
fn compile(python: &str, base: &Path, tree: &Path, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
    // Python's multiprocessing needs /dev/shm on Linux, which Lambda (the builder) doesn't have.
    let workers = if cfg!(target_os = "linux") && !Path::new("/dev/shm").is_dir() { "1" } else { "0" };
    buildlog::run(
        Command::new(python)
            .args(["-m", "compileall", "-q", "-j", workers, "--invalidation-mode", "unchecked-hash", "-s"])
            .arg(base)
            .args(["-p", "/var/task"])
            .arg(tree),
        "compiling bytecode",
        env,
        log,
    )
}

/// Every file under `root`, as `prefix/<relative path>`.
fn add_tree(entries: &mut BTreeMap<String, PathBuf>, root: &Path, prefix: &str) -> Result<()> {
    for entry in WalkBuilder::new(root).standard_filters(false).follow_links(false).build() {
        let entry = entry?;
        if entry.path().is_file() {
            let rel = entry.path().strip_prefix(root).expect("walked from root").to_string_lossy().replace('\\', "/");
            entries.insert(format!("{prefix}/{rel}"), entry.path().to_path_buf());
        }
    }
    Ok(())
}

/// What a zip entry holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A file on disk, read as it is.
    File(PathBuf),
    /// A symbolic link to this target, kept as a link: Lambda keeps a package's links (checked on
    /// AWS, C16), and pnpm's and Turbopack's `node_modules` are made of them.
    Link(String),
}

/// One entry of a package's zip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub source: Source,
    /// The time the entry is stamped with. None is the zip's fixed time (1980), which keeps a
    /// package of unchanged files the same bytes; a time is kept where the app reads it (Next.js
    /// takes a prerendered page's file time for when it was made).
    pub modified: Option<std::time::SystemTime>,
}

impl Entry {
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Entry { source: Source::File(path.into()), modified: None }
    }
}

/// Write the zip and return the total size of its files.
pub(crate) fn write_zip(path: &Path, entries: &BTreeMap<String, PathBuf>) -> Result<u64> {
    let entries: BTreeMap<String, Entry> = entries.iter().map(|(name, file)| (name.clone(), Entry::file(file))).collect();
    write_zip_entries(path, &entries)
}

/// `write_zip`, for entries that may be links or keep their own times.
pub(crate) fn write_zip_entries(path: &Path, entries: &BTreeMap<String, Entry>) -> Result<u64> {
    let mut zip = ZipWriter::new(BufWriter::new(File::create(path)?));
    let base = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .large_file(false);
    let mut total = 0u64;
    let mut buf = Vec::new();
    for (name, entry) in entries {
        let file = match &entry.source {
            Source::Link(target) => {
                total += target.len() as u64;
                zip.add_symlink(name, target, base)?;
                continue;
            }
            Source::File(file) => file,
        };
        let mode = if name == "bootstrap" || is_executable(file) { 0o755 } else { 0o644 };
        match entry.modified.and_then(zip_time) {
            Some((time, unix)) => {
                // The time twice: in the entry's own field, which has no zone (UTC here), and as
                // an extended timestamp, which is UTC by definition, for unzippers that take the
                // first for local time.
                let mut options = zip::write::FullFileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .last_modified_time(time)
                    .large_file(false)
                    .unix_permissions(mode);
                let mut field = vec![1u8];
                field.extend(unix.to_le_bytes());
                options.add_extra_field(0x5455, field, false)?;
                zip.start_file(name, options)?;
            }
            None => zip.start_file(name, base.unix_permissions(mode))?,
        }
        buf.clear();
        File::open(file).and_then(|mut f| f.read_to_end(&mut buf)).with_context(|| format!("reading {}", file.display()))?;
        total += buf.len() as u64;
        zip.write_all(&buf)?;
    }
    zip.finish()?.flush()?;
    Ok(total)
}

/// A file's time as a zip keeps it: to two seconds with no time zone (UTC here), and in Unix
/// seconds. None before 1980 or past 2107, which a zip can't say.
fn zip_time(time: std::time::SystemTime) -> Option<(DateTime, u32)> {
    use chrono::{Datelike, Timelike};
    let utc: chrono::DateTime<chrono::Utc> = time.into();
    let year = u16::try_from(utc.year()).ok()?;
    let dos = DateTime::from_date_and_time(year, utc.month() as u8, utc.day() as u8, utc.hour() as u8, utc.minute() as u8, utc.second() as u8).ok()?;
    Some((dos, u32::try_from(utc.timestamp()).ok()?))
}

/// A package made of entries the caller has laid out (their names relative to `app/`), with the
/// runtime as `bootstrap`: for a build whose files stay where they are and may be links (next.rs),
/// where `build_with` copies a project's files first.
pub fn build_entries(app: &BTreeMap<String, Entry>, opts: &Options, path: &Path) -> Result<Package> {
    check_bootstrap(&opts.bootstrap)?;
    let runtime = std::fs::read(&opts.bootstrap)?;
    let reads_sealed_env = forkrail_common::envfile::runtime_opens(&runtime);
    let app_aws_keys = forkrail_common::envfile::runtime_has(&runtime, forkrail_common::envfile::RUNTIME_AWS_KEYS_MARKER);
    drop(runtime);
    let mut entries: BTreeMap<String, Entry> = app.iter().map(|(name, entry)| (format!("app/{name}"), entry.clone())).collect();
    entries.insert("bootstrap".to_owned(), Entry::file(&opts.bootstrap));
    let unzipped = write_zip_entries(path, &entries)?;
    if unzipped > MAX_UNZIPPED {
        let _ = std::fs::remove_file(path);
        bail!("the package is {} MB unzipped, over Lambda's 250 MB", unzipped >> 20);
    }
    Ok(Package {
        zipped: std::fs::metadata(path)?.len(),
        sha256: forkrail_common::hex(&file_sha256(path)?),
        unzipped,
        entries: entries.len(),
        deps_cached: None,
        reads_sealed_env,
        app_aws_keys,
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    false
}

pub fn file_sha256(path: &Path) -> Result<[u8; 32]> {
    let mut reader = BufReader::new(File::open(path).with_context(|| format!("reading {}", path.display()))?);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Where Go's and Cargo's caches live between builds (native.rs).
pub fn native_cache() -> Result<PathBuf> {
    Ok(cache_dir()?.join("native"))
}

/// `$XDG_CACHE_HOME/forkrail`, or `~/.cache/forkrail`; on Windows `%LOCALAPPDATA%\forkrail`.
pub(crate) fn cache_dir() -> Result<PathBuf> {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = match var("XDG_CACHE_HOME") {
        Some(dir) => dir,
        None if cfg!(windows) => var("LOCALAPPDATA").context("LOCALAPPDATA isn't set")?,
        None => var("HOME").context("HOME isn't set")?.join(".cache"),
    };
    let dir = base.join("forkrail");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(what: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("forkrail-{what}-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&path)?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn collect_follows_links_only_inside_the_repository() {
        let outside = TempDir::new("collect-outside").unwrap();
        std::fs::write(outside.path().join("credentials"), "secret").unwrap();
        let repo = TempDir::new("collect-links").unwrap();
        let root = repo.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("app/api")).unwrap();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::write(root.join("app/api/main.py"), "").unwrap();
        std::fs::write(root.join("shared/config.json"), "{}").unwrap();
        std::os::unix::fs::symlink(root.join("shared/config.json"), root.join("app/config.json")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("credentials"), root.join("app/creds.txt")).unwrap();
        let files: Vec<String> = collect(&root.join("app"), Language::Python).unwrap().into_iter().map(|f| f.rel).collect();
        assert_eq!(files, ["api/main.py", "config.json"], "a link within the repository is followed; one out of it isn't");
    }

    #[test]
    fn collect_excludes_secrets_caches_and_vercelignore() {
        let dir = TempDir::new("collect-test").unwrap();
        let root = dir.path();
        for file in [
            "api/main.py",
            "api/__pycache__/main.cpython-313.pyc",
            "requirements.txt",
            ".env",
            ".env.local",
            ".git/HEAD",
            ".vercel/project.json",
            ".venv/bin/python",
            "node_modules/x/index.js",
            "notes/draft.md",
            "data/big.csv",
            "data/keep.json",
            ".vercelignore",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, file).unwrap();
        }
        std::fs::write(root.join(".vercelignore"), "notes/\n*.csv\n").unwrap();
        let files: Vec<String> = collect(root, Language::Python).unwrap().into_iter().map(|f| f.rel).collect();
        assert_eq!(files, ["api/main.py", "data/keep.json", "requirements.txt"]);
        // Node apps keep node_modules.
        let node: Vec<String> = collect(root, Language::Node).unwrap().into_iter().map(|f| f.rel).collect();
        assert!(node.contains(&"node_modules/x/index.js".to_owned()));
    }

    #[cfg(unix)]
    #[test]
    fn a_zip_keeps_links_and_the_times_it_is_given() {
        let dir = TempDir::new("zip-links").unwrap();
        let a = dir.path().join("a.txt");
        std::fs::write(&a, "hello").unwrap();
        // 2026-11-17 10:40:00 UTC.
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_794_912_000);
        let entries: BTreeMap<String, Entry> = [
            ("app/page.html".to_owned(), Entry { source: Source::File(a.clone()), modified: Some(when) }),
            ("app/node_modules/next".to_owned(), Entry { source: Source::Link(".pnpm/next@16/node_modules/next".to_owned()), modified: None }),
            ("app/plain.txt".to_owned(), Entry::file(&a)),
        ]
        .into();
        let path = dir.path().join("out.zip");
        assert_eq!(write_zip_entries(&path, &entries).unwrap(), 5 + 31 + 5);
        let mut zip = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
        {
            let mut link = zip.by_name("app/node_modules/next").unwrap();
            assert!(link.is_symlink());
            let mut target = String::new();
            link.read_to_string(&mut target).unwrap();
            assert_eq!(target, ".pnpm/next@16/node_modules/next");
        }
        {
            // The time in UTC in the entry's own field, and as an extended timestamp.
            let page = zip.by_name("app/page.html").unwrap();
            let time = page.last_modified().unwrap();
            assert_eq!((time.year(), time.month(), time.day(), time.hour(), time.minute(), time.second()), (2026, 11, 17, 10, 40, 0));
            let stamps: Vec<u32> = page
                .extra_data_fields()
                .filter_map(|f| match f {
                    zip::extra_fields::ExtraField::ExtendedTimestamp(t) => t.mod_time(),
                    _ => None,
                })
                .collect();
            assert_eq!(stamps, [1_794_912_000]);
        }
        // Without a time, the fixed one: such a package is the same bytes every build.
        assert_eq!(zip.by_name("app/plain.txt").unwrap().last_modified().unwrap().year(), 1980);
        // As the system unpacks it (as Lambda does): a link, and the file's own time.
        let run = dir.path().join("run");
        std::fs::create_dir_all(&run).unwrap();
        if Command::new("unzip").arg("-q").arg(&path).current_dir(&run).status().is_ok_and(|s| s.success()) {
            assert_eq!(std::fs::read_link(run.join("app/node_modules/next")).unwrap(), Path::new(".pnpm/next@16/node_modules/next"));
            assert_eq!(std::fs::metadata(run.join("app/page.html")).unwrap().modified().unwrap(), when);
        }
    }

    #[test]
    fn zip_is_deterministic() {
        let dir = TempDir::new("zip-test").unwrap();
        let a = dir.path().join("a.txt");
        std::fs::write(&a, "hello").unwrap();
        let entries: BTreeMap<String, PathBuf> = [("app/a.txt".to_owned(), a.clone()), ("bootstrap".to_owned(), a)].into();
        let (z1, z2) = (dir.path().join("1.zip"), dir.path().join("2.zip"));
        assert_eq!(write_zip(&z1, &entries).unwrap(), 10);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_zip(&z2, &entries).unwrap();
        assert_eq!(file_sha256(&z1).unwrap(), file_sha256(&z2).unwrap());
    }
}
