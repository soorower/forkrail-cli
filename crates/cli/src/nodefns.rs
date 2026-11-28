//! Node functions in `api/` (Vercel's zero-config `api/*.js`, `api/*.ts`, ...): every file a
//! function, all served by one Node app in the deployment's one Lambda function.
//!
//! - The project's dependencies are installed (flat, as for Node backends) when the site's build
//!   hasn't already, and TypeScript is compiled per file with esbuild (the project's own, else the
//!   pinned one nodeapp.rs uses): the project's own files bundled in, its packages left to
//!   `node_modules`, written beside the source as `<name>.forkrail.mjs` (or `.cjs`), so a function
//!   finds the files beside it as it did in the source. JavaScript runs as it is.
//! - `.forkrail/functions.mjs` lists the functions, each with its file, its URL paths and a loader,
//!   and hands them to the runtime's shim (crates/runtime/src/shim.mjs), which dispatches to them
//!   by the router's `x-forkrail-function` header.
//! - What the functions import from npm is found from esbuild's metafile, and the package keeps
//!   those packages and what they depend on, not the whole `node_modules` (a site's build tools
//!   stay out): Vercel traces each function's files the same way.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::buildlog::{self, BuildLog};
use crate::framework::{self, Install};
use crate::functions;

/// The app's entry, relative to the package's `app/`: the runtime passes the router's function
/// header to it (crates/runtime/src/app.rs, `DISPATCHERS`).
pub const ENTRY: &str = ".forkrail/functions.mjs";
/// The extensions of Node functions, as Vercel's zero-config `api/` takes them (`.cjs`, `.mts` and
/// `.cts` files are modules the functions may import, not functions).
pub const EXTENSIONS: [&str; 4] = ["js", "mjs", "ts", "tsx"];
/// Lambda's oldest Node, which compiled code must run on.
const TARGET: &str = "node22";

/// What the build made: files for the package's `app/` (compiled functions and the entry), and the
/// npm packages the functions import (by name).
#[derive(Debug, Default)]
pub struct Prepared {
    pub files: Vec<(String, PathBuf)>,
    pub packages: BTreeSet<String>,
}

fn typescript(file: &str) -> bool {
    ["ts", "mts", "cts", "tsx"].iter().any(|ext| file.ends_with(&format!(".{ext}")))
}

/// Whether a TypeScript file compiles to an ES module: `.mts`, or the project's `"type": "module"`
/// (`.cts` never).
fn compiles_to_esm(file: &str, module_package: bool) -> bool {
    file.ends_with(".mts") || (module_package && !file.ends_with(".cts"))
}

/// The compiled file of a TypeScript function, beside it: `api/users/[id].forkrail.cjs`.
pub fn compiled_name(file: &str, esm: bool) -> String {
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    format!("{stem}.forkrail.{}", if esm { "mjs" } else { "cjs" })
}

/// Install the project's dependencies if they aren't (or `install` says always, and the site's
/// build didn't just do it: `installed`), compile its TypeScript functions, and write the entry.
/// Made files go in `work`.
#[allow(clippy::too_many_arguments)]
pub fn build(dir: &Path, files: &[String], config: Option<&Value>, work: &Path, install: Install, installed: bool, middleware: Option<&crate::middleware::Found>, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<Prepared> {
    let package: Option<Value> = std::fs::read_to_string(dir.join("package.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
    if package.is_some() && ((install == Install::Always && !installed) || !dir.join("node_modules").is_dir()) {
        log.step(buildlog::INSTALLING);
        let command = config.and_then(|c| c.get("installCommand")).and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| framework::flat_install_command(dir));
        log.info(&command);
        buildlog::run(Command::new("sh").arg("-c").arg(&command).current_dir(dir), &format!("`{command}`"), env, log)?;
    }
    log.step(buildlog::BUILDING);
    let module_package = package.as_ref().and_then(|p| p.get("type")).and_then(Value::as_str) == Some("module");
    let mut prepared = Prepared::default();
    let out = work.join("node-functions");
    // TypeScript in two batches, by the module format it compiles to; JavaScript traced alone.
    let (esm, cjs): (Vec<&String>, Vec<&String>) = files.iter().filter(|f| typescript(f)).partition(|f| compiles_to_esm(f, module_package));
    let js: Vec<&String> = files.iter().filter(|f| !typescript(f)).collect();
    for (batch, esm) in [(&esm, true), (&cjs, false)] {
        if batch.is_empty() {
            continue;
        }
        let meta = work.join(if esm { "esm-meta.json" } else { "cjs-meta.json" });
        let extension = if esm { ".forkrail.mjs" } else { ".forkrail.cjs" };
        let mut command = esbuild(dir);
        command
            .args(batch.iter().map(|f| f.as_str()))
            .args(["--bundle", "--platform=node", "--packages=external", "--log-level=warning", "--sourcemap=inline", "--outbase=."])
            .arg(format!("--target={TARGET}"))
            .arg(format!("--format={}", if esm { "esm" } else { "cjs" }))
            .arg(format!("--out-extension:.js={extension}"))
            .arg(format!("--outdir={}", out.display()))
            .arg(format!("--metafile={}", meta.display()))
            .current_dir(dir);
        log.info(&format!("compiling {} TypeScript function{} with esbuild ({})", batch.len(), if batch.len() == 1 { "" } else { "s" }, if esm { "ES modules" } else { "CommonJS" }));
        buildlog::run(&mut command, "esbuild", env, log).context("compiling TypeScript")?;
        prepared.packages.extend(external_packages(&meta)?);
        for file in batch.iter() {
            let name = compiled_name(file, esm);
            let made = out.join(&name);
            if !made.is_file() {
                bail!("esbuild left no {name} for {file}");
            }
            prepared.files.push((name, made));
        }
    }
    if !js.is_empty() {
        // Not compiled: bundled only to read what they import.
        let meta = work.join("js-meta.json");
        let mut command = esbuild(dir);
        command
            .args(js.iter().map(|f| f.as_str()))
            .args(["--bundle", "--platform=node", "--packages=external", "--log-level=error", "--outbase=.", "--format=esm"])
            .arg(format!("--target={TARGET}"))
            .arg(format!("--outdir={}", work.join("node-trace").display()))
            .arg(format!("--metafile={}", meta.display()))
            .current_dir(dir);
        buildlog::run(&mut command, "esbuild", env, log).context("reading what the JavaScript functions import")?;
        prepared.packages.extend(external_packages(&meta)?);
    }
    let entry = work.join("functions.mjs");
    let mut source = entry_source(files, module_package);
    if let Some(found) = middleware {
        // The project's Routing Middleware, which only the router names (middleware.rs).
        source = source.replacen("]);\n", &format!("], {{ middleware: [{}] }});\n", found.dispatcher_entry()), 1);
    }
    std::fs::write(&entry, source)?;
    prepared.files.push((ENTRY.to_owned(), entry));
    if !prepared.packages.is_empty() {
        log.info(&format!("npm packages the functions import: {}", prepared.packages.iter().cloned().collect::<Vec<_>>().join(", ")));
    }
    Ok(prepared)
}

fn esbuild(dir: &Path) -> Command {
    crate::nodeapp::esbuild(dir)
}

/// The entry: every function with its file, paths and loader, handed to the shim.
pub fn entry_source(files: &[String], module_package: bool) -> String {
    let mut out = String::from(
        "// Written by forkrail: the project's functions in api/, served by forkrail's runtime (shim.mjs).\n\
         const serve = globalThis[Symbol.for('forkrail.functions')];\n\
         if (typeof serve !== 'function') throw new Error('forkrail: this app runs under forkrail\\'s runtime');\n\
         serve([\n",
    );
    for file in files {
        let loaded = if typescript(file) { compiled_name(file, compiles_to_esm(file, module_package)) } else { file.clone() };
        let paths = forkrail_common::rules::file_paths(file);
        let target = serde_json::to_string(&format!("../{loaded}")).expect("a string");
        out.push_str(&format!(
            "  {{ file: {}, paths: {}, load: () => import(new URL({target}, import.meta.url).href) }},\n",
            serde_json::to_string(file).expect("a string"),
            serde_json::to_string(&paths).expect("strings"),
        ));
    }
    out.push_str("]);\n");
    out
}

/// The npm packages an esbuild metafile's inputs import (bare names, not Node's own modules).
fn external_packages(meta: &Path) -> Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(meta).with_context(|| format!("reading {}", meta.display()))?;
    let meta: Value = serde_json::from_str(&text).context("esbuild's metafile isn't JSON")?;
    let mut out = BTreeSet::new();
    for input in meta.get("inputs").and_then(Value::as_object).into_iter().flat_map(|m| m.values()) {
        for import in input.get("imports").and_then(Value::as_array).into_iter().flatten() {
            if import.get("external").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            if let Some(name) = import.get("path").and_then(Value::as_str).and_then(package_name) {
                out.insert(name);
            }
        }
    }
    Ok(out)
}

/// The npm package a bare import names (`@scope/name/sub` → `@scope/name`), or None for Node's
/// own modules, relative paths and URLs.
pub fn package_name(import: &str) -> Option<String> {
    if import.starts_with(['.', '/']) || import.starts_with("node:") || import.contains("://") || import.starts_with("data:") {
        return None;
    }
    let mut parts = import.split('/');
    let first = parts.next()?;
    let name = if first.starts_with('@') { format!("{first}/{}", parts.next()?) } else { first.to_owned() };
    if !first.starts_with('@') && NODE_BUILTINS.contains(&first) {
        return None;
    }
    Some(name)
}

/// Node's own modules, imported without `node:`.
const NODE_BUILTINS: [&str; 56] = [
    "assert", "async_hooks", "buffer", "child_process", "cluster", "console", "constants", "crypto", "dgram", "diagnostics_channel", "dns", "domain", "events", "fs", "http", "http2", "https",
    "inspector", "module", "net", "os", "path", "perf_hooks", "process", "punycode", "querystring", "readline", "repl", "stream", "string_decoder", "sys", "timers", "tls", "trace_events", "tty",
    "url", "util", "v8", "vm", "wasi", "worker_threads", "zlib", "test", "sqlite", "sea", "stream/web", "fs/promises", "dns/promises", "path/posix", "path/win32", "readline/promises",
    "stream/promises", "stream/consumers", "timers/promises", "util/types", "assert/strict",
];

/// The folders under `node_modules` (relative to `dir`, as `node_modules/a`,
/// `node_modules/a/node_modules/b`) the packages `names` need: each one as Node would find it from
/// the project, and what it depends on (`dependencies`, `optionalDependencies`, and
/// `peerDependencies` not marked optional) as Node would find them from it, all the way down. A
/// name not installed is left out (an optional dependency for another platform, or a peer nothing
/// installed). An optional peer (Next.js for a library that also works without it) comes in only
/// when the functions import it themselves.
pub fn package_dirs(dir: &Path, names: &BTreeSet<String>) -> BTreeSet<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<(String, String)> = names.iter().map(|n| (String::new(), n.clone())).collect();
    while let Some((from, name)) = queue.pop() {
        let Some(at) = resolve(dir, &from, &name) else { continue };
        if !found.insert(at.clone()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(dir.join(&at).join("package.json")) else { continue };
        let Ok(package) = serde_json::from_str::<Value>(&text) else { continue };
        for key in ["dependencies", "optionalDependencies", "peerDependencies"] {
            for dep in package.get(key).and_then(Value::as_object).into_iter().flat_map(|d| d.keys()) {
                let optional = key == "peerDependencies" && package.pointer(&format!("/peerDependenciesMeta/{}/optional", dep.replace('~', "~0").replace('/', "~1"))).and_then(Value::as_bool) == Some(true);
                if !optional {
                    queue.push((at.clone(), dep.clone()));
                }
            }
        }
    }
    found
}

/// Where Node finds package `name` from the folder `from` (relative to `dir`; empty for the
/// project): `from/node_modules/name`, then each folder up.
fn resolve(dir: &Path, from: &str, name: &str) -> Option<String> {
    let mut at = from.to_owned();
    loop {
        let candidate = if at.is_empty() { format!("node_modules/{name}") } else { format!("{at}/node_modules/{name}") };
        if dir.join(&candidate).join("package.json").is_file() {
            return Some(candidate);
        }
        if at.is_empty() {
            return None;
        }
        // Up past this package's folder (and the `node_modules` it's in).
        at = match at.rfind("/node_modules/") {
            Some(i) => at[..i].to_owned(),
            None => String::new(),
        };
    }
}

/// Whether a project file goes in the package: anything outside `node_modules`, and inside it only
/// what's in one of `dirs` (from `package_dirs`).
pub fn keeps(rel: &str, dirs: &BTreeSet<String>) -> bool {
    if !rel.starts_with("node_modules/") {
        return true;
    }
    // The longest folder at or above the file is enough: `dirs` holds whole packages.
    let mut at = rel;
    while let Some(i) = at.rfind('/') {
        at = &at[..i];
        if dirs.contains(at) {
            return true;
        }
    }
    false
}

/// The function files of a project's Node functions, from `functions::discover_any`.
pub fn discover(dir: &Path) -> Vec<String> {
    functions::discover_any(dir, &EXTENSIONS)
}

/// The `maxDuration` a function file's own `export const config = { ... }` sets, read from its text
/// as Vercel reads it at build (a literal number, or `'max'`); `vercel.json`'s `functions` wins.
pub fn file_max_duration(text: &str) -> Option<ConfigDuration> {
    let at = text.find("export const config")?;
    let rest = &text[at..];
    let open = rest.find('{')?;
    // The object literal, to its matching brace.
    let mut depth = 0;
    let mut end = None;
    for (i, c) in rest[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let object = &rest[open..end?];
    let key = object.find("maxDuration")?;
    let value = object[key + "maxDuration".len()..].trim_start().strip_prefix(':')?.trim_start();
    if value.starts_with("'max'") || value.starts_with("\"max\"") {
        return Some(ConfigDuration::Max);
    }
    let digits: String = value.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok().map(ConfigDuration::Seconds)
}

/// A maxDuration: seconds, or the most allowed (`"max"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDuration {
    Seconds(u64),
    Max,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-nodefns-{}", forkrail_common::random_hex(6)));
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    #[test]
    fn package_names() {
        assert_eq!(package_name("ms"), Some("ms".into()));
        assert_eq!(package_name("@vercel/kv/dist/x.js"), Some("@vercel/kv".into()));
        assert_eq!(package_name("lodash/get"), Some("lodash".into()));
        assert_eq!(package_name("node:fs"), None);
        assert_eq!(package_name("fs/promises"), None);
        assert_eq!(package_name("crypto"), None);
        assert_eq!(package_name("./lib"), None);
        assert_eq!(package_name("https://x.dev/a.js"), None);
    }

    #[test]
    fn packages_and_what_they_depend_on() {
        let dir = project(&[
            ("node_modules/a/package.json", r#"{"dependencies": {"b": "1", "c": "1"}, "optionalDependencies": {"gone": "1"}, "peerDependencies": {"@s/x": "1", "vite": "7"}, "peerDependenciesMeta": {"vite": {"optional": true}}}"#),
            ("node_modules/a/index.js", ""),
            ("node_modules/a/node_modules/b/package.json", r#"{"dependencies": {"d": "1"}}"#),
            ("node_modules/b/package.json", "{}"),
            ("node_modules/c/package.json", "{}"),
            ("node_modules/d/package.json", "{}"),
            ("node_modules/vite/package.json", r#"{"dependencies": {"rollup": "4"}}"#),
            ("node_modules/rollup/package.json", "{}"),
            ("node_modules/@s/x/package.json", "{}"),
        ]);
        let dirs = package_dirs(&dir, &["a".to_owned(), "missing".to_owned()].into());
        assert_eq!(dirs.iter().map(String::as_str).collect::<Vec<_>>(), ["node_modules/@s/x", "node_modules/a", "node_modules/a/node_modules/b", "node_modules/c", "node_modules/d"]);
        assert!(keeps("api/hello.ts", &dirs));
        assert!(keeps("node_modules/a/index.js", &dirs));
        assert!(keeps("node_modules/a/node_modules/b/x/y.js", &dirs));
        assert!(keeps("node_modules/@s/x/package.json", &dirs));
        assert!(!keeps("node_modules/vite/package.json", &dirs));
        assert!(!keeps("node_modules/b/package.json", &dirs), "only the copy a uses");
        assert!(!keeps("node_modules/.package-lock.json", &dirs));
    }

    #[test]
    fn the_entry_lists_every_function() {
        let files = vec!["api/hello.ts".to_owned(), "api/users/[id].js".to_owned(), "api/index.mts".to_owned(), "api/old.cts".to_owned()];
        let source = entry_source(&files, false);
        assert!(source.contains(r#"{ file: "api/hello.ts", paths: ["/api/hello.ts","/api/hello"], load: () => import(new URL("../api/hello.forkrail.cjs", import.meta.url).href) }"#), "{source}");
        assert!(source.contains(r#"import(new URL("../api/users/[id].js""#), "{source}");
        assert!(source.contains(r#"paths: ["/api/index.mts","/api/index","/api","/api/"]"#) && source.contains("../api/index.forkrail.mjs"), "{source}");
        assert!(entry_source(&files, true).contains("../api/hello.forkrail.mjs") && entry_source(&files, true).contains("../api/old.forkrail.cjs"));
    }

    #[test]
    fn discovery_skips_declarations_and_private_files() {
        let dir = project(&[("api/hello.ts", ""), ("api/types.d.ts", ""), ("api/_lib/db.ts", ""), ("api/users/[id].tsx", ""), ("api/x.json", ""), ("api/main.py", ""), ("api/old.cjs", ""), ("api/.hidden/x.js", "")]);
        assert_eq!(discover(&dir), ["api/hello.ts", "api/users/[id].tsx"]);
    }

    #[test]
    fn a_files_own_max_duration() {
        assert_eq!(file_max_duration("export const config = {\n  maxDuration: 30,\n};\nexport async function POST() {}"), Some(ConfigDuration::Seconds(30)));
        assert_eq!(file_max_duration("export const config = { runtime: 'nodejs', maxDuration: 'max' }"), Some(ConfigDuration::Max));
        assert_eq!(file_max_duration("export const config = { api: { bodyParser: false } }\nconst maxDuration = 5"), None);
        assert_eq!(file_max_duration("export default function handler() {}"), None);
    }
}
