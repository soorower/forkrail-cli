//! A Node backend preset's build (Express, Hono, Fastify, Koa, NestJS, Elysia, H3, or a plain
//! `server.*`), as Vercel's builders do it: the project's dependencies installed with its package
//! manager, its `vercel-build` or `build` script run if it has one, a TypeScript entry compiled,
//! and, for a framework's app, a launcher that serves an app the entry exports rather than
//! listening itself (Vercel takes both; the shim does the serving, shim.mjs).
//!
//! TypeScript is bundled with esbuild (the project's own, else a pinned one through npx), the
//! project's own files into one, its packages left to `node_modules`: CommonJS unless the
//! `package.json` says `"type": "module"`, written beside the entry so `__dirname` is the entry's
//! folder, as it was in the source.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::backends::Backend;
use crate::buildlog::{self, BuildLog};
use crate::framework::{self, Install};

/// The esbuild a project without one of its own is compiled with.
pub const ESBUILD: &str = "esbuild@0.28.2";
/// Where the launcher goes in the package's `app/`.
pub const LAUNCHER: &str = ".forkrail/launch.mjs";
/// Lambda's oldest Node, which compiled code must run on.
const TARGET: &str = "node22";

/// What the build made: the entry the runtime starts, relative to `app/`, and the files to add.
#[derive(Debug, Default)]
pub struct Prepared {
    pub entry: String,
    pub files: Vec<(String, PathBuf)>,
}

/// Install, build and compile the backend in `dir`; made files go in `work`.
pub fn build(dir: &Path, backend: &Backend, config: Option<&Value>, work: &Path, install: Install, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<Prepared> {
    let package: Option<Value> = std::fs::read_to_string(dir.join("package.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
    let script = |key: &str| config.and_then(|c| c.get(key)).and_then(Value::as_str).map(str::to_owned);
    if let Some(package) = &package {
        if install == Install::Always || !dir.join("node_modules").is_dir() {
            log.step(buildlog::INSTALLING);
            let command = script("installCommand").unwrap_or_else(|| framework::flat_install_command(dir));
            shell(dir, &command, env, log)?;
        }
        let manager = framework::package_manager_name(dir);
        let build = script("buildCommand").or_else(|| {
            let scripts = package.get("scripts")?;
            ["vercel-build", "build"].iter().find(|s| scripts.get(**s).is_some()).map(|s| format!("{manager} run {s}"))
        });
        if let Some(command) = build {
            log.step(buildlog::BUILDING);
            shell(dir, &command, env, log)?;
        }
    }
    let mut prepared = Prepared { entry: backend.entry.clone(), files: Vec::new() };
    // NestJS needs its own compiler (decorator metadata, which esbuild doesn't write): its build's
    // output, when it left one.
    if backend.preset.slug == "nestjs" && dir.join("dist/main.js").is_file() {
        log.info("NestJS: running dist/main.js, its build's output");
        prepared.entry = "dist/main.js".to_owned();
    } else if backend.typescript() {
        let module = package.as_ref().and_then(|p| p.get("type")).and_then(Value::as_str) == Some("module") || backend.entry.ends_with(".mts");
        let (stem, ext) = backend.entry.rsplit_once('.').expect("a TypeScript entry has an extension");
        let out_rel = format!("{stem}.forkrail.{}", if module { "mjs" } else { "cjs" });
        let _ = ext;
        let out = work.join(&out_rel);
        std::fs::create_dir_all(out.parent().expect("has a folder"))?;
        compile(dir, &backend.entry, &out, module, env, log)?;
        if backend.preset.slug == "nestjs" {
            log.info("warning: NestJS compiled with esbuild has no decorator metadata, so constructor injection may fail; give the project a `build` script (`nest build`) that writes dist/main.js");
        }
        prepared.files.push((out_rel.clone(), out));
        prepared.entry = out_rel;
    }
    // A framework's app may be exported rather than listening: the launcher serves it.
    if backend.preset.slug != "node" {
        let launcher = work.join(LAUNCHER);
        std::fs::create_dir_all(launcher.parent().expect("has a folder"))?;
        std::fs::write(&launcher, launcher_source(&prepared.entry))?;
        prepared.files.push((LAUNCHER.to_owned(), launcher));
        prepared.entry = LAUNCHER.to_owned();
    }
    Ok(prepared)
}

/// The launcher: imports the entry, then has the shim serve what it exports if nothing listened.
fn launcher_source(entry: &str) -> String {
    let target = serde_json::to_string(&format!("../{entry}")).expect("a string");
    format!(
        "// Written by forkrail: runs the app's entry, then serves the app it exports if it didn't listen.\n\
         const module = await import(new URL({target}, import.meta.url).href);\n\
         const serve = globalThis[Symbol.for('forkrail.serve')];\n\
         if (typeof serve === 'function') serve(module, {target});\n"
    )
}

/// The oldest esbuild of a project's own that's used: it has `--packages=external` (0.17).
const OWN_ESBUILD_SINCE: (u64, u64) = (0, 17);

/// esbuild: the project's own when it's new enough (`OWN_ESBUILD_SINCE`; an old one comes with
/// old tools such as `@vercel/node` 2), else the pinned one through npx.
pub fn esbuild(dir: &Path) -> Command {
    let own = dir.join("node_modules/.bin/esbuild");
    let version = std::fs::read_to_string(dir.join("node_modules/esbuild/package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|p| p.get("version").and_then(Value::as_str).map(str::to_owned))
        .and_then(|v| {
            let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
            Some((parts.next()??, parts.next()??))
        });
    if own.is_file() && version.is_some_and(|v| v >= OWN_ESBUILD_SINCE) {
        Command::new(own)
    } else {
        let mut c = Command::new("npx");
        c.args(["--yes", ESBUILD]);
        c
    }
}

/// Bundle a TypeScript entry with esbuild.
fn compile(dir: &Path, entry: &str, out: &Path, module: bool, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
    let mut command = esbuild(dir);
    command
        .arg(entry)
        .args(["--bundle", "--platform=node", "--packages=external", "--log-level=warning", "--sourcemap=inline"])
        .arg(format!("--target={TARGET}"))
        .arg(format!("--format={}", if module { "esm" } else { "cjs" }))
        .arg(format!("--outfile={}", out.display()))
        .current_dir(dir);
    log.info(&format!("compiling {entry} with esbuild ({})", if module { "ES module" } else { "CommonJS" }));
    buildlog::run(&mut command, "esbuild", env, log).context("compiling TypeScript")
}

fn shell(dir: &Path, command: &str, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
    log.info(command);
    buildlog::run(Command::new("sh").arg("-c").arg(command).current_dir(dir), &format!("`{command}`"), env, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_imports_the_entry_beside_it() {
        let source = launcher_source("src/index.forkrail.cjs");
        assert!(source.contains("import(new URL(\"../src/index.forkrail.cjs\", import.meta.url).href)"), "{source}");
        assert!(source.contains("Symbol.for('forkrail.serve')"));
    }
}
