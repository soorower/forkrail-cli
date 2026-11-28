//! Go and Rust web servers: a program the project builds that listens on `PORT`, run on Lambda's
//! `provided.al2023` with forkrail's runtime as its `bootstrap` (crates/runtime, `app.rs`: a
//! program without a shim).
//!
//! - **Go:** a `go.mod` at the project's top and a `main` package there (or the one under `cmd/`),
//!   built with `go build` for Linux arm64, statically (`CGO_ENABLED=0`), as Vercel builds Go.
//! - **Rust:** a `Cargo.toml` with one binary (or `default-run`), built with `cargo zigbuild` for
//!   Linux arm64 with Amazon Linux 2023's glibc, here and in the builder alike: zig is the linker
//!   and the C compiler for crates with C in them, which Lambda's images (the builder's) haven't got.
//!
//! Vercel's own Go and Rust support is a function per file in `api/` (`api/*.go`, `api/*.rs`):
//! that's forkrail's C18, refused here by name until then.
//!
//! The program goes into the package at `app/.forkrail/<name>`, beside the project's own files
//! (templates and the like that it reads from its working directory), which leave out the build's
//! output (`target/`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::buildlog::{self, BuildLog};
use crate::vercel::Language;

/// Where in the app's folder the program goes. The runtime runs `FORKRAIL_APP_ENTRY`, relative to
/// it, as a program when it has no extension the shims run.
pub const BIN_DIR: &str = ".forkrail";
/// The Lambda runtime Go and Rust run on: Amazon Linux 2023, whose `bootstrap` is forkrail's runtime.
pub const RUNTIME: &str = "provided.al2023";
/// Amazon Linux 2023's glibc, which a Rust program built away from it must not need more of.
const GLIBC: &str = "2.34";
const RUST_TARGET: &str = "aarch64-unknown-linux-gnu";

/// A Go or Rust server in a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub language: Language,
    /// Go: the package to build (`.`, `./cmd/api`). Rust: the binary's name.
    pub target: String,
    /// The program's name in the package, under `BIN_DIR`.
    pub name: String,
}

impl Server {
    /// The entry the runtime starts, relative to the package's `app/`.
    pub fn entry(&self) -> String {
        format!("{BIN_DIR}/{}", self.name)
    }

    /// What it is, for the user: `Go (.)`, `Rust (api)`.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.language.name(), self.target)
    }
}

/// The Go or Rust server in `dir`, if it has one: Go first when it has both a `go.mod` and a
/// `Cargo.toml` at its top (as Vercel looks at `go.mod` first). `Ok(None)` when it has neither.
pub fn detect(dir: &Path) -> Result<Option<Server>> {
    if dir.join("go.mod").is_file() {
        return go(dir).map(Some);
    }
    if dir.join("Cargo.toml").is_file() {
        return rust(dir).map(Some);
    }
    Ok(None)
}

/// A Go `main` package: at the project's top, else the only one under `cmd/`.
fn go(dir: &Path) -> Result<Server> {
    let module = std::fs::read_to_string(dir.join("go.mod")).context("reading go.mod")?;
    let module_name = module.lines().find_map(|l| l.trim().strip_prefix("module ")).map(|m| m.trim().trim_matches('"').to_owned());
    let name = |pkg: &str| -> String {
        let last = if pkg == "." { module_name.as_deref().and_then(|m| m.rsplit('/').next()).unwrap_or("server") } else { pkg.rsplit('/').next().unwrap_or("server") };
        clean_name(last)
    };
    if is_go_main(dir) {
        return Ok(Server { language: Language::Go, target: ".".to_owned(), name: name(".") });
    }
    let mut mains: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("cmd")) {
        for entry in entries.flatten() {
            if entry.path().is_dir() && is_go_main(&entry.path()) {
                mains.push(format!("./cmd/{}", entry.file_name().to_string_lossy()));
            }
        }
    }
    mains.sort();
    // Vercel's Go preset looks at cmd/api, then cmd/server.
    if let Some(known) = ["./cmd/api", "./cmd/server"].iter().find(|k| mains.iter().any(|m| m == *k)) {
        return Ok(Server { language: Language::Go, target: (*known).to_owned(), name: name(known) });
    }
    match mains.as_slice() {
        [one] => Ok(Server { language: Language::Go, target: one.clone(), name: name(one) }),
        [] if has_go_files(&dir.join("api")) => bail!(
            "Go functions in api/ (Vercel's `api/*.go`, one function a file) aren't supported yet; a Go web server is: a `main` package at the project's top that listens on PORT"
        ),
        [] => bail!("go.mod, but no `main` package at the project's top or under cmd/: forkrail runs a Go web server that listens on PORT"),
        many => bail!("{} are all `main` packages: keep one under cmd/, or put the server's at the project's top", many.join(", ")),
    }
}

/// Whether the folder's Go files (not tests) are `package main` with a `func main`.
fn is_go_main(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let mut main_package = false;
    let mut main_func = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".go") || name.ends_with("_test.go") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        if go_package(&text) == Some("main") {
            main_package = true;
            if text.lines().any(|l| l.trim_start().starts_with("func main()")) {
                main_func = true;
            }
        }
    }
    main_package && main_func
}

fn has_go_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".go")))
}

/// A Go file's package name: the first `package` clause outside comments.
pub fn go_package(text: &str) -> Option<&str> {
    let mut in_block = false;
    for line in text.lines() {
        let mut line = line.trim();
        if in_block {
            match line.find("*/") {
                Some(end) => {
                    in_block = false;
                    line = line[end + 2..].trim();
                }
                None => continue,
            }
        }
        if line.starts_with("//") || line.is_empty() {
            continue;
        }
        if line.starts_with("/*") {
            if !line.contains("*/") {
                in_block = true;
            }
            continue;
        }
        return line.strip_prefix("package ").map(|p| p.split_whitespace().next().unwrap_or(""));
    }
    None
}

/// A Rust binary: the package's only one, or `default-run`.
fn rust(dir: &Path) -> Result<Server> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).context("reading Cargo.toml")?;
    let manifest: toml::Table = toml::from_str(&text).context("Cargo.toml isn't valid TOML")?;
    let Some(package) = manifest.get("package").and_then(|p| p.as_table()) else {
        if manifest.contains_key("workspace") {
            bail!("Cargo.toml is a workspace: set the project's root directory to the package of the server");
        }
        bail!("Cargo.toml has no [package]");
    };
    let package_name = package.get("name").and_then(|n| n.as_str()).context("Cargo.toml's [package] has no name")?;
    let mut bins: Vec<String> = Vec::new();
    if let Some(list) = manifest.get("bin").and_then(|b| b.as_array()) {
        for bin in list {
            if let Some(name) = bin.get("name").and_then(|n| n.as_str()) {
                bins.push(name.to_owned());
            }
        }
    }
    let autobins = package.get("autobins").and_then(|a| a.as_bool()).unwrap_or(true);
    if autobins {
        if dir.join("src/main.rs").is_file() && !bins.iter().any(|b| b == package_name) {
            bins.push(package_name.to_owned());
        }
        if let Ok(entries) = std::fs::read_dir(dir.join("src/bin")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let bin = if let Some(stem) = name.strip_suffix(".rs") {
                    Some(stem.to_owned())
                } else if entry.path().join("main.rs").is_file() {
                    Some(name)
                } else {
                    None
                };
                if let Some(bin) = bin.filter(|b| !bins.contains(b)) {
                    bins.push(bin);
                }
            }
        }
    }
    bins.sort();
    let chosen = match package.get("default-run").and_then(|d| d.as_str()) {
        Some(run) if bins.iter().any(|b| b == run) => run.to_owned(),
        Some(run) => bail!("Cargo.toml's default-run is {run:?}, which isn't one of its binaries ({})", bins.join(", ")),
        None => match bins.as_slice() {
            [one] => one.clone(),
            [] if has_rust_files(&dir.join("api")) => bail!(
                "Rust functions in api/ (Vercel's `api/*.rs`, one function a file) aren't supported yet; a Rust web server is: a binary (src/main.rs) that listens on PORT"
            ),
            [] => bail!("Cargo.toml has no binary (src/main.rs or [[bin]]): forkrail runs a Rust web server that listens on PORT"),
            many => bail!("Cargo.toml has binaries {}: say which serves with `default-run` in [package]", many.join(", ")),
        },
    };
    Ok(Server { language: Language::Rust, name: clean_name(&chosen), target: chosen })
}

fn has_rust_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| entries.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".rs")))
}

/// A program's name as a file name: letters, digits, `-`, `_` and `.` (not first).
fn clean_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    let cleaned = cleaned.trim_start_matches('.').to_owned();
    if cleaned.is_empty() { "server".to_owned() } else { cleaned }
}

/// Build the server in `dir`; returns the program, built for Linux arm64. Its tools' caches are
/// under `cache` (`go-build`, `go-mod`, `cargo-target`), which the builder keeps between builds.
pub fn build(dir: &Path, server: &Server, cache: &Path, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<PathBuf> {
    log.step(buildlog::BUILDING);
    match server.language {
        Language::Go => build_go(dir, server, cache, env, log),
        Language::Rust => build_rust(dir, server, cache, env, log),
        other => bail!("{} isn't built as a program", other.name()),
    }
}

fn build_go(dir: &Path, server: &Server, cache: &Path, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<PathBuf> {
    let out = cache.join("go-out");
    std::fs::create_dir_all(&out)?;
    let program = out.join(&server.name);
    let _ = std::fs::remove_file(&program);
    let mut env = env.clone();
    for (k, v) in [("GOOS", "linux"), ("GOARCH", "arm64"), ("CGO_ENABLED", "0")] {
        env.entry(k.to_owned()).or_insert_with(|| v.to_owned());
    }
    env.entry("GOCACHE".to_owned()).or_insert_with(|| cache.join("go-build").display().to_string());
    env.entry("GOMODCACHE".to_owned()).or_insert_with(|| cache.join("go-mod").display().to_string());
    // The go.mod's `toolchain` (or a `go` line newer than the installed Go) fetches that Go.
    env.entry("GOTOOLCHAIN".to_owned()).or_insert_with(|| "auto".to_owned());
    // Vercel's default flags, or the project's GO_BUILD_FLAGS; vendored modules when it has them.
    let mut flags = match env.get("GO_BUILD_FLAGS").filter(|f| !f.trim().is_empty()) {
        Some(flags) => split_words(flags),
        None => vec!["-ldflags=-s -w".to_owned()],
    };
    if dir.join("vendor/modules.txt").is_file() && !flags.iter().any(|f| f.starts_with("-mod")) {
        flags.push("-mod=vendor".to_owned());
    }
    let mut command = Command::new("go");
    command.args(["build", "-trimpath"]).args(&flags).arg("-o").arg(&program).arg(&server.target).current_dir(dir);
    log.info(&format!("go build -trimpath {} -o {} {}  (GOOS=linux GOARCH=arm64 CGO_ENABLED={})", flags.join(" "), server.entry(), server.target, env["CGO_ENABLED"]));
    buildlog::run(&mut command, "go build", &env, log).map_err(|e| missing_tool(e, "go", "Go"))?;
    check_program(&program)?;
    Ok(program)
}

fn build_rust(dir: &Path, server: &Server, cache: &Path, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<PathBuf> {
    let mut programs = build_rust_bins(dir, std::slice::from_ref(&server.target), cache, env, log)?;
    Ok(programs.remove(0))
}

/// Build the crate's binaries `bins` for Linux arm64, natively on the builder (Amazon Linux 2023
/// on arm64, Lambda's own) or with cargo-zigbuild elsewhere; returns their programs, in order.
/// Cargo's registry and its target folder are under `cache`, which the builder keeps.
pub fn build_rust_bins(dir: &Path, bins: &[String], cache: &Path, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<Vec<PathBuf>> {
    let mut env = env.clone();
    env.entry("CARGO_TARGET_DIR".to_owned()).or_insert_with(|| cache.join("cargo-target").display().to_string());
    // In the builder Cargo's registry goes with the build cache; elsewhere it's the machine's own.
    if std::env::var_os("AWS_LAMBDA_FUNCTION_NAME").is_some() {
        env.entry("CARGO_HOME".to_owned()).or_insert_with(|| cache.join("cargo-home").display().to_string());
    }
    // The builder's image has no C compiler: zig's `cc` for build scripts, for Cargo alone.
    if let Some(cc) = std::env::var_os("FORKRAIL_RUST_CC").filter(|d| Path::new(d).is_dir()) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(std::iter::once(PathBuf::from(cc)).chain(std::env::split_paths(&path)))?;
        env.insert("PATH".to_owned(), joined.to_string_lossy().into_owned());
    }
    // Plain output, without colours meant for a terminal.
    env.entry("CARGO_TERM_COLOR".to_owned()).or_insert_with(|| "never".to_owned());
    let target_dir = PathBuf::from(&env["CARGO_TARGET_DIR"]);
    let mut args: Vec<String> = ["zigbuild", "--release", "--target"].map(str::to_owned).to_vec();
    args.push(format!("{RUST_TARGET}.{GLIBC}"));
    let out = target_dir.join(RUST_TARGET).join("release");
    // A lockfile is kept to; without one cargo picks today's versions.
    if dir.join("Cargo.lock").is_file() {
        args.push("--locked".to_owned());
    }
    for bin in bins {
        args.extend(["--bin".to_owned(), bin.clone()]);
        let _ = std::fs::remove_file(out.join(bin));
    }
    log.info(&format!("cargo {}", args.join(" ")));
    buildlog::run(Command::new("cargo").args(&args).current_dir(dir), "cargo build", &env, log).map_err(|e| missing_tool(e, "cargo", "Rust"))?;
    let programs: Vec<PathBuf> = bins.iter().map(|b| out.join(b)).collect();
    for program in &programs {
        check_program(program)?;
    }
    Ok(programs)
}

/// Words as a shell splits them, for GO_BUILD_FLAGS: by spaces, with '…' and "…" kept whole.
pub fn split_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                started = false;
            }
            (None, c) => word.push(c),
        }
    }
    if started || !word.is_empty() {
        words.push(word);
    }
    words
}

/// A build that couldn't start its tool says which to install.
pub fn missing_tool(error: anyhow::Error, tool: &str, language: &str) -> anyhow::Error {
    let text = format!("{error:#}");
    if text.contains("starting") && (text.contains("No such file") || text.contains("not found")) {
        anyhow::anyhow!("{language} isn't installed here (`{tool}` couldn't be started): install it, or build in the builder (deploy without --local)")
    } else {
        error
    }
}

/// The program must be a Linux arm64 executable, or every worker would fail to start it.
pub fn check_program(path: &Path) -> Result<()> {
    let mut head = [0u8; 20];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
        .with_context(|| format!("the build left no program at {}", path.display()))?;
    if &head[..4] != b"\x7fELF" || head[4] != 2 || u16::from_le_bytes([head[18], head[19]]) != 183 {
        bail!("{} isn't a Linux arm64 program", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-native-{}", forkrail_common::random_hex(6)));
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    const MAIN: &str = "// Package main serves.\n/* a\n block */\npackage main\n\nimport \"net/http\"\n\nfunc main() {\n\thttp.ListenAndServe(\":8080\", nil)\n}\n";

    #[test]
    fn go_servers() {
        let root = project(&[("go.mod", "module github.com/ada/shop-api\n\ngo 1.25\n"), ("main.go", MAIN), ("main_test.go", "package main\n")]);
        let server = detect(&root).unwrap().unwrap();
        assert_eq!((server.language, server.target.as_str(), server.entry()), (Language::Go, ".", ".forkrail/shop-api".to_owned()));
        let cmd = project(&[("go.mod", "module shop\n"), ("cmd/api/main.go", MAIN), ("cmd/tool/x.go", "package tool\n"), ("internal/db/db.go", "package db\n")]);
        assert_eq!(detect(&cmd).unwrap().unwrap().target, "./cmd/api");
        let known = project(&[("go.mod", "module shop\n"), ("cmd/server/main.go", MAIN), ("cmd/worker/main.go", MAIN)]);
        assert_eq!(detect(&known).unwrap().unwrap().target, "./cmd/server");
        let two = project(&[("go.mod", "module shop\n"), ("cmd/web/main.go", MAIN), ("cmd/worker/main.go", MAIN)]);
        assert!(format!("{:#}", detect(&two).unwrap_err()).contains("./cmd/web, ./cmd/worker"));
        let functions = project(&[("go.mod", "module shop\n"), ("api/index.go", "package handler\n")]);
        assert!(format!("{:#}", detect(&functions).unwrap_err()).contains("Go functions in api/"));
        let library = project(&[("go.mod", "module shop\n"), ("shop.go", "package shop\n\nfunc main() {}\n")]);
        assert!(detect(&library).is_err());
        assert_eq!(detect(&project(&[("package.json", "{}")])).unwrap(), None);
    }

    #[test]
    fn rust_servers() {
        let one = project(&[("Cargo.toml", "[package]\nname = \"rust-axum\"\nversion = \"0.1.0\"\n"), ("src/main.rs", "fn main() {}")]);
        let server = detect(&one).unwrap().unwrap();
        assert_eq!((server.language, server.target.as_str(), server.entry()), (Language::Rust, "rust-axum", ".forkrail/rust-axum".to_owned()));
        let many = project(&[("Cargo.toml", "[package]\nname = \"shop\"\n"), ("src/main.rs", ""), ("src/bin/migrate.rs", "")]);
        assert!(format!("{:#}", detect(&many).unwrap_err()).contains("default-run"));
        let chosen = project(&[("Cargo.toml", "[package]\nname = \"shop\"\ndefault-run = \"server\"\n\n[[bin]]\nname = \"server\"\npath = \"src/server.rs\"\n"), ("src/bin/migrate.rs", "")]);
        assert_eq!(detect(&chosen).unwrap().unwrap().target, "server");
        let workspace = project(&[("Cargo.toml", "[workspace]\nmembers = [\"api\"]\n")]);
        assert!(format!("{:#}", detect(&workspace).unwrap_err()).contains("root directory"));
        let functions = project(&[("Cargo.toml", "[package]\nname = \"x\"\n"), ("api/hello.rs", "")]);
        assert!(format!("{:#}", detect(&functions).unwrap_err()).contains("Rust functions in api/"));
    }

    #[test]
    fn package_clauses() {
        assert_eq!(go_package(MAIN), Some("main"));
        assert_eq!(go_package("//go:build linux\n\npackage db // the store\n"), Some("db"));
        assert_eq!(go_package("/* only a comment */"), None);
        assert_eq!(clean_name("..x y"), "x-y");
        assert_eq!(split_words("-ldflags '-s -w -X main.v=1' -tags \"a b\""), ["-ldflags", "-s -w -X main.v=1", "-tags", "a b"]);
    }
}
