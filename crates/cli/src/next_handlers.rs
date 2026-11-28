//! Which server runs a Next.js build, and the package of forkrail's own (N8).
//!
//! Two servers can run the same build. Next's own standalone server ("S", next.rs) runs every
//! version forkrail takes. forkrail's server of Next's entrypoints ("H",
//! `crates/runtime/src/next/`) resolves each request with `@next/routing` and calls the build's
//! entrypoint handlers directly, as Vercel runs them: a smaller package and a faster start. It
//! is built against Next's internals, which move between releases, so it runs only the release
//! lines it has passed Next's deploy tests on (`verified.json` beside it) and only with the
//! `@next/routing` of exactly the app's Next version; every other build stays on S. A build can
//! also be kept on S by hand: `FORKRAIL_NEXT_SERVER=next` in the build's environment.
//!
//! H is made from what the same `next build` left for it (`next-handlers.json`, `next::Handlers`):
//! every entrypoint with its traced files, plus what the entrypoints read without importing it
//! (the dist folder's manifests and `BUILD_ID`, the prerendered files, an Edge entry's files) and
//! what this server loads of Next (`@next/env`, the Edge sandbox, `instrumentation`'s runner, the
//! image optimizer), traced with Next's own tracer. Where S's package already has a file, H takes
//! S's entry (the link as a link, the file's time, sharp for Linux, the seed's choices), so the
//! two packages agree file by file. H then replaces S's entries: one package, at the same entry
//! name (`next::ENTRY`), so publishing and the routers treat both alike; `build.json` says which
//! (`NextBuild.server`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::buildlog::BuildLog;
use crate::next::{Built, Handlers};
use crate::package::{self, Entry, Source, TempDir};

/// The build's environment variable that keeps a build on Next's own server (`next`).
pub const SERVER_ENV: &str = "FORKRAIL_NEXT_SERVER";
/// The H server's own folder in the package (`app/_forkrail_next/`).
const SERVER_DIR: &str = "_forkrail_next";
/// H's manifest, at the package's root: what `server.mjs` routes by.
const MANIFEST: &str = "forkrail-next.json";
const TRACE_JS: &str = include_str!("next_handlers.cjs");
const VERIFIED: &str = include_str!("../../runtime/src/next/verified.json");
/// The server's files, as they go in the package (tests stay out).
const SERVER_FILES: &[(&str, &str)] = &[
    ("main.mjs", include_str!("../../runtime/src/next/main.mjs")),
    ("server.mjs", include_str!("../../runtime/src/next/server.mjs")),
    ("routing.mjs", include_str!("../../runtime/src/next/routing.mjs")),
    ("compress.mjs", include_str!("../../runtime/src/next/compress.mjs")),
    ("edge.mjs", include_str!("../../runtime/src/next/edge.mjs")),
    ("image.mjs", include_str!("../../runtime/src/next/image.mjs")),
    ("statics.mjs", include_str!("../../runtime/src/next/statics.mjs")),
    ("verified.json", VERIFIED),
];

/// Which server a Next build runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Server {
    /// Next's own standalone server (`server.js`).
    #[default]
    Next,
    /// forkrail's server of Next's entrypoints.
    Direct,
}

impl Server {
    pub fn is_next(&self) -> bool {
        *self == Server::Next
    }

    /// For the build log's line on the package.
    pub fn describe(&self) -> &'static str {
        match self {
            Server::Next => "Next's standalone server",
            Server::Direct => "Next's entrypoints on forkrail's server",
        }
    }
}

/// What the tracer found (`next_handlers.cjs`).
#[derive(Debug, Default, Deserialize)]
pub struct Traced {
    pub files: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub routing: Option<Installed>,
}

#[derive(Debug, Deserialize)]
pub struct Installed {
    pub dir: PathBuf,
    pub version: String,
}

/// What the tracer is asked for.
#[derive(Debug, Serialize)]
pub struct Wanted<'a> {
    #[serde(rename = "projectDir")]
    pub project_dir: &'a Path,
    #[serde(rename = "repoRoot")]
    pub repo_root: &'a Path,
    pub edge: bool,
    pub instrumentation: bool,
    pub images: bool,
}

type Trace<'a> = &'a dyn Fn(&Wanted) -> Result<Traced>;
type FetchRouting<'a> = &'a dyn Fn(&str, &dyn BuildLog) -> Result<PathBuf>;

/// Decide the server for the Next build `built` (just made by `next::collect` from the build
/// folder `out`) and, for H, put H's package in place of S's. Says why in the log either way.
pub fn choose(out: &Path, built: &mut Built, log: &dyn BuildLog) -> Result<Server> {
    let wanted = std::env::var(SERVER_ENV).unwrap_or_default();
    choose_with(out, built, &wanted, &trace, &fetch_routing, log)
}

fn choose_with(out: &Path, built: &mut Built, wanted: &str, trace: Trace, fetch: FetchRouting, log: &dyn BuildLog) -> Result<Server> {
    let version = built.manifest.next_version.clone();
    match wanted {
        "" | "auto" | "direct" => {}
        "next" => {
            log.info(&format!("Next.js server: Next's own ({SERVER_ENV}=next)"));
            return Ok(Server::Next);
        }
        other => bail!("{SERVER_ENV} must be `next` or `direct`, not {other:?}"),
    }
    let verified: Value = serde_json::from_str(VERIFIED).expect("verified.json is JSON");
    let lines: Vec<&str> = verified.get("next").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let refuse = |why: String| -> Result<Server> {
        if wanted == "direct" {
            bail!("{SERVER_ENV}=direct, but forkrail's server can't run this build: {why}");
        }
        log.info(&format!("Next.js server: Next's own ({why})"));
        Ok(Server::Next)
    };
    if let Err(why) = verdict(&version, &lines) {
        return refuse(why);
    }
    let Some(handlers) = Handlers::read(out)? else {
        return refuse("the build left no list of its entrypoints".to_owned());
    };
    if handlers.0.get("staticFiles").and_then(Value::as_array).is_none() {
        return refuse("the build's list of entrypoints has no static files (made by an older forkrail)".to_owned());
    }
    let scratch = keep_dir(out_of(built))?;
    // The files of `public/`, which Next's adapter leaves out of its list (they are the platform's).
    let public: Vec<String> = built.manifest.static_files.iter().filter(|f| !f.immutable).map(|f| f.pathname.clone()).collect();
    let made = match package_with(&handlers, &built.entries, &public, built.manifest.images.as_ref(), &scratch, trace, fetch, log) {
        Ok(Some(made)) => made,
        Ok(None) => return refuse(format!("@next/routing {version} couldn't be had")),
        Err(e) => return refuse(format!("its package couldn't be made: {e:#}")),
    };
    // The entry is where S's is (`next::Manifest::entry`: beside `server.js`, so under the
    // project's path in a workspace), which is what the deployment starts.
    let mut made = made;
    if built.manifest.entry != crate::next::ENTRY {
        made.remove(crate::next::ENTRY);
        let launcher = scratch.join("entry.mjs");
        std::fs::write(&launcher, launcher_js(&built.manifest.entry))?;
        made.insert(built.manifest.entry.clone(), Entry::file(launcher));
    }
    let (sha, files) = hash_entries(&made)?;
    log.info(&format!(
        "Next.js server: forkrail's own, calling Next's entrypoints directly ({} files in place of Next's server's {})",
        files, built.source_files
    ));
    built.entries = made;
    built.source_sha256 = sha;
    built.source_files = files;
    Ok(Server::Direct)
}

/// Is `version` on a line H has been verified on? Prereleases never are: their internals move
/// between builds. The same rule as `versionVerdict` in routing.mjs, which the server applies
/// again at its start.
pub fn verdict(version: &str, verified: &[&str]) -> std::result::Result<(), String> {
    let (core, pre) = version.split_once('-').map_or((version, false), |(core, _)| (core, true));
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
        return Err(format!("\"{version}\" isn't a version"));
    }
    if pre {
        return Err(format!("Next {version} is a prerelease, which forkrail's server isn't verified on"));
    }
    let line = format!("{}.{}", parts[0], parts[1]);
    if !verified.contains(&line.as_str()) {
        return Err(format!("forkrail's server is verified on Next {}, not {line}", verified.join(", ")));
    }
    Ok(())
}

/// H's entries, by their path in `app/`, from the handlers file, S's entries `s`, the paths the
/// files of `public/` are served at (with the base path) and the build's image settings; the
/// files it writes go in `scratch`. None when `@next/routing` couldn't be had.
#[allow(clippy::too_many_arguments)]
fn package_with(handlers: &Handlers, s: &BTreeMap<String, Entry>, public: &[String], images: Option<&Value>, scratch: &Path, trace: Trace, fetch: FetchRouting, log: &dyn BuildLog) -> Result<Option<BTreeMap<String, Entry>>> {
    let h = &handlers.0;
    let text = |key: &str| h.get(key).and_then(Value::as_str).unwrap_or_default();
    let repo_root = PathBuf::from(text("repoRoot"));
    let project = text("projectDir").trim_matches('/').to_owned();
    // `distDir` is the project's (`../.next` for a project in `apps/web` whose next.config says
    // so): its place from the repository's root, `..` resolved, is how entries are named.
    let dist = normalize(&join(&project, text("distDir")));
    let dist_dir = repo_root.join(&dist);
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    // S's entry where it has the file, else the file where the build left it.
    let take = |name: &str, from: &Path, entries: &mut BTreeMap<String, Entry>| {
        if entries.contains_key(name) {
            return Ok::<_, anyhow::Error>(());
        }
        if let Some(entry) = s.get(name) {
            entries.insert(name.to_owned(), entry.clone());
        } else if let Some(entry) = on_disk(from)? {
            entries.insert(name.to_owned(), entry);
        }
        Ok(())
    };

    // The entrypoints and what Next traced for them.
    for (name, from) in handlers.files(&repo_root) {
        take(&name, &from, &mut entries)?;
    }
    // What the entrypoints read beside them, as S's package has it (the seed's choices kept):
    // the dist folder's `BUILD_ID` and manifests, the files directly in `server/` (the manifests'
    // `.js` forms), the prerendered pages and route answers, the scoped cache's files.
    let dist_prefix = format!("{dist}/");
    for (name, entry) in s {
        let Some(rel) = name.strip_prefix(&dist_prefix) else { continue };
        let top = !rel.contains('/') && (rel == "BUILD_ID" || rel.ends_with(".json") || rel.ends_with(".js")) && rel != "forkrail-next.json";
        let server_file = rel.strip_prefix("server/").is_some_and(|r| !r.contains('/'));
        let prerendered = (rel.starts_with("server/app/") || rel.starts_with("server/pages/")) && [".html", ".rsc", ".meta", ".body", ".json"].iter().any(|ext| rel.ends_with(ext));
        if top || server_file || prerendered || rel.starts_with("server/route-cache/") {
            entries.entry(name.clone()).or_insert_with(|| entry.clone());
        }
    }
    // The static files S's package keeps for Next's server (one the middleware may match, an
    // image Next's optimizer reads: next.rs decides): requests for them reach this server
    // instead, so its package holds the same ones. Without them, an app whose middleware matches
    // every path got a 404 for each of its scripts and styles.
    let files = static_files(h, &project, public);
    for (_, file) in &files {
        if let Some(entry) = s.get(file) {
            entries.entry(file.clone()).or_insert_with(|| entry.clone());
        }
    }
    // The project's package.json, as Next's standalone folder has it.
    let package_json = join(&project, "package.json");
    take(&package_json, &repo_root.join(&package_json), &mut entries)?;
    // The fetch cache `next build` made (`fetch` with `force-cache` while prerendering), which
    // Next's own server would read at run time.
    for (rel, abs) in files_under(&dist_dir.join("cache/fetch-cache"))? {
        take(&format!("{dist}/cache/fetch-cache/{rel}"), &abs, &mut entries)?;
    }
    // A prerender's fallback file, where S's package keeps it: a fallback or a shell, served for
    // what wasn't built ahead. For a page built ahead it's the page itself, which left S's package
    // with the shared cache's seed (`next::collect_with`) and is read from the cache: taking it
    // from the build folder put every seeded page back (a 328 MB package for Next's
    // segment-cache/memory-pressure test, whose 63 pages S had left out).
    for prerender in h.get("prerenders").and_then(Value::as_array).into_iter().flatten() {
        if let Some(file) = prerender.pointer("/fallback/file").and_then(Value::as_str) {
            let name = normalize(&join(&project, file));
            if let Some(entry) = s.get(&name) {
                entries.entry(name).or_insert_with(|| entry.clone());
            }
        }
    }

    // An Edge entry is loaded by Next's sandbox from the file list in `middleware-manifest.json`
    // (its chunks, the page's client-reference manifest, wasm, assets): more than its own trace.
    let outputs: Vec<&Value> = h.get("outputs").and_then(Value::as_array).into_iter().flatten().chain(h.get("middleware").filter(|m| m.is_object())).collect();
    let edge = outputs.iter().any(|o| o.get("runtime").and_then(Value::as_str) == Some("edge"));
    if edge && let Ok(bytes) = std::fs::read(dist_dir.join("server/middleware-manifest.json")) {
        let manifest: Value = serde_json::from_slice(&bytes).context("middleware-manifest.json isn't JSON")?;
        for function in ["middleware", "functions"].iter().filter_map(|k| manifest.get(*k).and_then(Value::as_object)).flat_map(|m| m.values()) {
            let listed = function.get("files").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
            let paths = ["wasm", "assets"].iter().filter_map(|k| function.get(*k).and_then(Value::as_array)).flatten().filter_map(|a| a.get("filePath").and_then(Value::as_str));
            for rel in listed.chain(paths) {
                let name = format!("{dist}/{rel}");
                take(&name, &repo_root.join(&name), &mut entries)?;
            }
        }
    }

    // What this server loads of Next beyond the entrypoints.
    let images = optimizes_images(images);
    let project_dir = repo_root.join(&project);
    let traced = trace(&Wanted { project_dir: &project_dir, repo_root: &repo_root, edge, instrumentation: dist_dir.join("server/instrumentation.js").is_file(), images })?;
    for name in traced.files.iter().chain(&traced.links) {
        take(name, &repo_root.join(name), &mut entries)?;
    }
    if images {
        // sharp as S's package has it: Linux's binaries in place of this machine's (next.rs).
        for (name, entry) in s {
            if name.contains("node_modules/sharp/") || name.contains("node_modules/@img/") {
                entries.entry(name.clone()).or_insert_with(|| entry.clone());
            }
        }
        entries.retain(|name, _| !(name.contains("node_modules/@img/") && !s.contains_key(name)));
        // sharp's WebAssembly build (9 MB) is the fallback it tries only after its Linux arm64
        // binary fails to load (sharp's `lib/sharp`): with that binary here, the fallback is left
        // out. A smaller package is a shorter start (Lambda fetches the whole zip first).
        if entries.keys().any(|name| name.contains("@img/sharp-linux-arm64/")) {
            entries.retain(|name, _| !name.contains("@img/sharp-wasm32") && !name.contains("@img+sharp-wasm32@"));
        }
    }

    // `@next/routing` of exactly the app's version: the project's own, or npm's.
    let version = text("nextVersion");
    let routing = match traced.routing {
        Some(installed) if installed.version == version => installed.dir,
        _ => match fetch(version, log) {
            Ok(dir) => dir,
            Err(e) => {
                log.info(&format!("warning: @next/routing {version} couldn't be fetched ({e:#})"));
                return Ok(None);
            }
        },
    };
    for rel in ["package.json", "dist/index.js"] {
        let from = routing.join(rel);
        if !from.is_file() {
            log.info(&format!("warning: {} has no {rel}", routing.display()));
            return Ok(None);
        }
        entries.insert(format!("{SERVER_DIR}/node_modules/@next/routing/{rel}"), Entry::file(from));
    }

    // The server itself, its manifest, and the entry.
    for (name, text) in SERVER_FILES {
        let path = scratch.join(name);
        std::fs::write(&path, text)?;
        entries.insert(format!("{SERVER_DIR}/{name}"), Entry::file(path));
    }
    let statics = statics(&files, &entries);
    let config = std::fs::read(dist_dir.join("required-server-files.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|r| r.get("config").cloned())
        .unwrap_or(Value::Null);
    let manifest = manifest(h, &config, statics);
    let path = scratch.join(MANIFEST);
    std::fs::write(&path, serde_json::to_vec(&manifest)?)?;
    entries.insert(MANIFEST.to_owned(), Entry::file(path));
    let launcher = scratch.join("next.mjs");
    std::fs::write(&launcher, launcher_js(crate::next::ENTRY))?;
    entries.insert(crate::next::ENTRY.to_owned(), Entry::file(launcher));
    Ok(Some(entries))
}

/// The entry at `entry` (a path in the package's `app/`), which starts the server in
/// `_forkrail_next/` at the package's top.
fn launcher_js(entry: &str) -> String {
    let up = entry.matches('/').count();
    let to = if up == 0 { "./".to_owned() } else { "../".repeat(up) };
    format!("// forkrail's entry for Next.js: Next's entrypoints on forkrail's server ({SERVER_DIR}/).\nimport '{to}{SERVER_DIR}/main.mjs'\n")
}

/// The manifest `server.mjs` routes by: Next's routing and outputs as its adapter handed them,
/// keyed by path (with the base path, as Next names them), and what Next's own server takes from
/// the config at its start.
fn manifest(h: &Value, config: &Value, statics: Map<String, Value>) -> Value {
    let repo_root = h.get("repoRoot").and_then(Value::as_str).unwrap_or_default();
    let relative = |p: &str| p.strip_prefix(repo_root).map(|r| r.trim_start_matches('/')).unwrap_or(p).to_owned();
    let entry = |o: &Value| -> Value {
        let kind = match o.get("kind").and_then(Value::as_str).unwrap_or_default() {
            "pages" => "PAGES",
            "pagesApi" => "PAGES_API",
            "appPages" => "APP_PAGE",
            "appRoutes" => "APP_ROUTE",
            _ => "MIDDLEWARE",
        };
        let mut e = json!({
            "id": o.get("id"),
            "type": kind,
            "file": o.get("file"),
            "runtime": o.get("runtime"),
            "sourcePage": o.get("sourcePage"),
        });
        let fields = e.as_object_mut().expect("an object");
        if let Some(edge) = o.get("edgeRuntime").filter(|v| v.is_object()) {
            fields.insert(
                "edge".to_owned(),
                json!({ "modulePath": relative(edge.get("modulePath").and_then(Value::as_str).unwrap_or_default()), "entryKey": edge.get("entryKey"), "handlerExport": edge.get("handlerExport") }),
            );
        }
        if let Some(env) = o.pointer("/config/env").filter(|v| v.is_object()) {
            fields.insert("env".to_owned(), env.clone());
        }
        if let Some(wasm) = o.get("wasmAssets").filter(|v| v.as_object().is_some_and(|w| !w.is_empty())) {
            fields.insert("wasm".to_owned(), wasm.clone());
        }
        if let Some(max) = o.pointer("/config/maxDuration").filter(|v| v.is_number()) {
            fields.insert("maxDuration".to_owned(), max.clone());
        }
        e
    };
    let mut functions = Map::new();
    for o in h.get("outputs").and_then(Value::as_array).into_iter().flatten() {
        if let Some(pathname) = o.get("pathname").and_then(Value::as_str) {
            functions.insert(pathname.to_owned(), entry(o));
        }
    }
    let middleware = h.get("middleware").filter(|m| m.is_object()).map(|o| {
        let mut m = entry(o);
        m["matchers"] = o.pointer("/config/matchers").cloned().unwrap_or(json!([]));
        m
    });
    // A page with prerenders is statically generated (`getStaticProps`, a prerendered App
    // Router page or route), which Next's server treats differently; a localized page is one
    // module under a name per locale, so it is a property of the module.
    let prerenders: Vec<&Value> = h.get("prerenders").and_then(Value::as_array).into_iter().flatten().collect();
    let parents: std::collections::BTreeSet<&str> = prerenders.iter().filter_map(|p| p.get("parentOutputId").and_then(Value::as_str)).collect();
    let ssg_files: std::collections::BTreeSet<String> = functions.values().filter(|f| f["id"].as_str().is_some_and(|id| parents.contains(id))).filter_map(|f| f["file"].as_str().map(str::to_owned)).collect();
    for f in functions.values_mut() {
        if f["file"].as_str().is_some_and(|file| ssg_files.contains(file)) {
            f["ssg"] = json!(true);
        }
    }
    let mut by_path = Map::new();
    for p in &prerenders {
        let Some(pathname) = p.get("pathname").and_then(Value::as_str) else { continue };
        let mut e = json!({ "parent": p.get("parentOutputId") });
        if let Some(t) = p.get("routeType").filter(|v| !v.is_null()) {
            e["routeType"] = t.clone();
        }
        if let Some(r) = p.get("renderingMode").filter(|v| !v.is_null()) {
            e["renderingMode"] = r.clone();
        }
        by_path.insert(pathname.to_owned(), e);
    }
    let flag = |v: Option<&Value>, default: bool| v.and_then(Value::as_bool).unwrap_or(default);
    json!({
        "version": 1,
        "nextVersion": h.get("nextVersion"),
        "buildId": h.get("buildId"),
        "deploymentId": config.get("deploymentId").and_then(Value::as_str).unwrap_or_default(),
        "projectDir": match h.get("projectDir").and_then(Value::as_str).unwrap_or_default().trim_matches('/') { "" => ".", p => p },
        "distDir": h.get("distDir"),
        "basePath": h.get("basePath").and_then(Value::as_str).unwrap_or_default(),
        "trailingSlash": flag(h.get("trailingSlash"), false),
        "i18n": h.get("i18n").filter(|v| v.is_object()),
        "skipProxyUrlNormalize": flag(config.get("skipProxyUrlNormalize").or_else(|| config.get("skipMiddlewareUrlNormalize")), false),
        "validateRsc": flag(config.pointer("/experimental/validateRSCRequestHeaders"), true),
        "compress": flag(config.get("compress"), true),
        "env": config.get("env").filter(|v| v.is_object()).cloned().unwrap_or(json!({})),
        "poweredByHeader": flag(config.get("poweredByHeader"), true),
        "immutableAssets": flag(config.get("supportsImmutableAssets"), false),
        "routing": h.get("routing"),
        "functions": functions,
        "middleware": middleware,
        "prerenders": by_path,
        "statics": statics,
    })
}

/// Every static file, as (the path it's served at, its name in the package): those Next listed
/// (`.next/static/`, pages written as files, static metadata routes), then the files of
/// `public/` (`public`: their paths with the base path), which Next's adapter leaves to the
/// platform. Next's own entry wins where both name a path.
fn static_files(h: &Value, project: &str, public: &[String]) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = Vec::new();
    for pair in h.get("staticFiles").and_then(Value::as_array).into_iter().flatten() {
        if let (Some(pathname), Some(file)) = (pair.get(0).and_then(Value::as_str), pair.get(1).and_then(Value::as_str)) {
            files.push((pathname.to_owned(), file.to_owned()));
        }
    }
    let listed: std::collections::BTreeSet<String> = files.iter().map(|(pathname, _)| pathname.clone()).collect();
    let base = h.get("basePath").and_then(Value::as_str).unwrap_or_default().trim_end_matches('/');
    for pathname in public {
        if let Some(rel) = pathname.strip_prefix(base).filter(|rel| rel.starts_with('/'))
            && !listed.contains(pathname)
        {
            files.push((pathname.clone(), join(project, &format!("public{rel}"))));
        }
    }
    files
}

/// Every static file by the path it's served at: its place in the package, or null where the
/// package doesn't hold it (the router serves it). Routing must know them all: a rewrite never
/// shadows a file.
fn statics(files: &[(String, String)], entries: &BTreeMap<String, Entry>) -> Map<String, Value> {
    let mut out = Map::new();
    for (pathname, file) in files {
        out.insert(pathname.clone(), if entries.contains_key(file) { json!(file) } else { Value::Null });
    }
    out
}

/// A file or link where the build left it, as a package entry: a link stays a link when it
/// points within the repository, as `next::walk` keeps them; None for what isn't there.
fn on_disk(path: &Path) -> Result<Option<Entry>> {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return Ok(None) };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).with_context(|| format!("reading the link {}", path.display()))?;
        if target.is_relative() {
            return Ok(Some(Entry { source: Source::Link(target.to_string_lossy().replace('\\', "/")), modified: None }));
        }
        return Ok(std::fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| Entry { source: Source::File(path.to_path_buf()), modified: m.modified().ok() }));
    }
    Ok(meta.is_file().then(|| Entry { source: Source::File(path.to_path_buf()), modified: meta.modified().ok() }))
}

/// The files under `dir`, by their path from it.
fn files_under(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((at, prefix)) = stack.pop() {
        for item in std::fs::read_dir(&at).with_context(|| format!("reading {}", at.display()))? {
            let item = item?;
            let name = join(&prefix, &item.file_name().to_string_lossy());
            if item.file_type()?.is_dir() {
                stack.push((item.path(), name));
            } else {
                out.push((name, item.path()));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The same as next.rs's: does Next's own optimizer answer `/_next/image`?
fn optimizes_images(images: Option<&Value>) -> bool {
    let Some(images) = images else { return true };
    images.get("loader").and_then(Value::as_str).unwrap_or("default") == "default" && images.get("unoptimized").and_then(Value::as_bool) != Some(true)
}

/// The hash of what the package holds and how many files, as next.rs counts S's.
fn hash_entries(entries: &BTreeMap<String, Entry>) -> Result<(String, usize)> {
    let mut hasher = Sha256::new();
    let mut files = 0;
    for (name, entry) in entries {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        match &entry.source {
            Source::File(path) => {
                hasher.update(package::file_sha256(path)?);
                files += 1;
            }
            Source::Link(target) => {
                hasher.update([1]);
                hasher.update(target.as_bytes());
            }
        }
    }
    Ok((forkrail_common::hex(&hasher.finalize()), files))
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        _ => format!("{}/{}", a.trim_end_matches('/'), b.trim_start_matches('/')),
    }
}

/// A relative path with `.` and `..` resolved (`apps/web/../.next` is `apps/.next`). A `..` past
/// the start is kept: nothing the build names lies outside the repository.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// A folder next to S's scratch files, removed with the process's other temporary folders:
/// `Built` keeps S's scratch folder alive for as long as its entries are used.
fn out_of(built: &Built) -> &Path {
    built.static_dir.parent().unwrap_or(&built.static_dir)
}

fn keep_dir(parent: &Path) -> Result<PathBuf> {
    let dir = parent.join("next-direct");
    std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
    Ok(dir)
}

/// Run the tracer with the build machine's Node.
fn trace(wanted: &Wanted) -> Result<Traced> {
    let scratch = TempDir::new("next-trace")?;
    let script = scratch.path().join("trace.cjs");
    std::fs::write(&script, TRACE_JS)?;
    let output = Command::new("node")
        .arg(&script)
        .arg(serde_json::to_string(wanted)?)
        .current_dir(wanted.project_dir)
        .stdin(std::process::Stdio::null())
        .output()
        .context("running node")?;
    if !output.status.success() {
        bail!("tracing Next's server files failed: {}", String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("").trim());
    }
    serde_json::from_slice(&output.stdout).context("the tracer's answer isn't JSON")
}

/// `@next/routing` at `version`, from npm, kept in forkrail's cache folder for the next build.
fn fetch_routing(version: &str, log: &dyn BuildLog) -> Result<PathBuf> {
    if !version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')) {
        bail!("{version:?} isn't a version to fetch");
    }
    let cache = package::native_cache()?.join("next-routing").join(version);
    let folder = cache.join("node_modules/@next/routing");
    if cache.join(".complete").is_file() && folder.is_dir() {
        return Ok(folder);
    }
    log.info(&format!("fetching @next/routing {version}"));
    let building = cache.with_extension(format!("tmp-{}", forkrail_common::random_hex(4)));
    std::fs::create_dir_all(&building)?;
    let output = Command::new("npm")
        .args(["install", "--no-save", "--ignore-scripts", "--no-audit", "--no-fund", "--prefix"])
        .arg(&building)
        .arg(format!("@next/routing@{version}"))
        .stdin(std::process::Stdio::null())
        .output()
        .context("running npm")?;
    if !output.status.success() || !building.join("node_modules/@next/routing/dist/index.js").is_file() {
        let _ = std::fs::remove_dir_all(&building);
        bail!("npm install @next/routing@{version} failed: {}", String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or("").trim());
    }
    std::fs::write(building.join(".complete"), b"")?;
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::create_dir_all(cache.parent().expect("has a folder"))?;
    std::fs::rename(&building, &cache).context("keeping the fetched @next/routing")?;
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Quiet;

    impl BuildLog for Quiet {
        fn line(&self, _: crate::buildlog::Stream, _: &str) {}
    }

    fn write(dir: &Path, name: &str, text: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn only_stable_releases_of_verified_lines_run_on_forkrails_server() {
        assert_eq!(verdict("16.4.6", &["16.4"]), Ok(()));
        assert!(verdict("16.4.0-canary.48", &["16.4"]).unwrap_err().contains("prerelease"));
        assert!(verdict("16.3.2", &["16.4"]).unwrap_err().contains("verified on Next 16.4, not 16.3"));
        assert!(verdict("16.5.0", &["16.4"]).is_err());
        assert!(verdict("16", &["16.4"]).unwrap_err().contains("isn't a version"));
        // The list the server itself checks at its start.
        let verified: Value = serde_json::from_str(VERIFIED).unwrap();
        assert!(verified["next"].as_array().is_some_and(|lines| !lines.is_empty()));
    }

    #[test]
    fn a_dist_folder_above_the_project_is_named_from_the_root() {
        assert_eq!(normalize(&join("apps/next-nx-test", "../.next")), "apps/.next");
        assert_eq!(normalize(&join("web", ".next")), "web/.next");
        assert_eq!(normalize(&join("", ".next")), ".next");
        assert_eq!(normalize("./a/./b/../c"), "a/c");
    }

    #[test]
    fn public_files_are_static_files_under_the_base_path() {
        let h = json!({ "basePath": "/docs", "staticFiles": [["/docs/_next/static/a.js", "web/.next/static/a.js"], ["/docs/404", "web/.next/server/pages/404.html"]] });
        let public = ["/docs/robots.txt".to_owned(), "/docs/img/a.png".to_owned(), "/docs/404".to_owned()];
        assert_eq!(
            static_files(&h, "web", &public),
            [
                ("/docs/_next/static/a.js".to_owned(), "web/.next/static/a.js".to_owned()),
                ("/docs/404".to_owned(), "web/.next/server/pages/404.html".to_owned()),
                ("/docs/robots.txt".to_owned(), "web/public/robots.txt".to_owned()),
                ("/docs/img/a.png".to_owned(), "web/public/img/a.png".to_owned()),
            ]
        );
        // At the top of the repository, no base path.
        assert_eq!(static_files(&json!({ "basePath": "" }), "", &["/a.txt".to_owned()]), [("/a.txt".to_owned(), "public/a.txt".to_owned())]);
    }

    #[test]
    fn the_package_is_the_entrypoints_what_they_read_and_the_server() {
        let repo = TempDir::new("next-direct-test").unwrap();
        let root = repo.path();
        let scratch = root.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        for name in [
            "web/.next/server/app/page.js",
            "web/.next/server/app/api/edge/route.js",
            "web/.next/server/edge/chunk.js",
            "web/.next/cache/fetch-cache/abc",
            "web/public/a.txt",
            "web/public/kept.txt",
            "web/.next/static/chunks/kept.js",
            "web/.next/static/chunks/router.js",
            "web/.next/server/app/built.html",
            "web/.next/server/app/[slug].html",
            "web/package.json",
            "node_modules/next/dist/shared.js",
            "node_modules/@next/env/dist/index.js",
            "node_modules/@img/sharp-darwin-arm64/lib/sharp.node",
        ] {
            write(root, name, "x");
        }
        write(root, "web/.next/server/middleware-manifest.json", r#"{"functions": {"/api/edge": {"files": ["server/edge/chunk.js"], "wasm": [], "assets": []}}}"#);
        write(root, "web/.next/required-server-files.json", r#"{"config": {"compress": false, "env": {"A": "1"}, "deploymentId": "dpl", "poweredByHeader": false}}"#);
        let routing_dir = root.join("routing");
        write(&routing_dir, "package.json", r#"{"version": "16.4.6"}"#);
        write(&routing_dir, "dist/index.js", "module.exports = {}");
        // S's package: Next's standalone folder, as next.rs laid it out.
        let file = |name: &str| (name.to_owned(), Entry::file(root.join(name)));
        let s: BTreeMap<String, Entry> = [
            file("web/server.js"),
            file("web/.next/BUILD_ID"),
            file("web/.next/routes-manifest.json"),
            file("web/.next/forkrail-next.json"),
            file("web/.next/server/pages-manifest.json"),
            file("web/.next/server/app/index.html"),
            file("web/.next/server/app/page.js"),
            file("web/.next/server/app/[slug].html"),
            file("web/.next/server/route-cache/k/$/icon.png.body"),
            // What next.rs kept for Next's server (the middleware may match them).
            file("web/.next/static/chunks/kept.js"),
            file("web/public/kept.txt"),
            file("node_modules/next/dist/server/next-server.js"),
            ("node_modules/next/dist/shared.js".to_owned(), Entry { source: Source::Link("../../.pnpm/shared.js".to_owned()), modified: None }),
            file("node_modules/@img/sharp-linux-arm64/lib/sharp.node"),
            file("node_modules/@img/sharp-wasm32/lib/sharp-wasm32.node.wasm"),
        ]
        .into_iter()
        .collect();
        let handlers = Handlers(json!({
            "repoRoot": root, "projectDir": "web", "distDir": ".next", "nextVersion": "16.4.6", "buildId": "b1",
            "basePath": "", "trailingSlash": false, "i18n": null, "routing": { "beforeMiddleware": [] },
            "outputs": [
                { "kind": "appPages", "id": "/page", "pathname": "/", "file": "web/.next/server/app/page.js", "runtime": "nodejs", "config": { "maxDuration": 30 }, "assets": [0] },
                { "kind": "appRoutes", "id": "/api/edge/route", "pathname": "/api/edge", "file": "web/.next/server/app/api/edge/route.js", "runtime": "edge",
                  "edgeRuntime": { "modulePath": root.join("web/.next/server/app/api/edge/route.js"), "entryKey": "middleware_app/api/edge/route", "handlerExport": "handler" },
                  "config": { "env": { "KEY": "k" } }, "assets": [] },
            ],
            "middleware": null,
            "files": ["node_modules/next/dist/shared.js"],
            "prerenders": [
                { "pathname": "/", "parentOutputId": "/page", "renderingMode": "STATIC" },
                // A page built ahead, seeded (S left its file out), and a shell S kept.
                { "pathname": "/built", "parentOutputId": "/page", "fallback": { "file": ".next/server/app/built.html" } },
                { "pathname": "/[slug]", "parentOutputId": "/page", "fallback": { "file": ".next/server/app/[slug].html" } },
            ],
            // As Next's adapter lists them: never the files of `public/`.
            "staticFiles": [
                ["/_next/static/chunks/kept.js", "web/.next/static/chunks/kept.js"],
                ["/_next/static/chunks/router.js", "web/.next/static/chunks/router.js"],
                ["/icon.png", "web/.next/server/route-cache/k/$/icon.png.body"],
            ],
        }));
        let asked = std::cell::RefCell::new(None);
        let trace = |wanted: &Wanted| {
            *asked.borrow_mut() = Some((wanted.edge, wanted.instrumentation, wanted.images));
            Ok(Traced { files: vec!["node_modules/@next/env/dist/index.js".to_owned(), "node_modules/@img/sharp-darwin-arm64/lib/sharp.node".to_owned()], links: vec![], routing: None })
        };
        let fetched = |version: &str, _: &dyn BuildLog| {
            assert_eq!(version, "16.4.6");
            Ok(routing_dir.clone())
        };
        let public = ["/a.txt".to_owned(), "/kept.txt".to_owned()];
        let entries = package_with(&handlers, &s, &public, None, &scratch, &trace, &fetched, &Quiet).unwrap().unwrap();
        assert_eq!(*asked.borrow(), Some((true, false, true)));

        let has = |name: &str| entries.contains_key(name);
        // The entrypoints and their traces, with S's entry where it has one (the link stays a link).
        assert!(has("web/.next/server/app/page.js") && has("web/.next/server/app/api/edge/route.js"));
        assert_eq!(entries["node_modules/next/dist/shared.js"].source, Source::Link("../../.pnpm/shared.js".to_owned()));
        // What they read beside them; never Next's server, nor the adapter's own file.
        for name in ["web/.next/BUILD_ID", "web/.next/routes-manifest.json", "web/.next/server/pages-manifest.json", "web/.next/server/app/index.html", "web/.next/server/route-cache/k/$/icon.png.body", "web/.next/cache/fetch-cache/abc", "web/.next/server/edge/chunk.js", "web/package.json"] {
            assert!(has(name), "{name}");
        }
        assert!(!has("web/server.js") && !has("node_modules/next/dist/server/next-server.js") && !has("web/.next/forkrail-next.json"));
        // A shell S kept is packaged; a seeded page S left out stays out.
        assert!(has("web/.next/server/app/[slug].html") && !has("web/.next/server/app/built.html"));
        // Traced extras; sharp as S has it, never this machine's.
        assert!(has("node_modules/@next/env/dist/index.js"));
        assert!(has("node_modules/@img/sharp-linux-arm64/lib/sharp.node") && !has("node_modules/@img/sharp-darwin-arm64/lib/sharp.node"));
        assert!(!has("node_modules/@img/sharp-wasm32/lib/sharp-wasm32.node.wasm"), "the fallback sharp never loads beside Linux's");
        // The server, @next/routing, the manifest and the entry.
        for name in ["_forkrail_next/main.mjs", "_forkrail_next/server.mjs", "_forkrail_next/verified.json", "_forkrail_next/node_modules/@next/routing/dist/index.js", MANIFEST, crate::next::ENTRY] {
            assert!(has(name), "{name}");
        }
        assert!(!entries.keys().any(|name| name.ends_with(".test.mjs")));
        let Source::File(launcher) = &entries[crate::next::ENTRY].source else { panic!("the entry is a file") };
        assert!(std::fs::read_to_string(launcher).unwrap().contains("import './_forkrail_next/main.mjs'"));
        // Under a workspace's project path, the entry finds the server at the package's top.
        assert!(launcher_js("apps/web/forkrail-server.mjs").contains("import '../../_forkrail_next/main.mjs'"));

        let Source::File(path) = &entries[MANIFEST].source else { panic!("the manifest is a file") };
        let m: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!((m["projectDir"].as_str(), m["distDir"].as_str(), m["buildId"].as_str(), m["deploymentId"].as_str()), (Some("web"), Some(".next"), Some("b1"), Some("dpl")));
        assert_eq!((m["compress"].as_bool(), m["poweredByHeader"].as_bool(), m["validateRsc"].as_bool()), (Some(false), Some(false), Some(true)));
        assert_eq!(m["env"]["A"], "1");
        assert_eq!(m["functions"]["/"]["type"], "APP_PAGE");
        assert_eq!(m["functions"]["/"]["ssg"], true);
        assert_eq!(m["functions"]["/"]["maxDuration"], 30);
        assert_eq!(m["functions"]["/api/edge"]["edge"]["modulePath"], "web/.next/server/app/api/edge/route.js");
        assert_eq!(m["functions"]["/api/edge"]["env"]["KEY"], "k");
        assert_eq!(m["prerenders"]["/"]["parent"], "/page");
        assert!(m["middleware"].is_null());
        // The router's files aren't in the package; the static metadata route's is, and so is
        // every file S's package kept for Next's server, `public/`'s included.
        assert!(m["statics"]["/a.txt"].is_null() && !has("web/public/a.txt"));
        assert!(m["statics"]["/_next/static/chunks/router.js"].is_null() && !has("web/.next/static/chunks/router.js"));
        assert_eq!(m["statics"]["/icon.png"], "web/.next/server/route-cache/k/$/icon.png.body");
        assert_eq!(m["statics"]["/_next/static/chunks/kept.js"], "web/.next/static/chunks/kept.js");
        assert_eq!(m["statics"]["/kept.txt"], "web/public/kept.txt");
        assert!(has("web/.next/static/chunks/kept.js") && has("web/public/kept.txt"));

        // No @next/routing of the version: no package (the build stays on Next's own server).
        let missing = |_: &str, _: &dyn BuildLog| -> Result<PathBuf> { bail!("offline") };
        assert!(package_with(&handlers, &s, &public, None, &scratch, &trace, &missing, &Quiet).unwrap().is_none());
    }
}
