//! Per-file functions, Vercel's zero-config layout: every file in `api/` with a runtime's extension
//! is a function, served at its path without the extension (`api/users.go` at `/api/users`, an
//! `index` file at its folder, `api/users/[id].go` at `/api/users/<id>` with `id` in the query).
//! Files and folders starting with `_` or `.` aren't functions, as on Vercel.
//!
//! All of a deployment's functions run in its one Lambda function: the router's routes find the
//! function (rules::file_routes) and say which with `x-forkrail-function`; in the app,
//!
//! - **Go** (`api/*.go`, each with an exported `func(http.ResponseWriter, *http.Request)`): one
//!   program, built from a copy of the project with a generated `main` that dispatches to every
//!   handler (`go_dispatcher`). Each file's handler is renamed after its file, as Vercel's builder
//!   renames it, so files of one package may all call theirs `Handler`.
//! - **Rust** (`api/*.rs`, each a `[[bin]]` in `Cargo.toml`, written with Vercel's
//!   `vercel_runtime` crate): one program each, which serves HTTP on `VERCEL_DEV_PORT` as that
//!   crate does without Vercel's bridge; the runtime runs them side by side (`Manifest`).
//!
//! JavaScript, TypeScript and Python files come with C18, through the same discovery and routes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::buildlog::{self, BuildLog};

/// Where the manifest goes in the package's `app/`: the runtime's entry.
pub const MANIFEST: &str = ".forkrail/functions.json";

/// The `api/` files with extension `ext`, relative to `dir`, sorted: not in a folder or a file
/// starting with `_` or `.`, nor `node_modules`, `vendor` or `testdata`, nor Go's `_test` files.
pub fn discover(dir: &Path, ext: &str) -> Vec<String> {
    discover_any(dir, &[ext])
}

/// `discover` for several extensions at once (Node's), sorted. TypeScript declarations (`.d.ts`)
/// aren't functions.
pub fn discover_any(dir: &Path, exts: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    let api = dir.join("api");
    if !api.is_dir() {
        return found;
    }
    let mut stack = vec![api];
    while let Some(folder) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(['_', '.']) || matches!(name.as_str(), "node_modules" | "vendor" | "testdata") {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if exts.iter().any(|ext| name.ends_with(&format!(".{ext}")) && !(matches!(*ext, "go" | "rs") && name.ends_with(&format!("_test.{ext}")))) && !is_declaration(&name) {
                found.push(path.strip_prefix(dir).expect("under dir").to_string_lossy().replace('\\', "/"));
            }
        }
    }
    found.sort();
    found
}

/// Whether `path` matches the glob `pattern` as minimatch matches `functions` patterns and
/// `includeFiles`: `*` any run of characters but `/`, `**` as a whole segment any number of
/// segments, `?` one character but `/`, `[abc]`/`[a-z]`/`[!a]` one of a set, `{a,b}` either.
/// A leading `./` is ignored on both.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let path = path.trim_start_matches("./");
    expand_braces(pattern.trim_start_matches("./")).iter().any(|p| glob_at(p.as_bytes(), path.as_bytes()))
}

/// `a{b,c}d` as `abd` and `acd`, nested braces included.
fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let Some(open) = bytes.iter().position(|b| *b == b'{') else { return vec![pattern.to_owned()] };
    let mut depth = 0;
    let mut parts = Vec::new();
    let mut start = open + 1;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    parts.push(&pattern[start..i]);
                    let (head, tail) = (&pattern[..open], &pattern[i + 1..]);
                    if parts.len() == 1 {
                        // `{a}` is literal, as in minimatch.
                        return expand_braces(tail).into_iter().map(|t| format!("{head}{{{}}}{t}", parts[0])).collect();
                    }
                    return parts.iter().flat_map(|p| expand_braces(&format!("{head}{p}{tail}"))).collect();
                }
            }
            b',' if depth == 1 => {
                parts.push(&pattern[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    vec![pattern.to_owned()]
}

fn glob_at(p: &[u8], t: &[u8]) -> bool {
    match p {
        [] => t.is_empty(),
        // `**/` matches no segment or any number of them; a trailing `/**` everything below.
        [b'*', b'*', b'/', rest @ ..] => (0..=t.len()).filter(|i| *i == 0 || t[i - 1] == b'/').any(|i| glob_at(rest, &t[i..])),
        [b'*', b'*'] => true,
        [b'*', b'*', rest @ ..] => (0..=t.len()).any(|i| glob_at(rest, &t[i..])),
        [b'*', rest @ ..] => (0..=t.len()).take_while(|i| *i == 0 || t[i - 1] != b'/').any(|i| glob_at(rest, &t[i..])),
        [b'?', rest @ ..] => t.first().is_some_and(|c| *c != b'/') && glob_at(rest, &t[1..]),
        [b'[', rest @ ..] => {
            let Some(end) = rest.iter().skip(1).position(|b| *b == b']').map(|i| i + 1) else {
                return t.first() == Some(&b'[') && glob_at(rest, &t[1..]);
            };
            let (set, after) = (&rest[..end], &rest[end + 1..]);
            let Some(&c) = t.first() else { return false };
            let (negate, set) = match set.first() {
                Some(b'!' | b'^') => (true, &set[1..]),
                _ => (false, set),
            };
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == b'-' {
                    hit |= (set[i]..=set[i + 2]).contains(&c);
                    i += 3;
                } else {
                    hit |= set[i] == c;
                    i += 1;
                }
            }
            c != b'/' && hit != negate && glob_at(after, &t[1..])
        }
        [b'\\', c, rest @ ..] => t.first() == Some(c) && glob_at(rest, &t[1..]),
        [c, rest @ ..] => t.first() == Some(c) && glob_at(rest, &t[1..]),
    }
}

/// A TypeScript declaration file (`types.d.ts`, `.d.mts`, `.d.cts`), never a function.
fn is_declaration(name: &str) -> bool {
    [".d.ts", ".d.mts", ".d.cts"].iter().any(|d| name.ends_with(d))
}

/// Two files that would be served at the same path (`api/a.go` and `api/a/index.go`, or two
/// `[param]` files in one folder), which Vercel refuses too.
pub fn check_paths(files: &[String]) -> Result<()> {
    let mut seen: BTreeMap<String, &String> = BTreeMap::new();
    for file in files {
        let stem = file.rsplit_once('.').map_or(file.as_str(), |(s, _)| s);
        let key: Vec<String> = stem
            .split('/')
            .map(|s| if s.starts_with('[') { "[]".to_owned() } else { s.to_owned() })
            .collect();
        let mut key = key.join("/");
        if let Some(folder) = key.strip_suffix("/index") {
            key = folder.to_owned();
        }
        if let Some(other) = seen.insert(key, file) {
            bail!("{other} and {file} would both be served at the same path: rename one (Vercel refuses this too)");
        }
    }
    // A dynamic segment has one name at its place in every path (`api/[team].ts` and
    // `api/[team]/[id].ts`, not `api/[user].ts`), and a name is used once in a path.
    let mut names: BTreeMap<String, (&str, &String)> = BTreeMap::new();
    for file in files {
        let stem = file.rsplit_once('.').map_or(file.as_str(), |(s, _)| s);
        let mut prefix = String::new();
        let mut used: BTreeSet<&str> = BTreeSet::new();
        for segment in stem.split('/') {
            if let Some(name) = segment.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                if !used.insert(name) {
                    bail!("{file} uses [{name}] twice: a path's dynamic segments need names of their own (Vercel refuses this too)");
                }
                match names.get(&prefix) {
                    Some((other_name, other)) if *other_name != name => {
                        bail!("{other} and {file} name the same dynamic segment differently ([{other_name}] and [{name}]): use one name (Vercel refuses this too)");
                    }
                    Some(_) => {}
                    None => {
                        names.insert(prefix.clone(), (name, file));
                    }
                }
                prefix.push_str("[]/");
            } else {
                prefix.push_str(segment);
                prefix.push('/');
            }
        }
    }
    Ok(())
}

/// The runtime's manifest (crates/runtime, `app::Manifest`).
#[derive(Debug, Serialize)]
pub struct Manifest {
    pub programs: Vec<Program>,
}

#[derive(Debug, Serialize)]
pub struct Program {
    pub program: String,
    pub functions: Vec<Function>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dispatches: bool,
}

#[derive(Debug, Serialize)]
pub struct Function {
    pub file: String,
    pub paths: Vec<String>,
}

impl Function {
    pub fn of(file: &str) -> Function {
        Function { file: file.to_owned(), paths: forkrail_common::rules::file_paths(file) }
    }
}

// ---- Go ----

/// A Go function file's package and its handler: the first exported top-level function taking an
/// `http.ResponseWriter` and an `*http.Request`, as Vercel's analyzer finds it.
pub fn go_handler(text: &str) -> Option<(String, String)> {
    let package = crate::native::go_package(text)?.to_owned();
    let mut rest = text;
    while let Some(at) = rest.find("func ") {
        let line_start = at == 0 || rest[..at].ends_with('\n');
        let after = &rest[at + 5..];
        rest = after;
        if !line_start {
            continue;
        }
        let after = after.trim_start();
        // A method has a receiver first.
        if after.starts_with('(') {
            continue;
        }
        let name: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if !name.starts_with(|c: char| c.is_uppercase()) {
            continue;
        }
        let params = &after[name.len()..];
        let Some(params) = params.trim_start().strip_prefix('(') else { continue };
        let Some(end) = params.find(')') else { continue };
        let params = &params[..end];
        if params.contains("http.ResponseWriter") && params.contains("*http.Request") {
            return Some((package, name));
        }
    }
    None
}

/// The name Vercel's builder gives a handler while it builds: `Handler_api_users__id__go`.
pub fn renamed(handler: &str, file: &str) -> String {
    let slug: String = file.chars().map(|c| if c.is_whitespace() || matches!(c, '\\' | '/' | ']' | '[' | '-' | '.') { '_' } else { c }).collect();
    format!("{handler}_{slug}")
}

/// Rename every ` name` (a word) in a Go file to ` to`, as Vercel's builder does.
fn rename_word(text: &str, name: &str, to: &str) -> String {
    let needle = format!(" {name}");
    let mut out = String::with_capacity(text.len() + 64);
    let mut rest = text;
    while let Some(at) = rest.find(&needle) {
        let after = &rest[at + needle.len()..];
        let word_ends = !after.starts_with(|c: char| c.is_alphanumeric() || c == '_');
        out.push_str(&rest[..at]);
        if word_ends {
            out.push(' ');
            out.push_str(to);
        } else {
            out.push_str(&needle);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// A name the Go tool takes for a folder or file: `[id]` as `p_id_`.
fn go_safe(name: &str) -> String {
    name.replace('[', "p_").replace(']', "_")
}

/// The Go functions in `dir` (`files`, from `discover`), built into one program for Linux arm64
/// from a copy of the project in `work`. Returns the program.
pub fn build_go(dir: &Path, files: &[String], work: &Path, cache: &Path, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<PathBuf> {
    log.step(buildlog::BUILDING);
    let (copy, built) = prepare_go(dir, files, work, log)?;
    let mut env = env.clone();
    for (k, v) in [("GOOS", "linux"), ("GOARCH", "arm64"), ("CGO_ENABLED", "0"), ("GOTOOLCHAIN", "auto")] {
        env.entry(k.to_owned()).or_insert_with(|| v.to_owned());
    }
    env.entry("GOCACHE".to_owned()).or_insert_with(|| cache.join("go-build").display().to_string());
    env.entry("GOMODCACHE".to_owned()).or_insert_with(|| cache.join("go-mod").display().to_string());
    let program = work.join("go-out").join("functions");
    compile_go(&copy, &program, &built, &env, log)?;
    crate::native::check_program(&program)?;
    Ok(program)
}

/// A copy of the project in `work/go-src` with its functions' handlers renamed and the generated
/// dispatcher in `forkrail_functions/`; returns the copy and the functions it serves.
pub fn prepare_go(dir: &Path, files: &[String], work: &Path, log: &dyn BuildLog) -> Result<(PathBuf, Vec<String>)> {
    let copy = work.join("go-src");
    copy_project(dir, &copy)?;
    // The module: the project's, or one made up for a project without a go.mod.
    let module = match std::fs::read_to_string(copy.join("go.mod")) {
        Ok(text) => text.lines().find_map(|l| l.trim().strip_prefix("module ")).map(|m| m.trim().trim_matches('"').to_owned()).context("go.mod has no module line")?,
        Err(_) => {
            std::fs::write(copy.join("go.mod"), "module forkrail.local/app\n\ngo 1.22\n")?;
            "forkrail.local/app".to_owned()
        }
    };
    struct Handler {
        file: String,
        alias: String,
        func: String,
    }
    let mut packages: BTreeMap<String, String> = BTreeMap::new();
    let mut handlers: Vec<Handler> = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(dir.join(file)).with_context(|| format!("reading {file}"))?;
        let Some((package, name)) = go_handler(&text) else {
            log.info(&format!("warning: {file} has no exported func(http.ResponseWriter, *http.Request), so it isn't a function (name it _{} to say so)", file.rsplit('/').next().unwrap_or(file)));
            continue;
        };
        if package == "main" && dir.join("go.mod").is_file() {
            bail!("{file}: please change `package main` to `package handler` (Vercel refuses it too)");
        }
        let func = renamed(&name, file);
        let mut text = rename_word(&text, &name, &func);
        if package == "main" {
            text = text.replacen("package main", "package handler", 1);
        }
        // The copy, with Go-safe names for `[param]` folders and files.
        let (folder, base) = file.rsplit_once('/').unwrap_or(("", file));
        let safe_folder: String = folder.split('/').map(go_safe).collect::<Vec<_>>().join("/");
        let to = copy.join(&safe_folder).join(go_safe(base));
        if go_safe(base) != base || safe_folder != folder {
            let _ = std::fs::remove_file(copy.join(file));
        }
        std::fs::create_dir_all(to.parent().expect("has a folder"))?;
        std::fs::write(&to, text)?;
        if safe_folder != folder {
            // The folder's other Go files go along.
            for entry in std::fs::read_dir(dir.join(folder)).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".go") && !files.contains(&format!("{folder}/{name}")) {
                    std::fs::copy(entry.path(), copy.join(&safe_folder).join(&name))?;
                }
            }
        }
        let import = if safe_folder.is_empty() { module.clone() } else { format!("{module}/{safe_folder}") };
        let next = packages.len();
        let alias = packages.entry(import).or_insert_with(|| format!("f{next}")).clone();
        handlers.push(Handler { file: file.clone(), alias, func });
    }
    if handlers.is_empty() {
        bail!("none of the Go files in api/ has an exported func(http.ResponseWriter, *http.Request)");
    }
    let mut source = String::from(GO_HEADER);
    source.push_str("import (\n\t\"net/http\"\n\t\"os\"\n\t\"strings\"\n\n");
    for (import, alias) in &packages {
        source.push_str(&format!("\t{alias} {import:?}\n"));
    }
    source.push_str(")\n\nvar exact = map[string]http.HandlerFunc{\n");
    let mut dynamic = String::new();
    let mut owners = String::new();
    for h in &handlers {
        for path in forkrail_common::rules::file_paths(&h.file) {
            source.push_str(&format!("\t{path:?}: {}.{},\n", h.alias, h.func));
            owners.push_str(&format!("\t{path:?}: {:?},\n", h.file));
        }
        let stem = h.file.rsplit_once('.').map_or(h.file.as_str(), |(s, _)| s);
        let mut segments: Vec<&str> = stem.split('/').collect();
        if segments.len() > 1 && segments.last() == Some(&"index") {
            segments.pop();
        }
        if segments.iter().any(|s| s.starts_with('[')) {
            let list: Vec<String> = segments.iter().map(|s| format!("{s:?}")).collect();
            dynamic.push_str(&format!("\t{{[]string{{{}}}, {}.{}}},\n", list.join(", "), h.alias, h.func));
        }
    }
    source.push_str("}\n\n// Each path's function file.\nvar handlerOf = map[string]string{\n");
    source.push_str(&owners);
    source.push_str("}\n\nvar dynamic = []route{\n");
    source.push_str(&dynamic);
    source.push_str("}\n");
    source.push_str(GO_SERVE);
    let main_dir = copy.join("forkrail_functions");
    std::fs::create_dir_all(&main_dir)?;
    std::fs::write(main_dir.join("main.go"), &source)?;
    Ok((copy, handlers.into_iter().map(|h| h.file).collect()))
}

/// `go build` the dispatcher of a prepared copy into `program`, for the GOOS and GOARCH in `env`.
pub fn compile_go(copy: &Path, program: &Path, functions: &[String], env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
    let env = env.clone();
    let vendored = copy.join("vendor/modules.txt").is_file();
    if !vendored && !copy.join("go.sum").is_file() {
        log.info("go mod tidy");
        buildlog::run(Command::new("go").args(["mod", "tidy"]).current_dir(copy), "go mod tidy", &env, log).map_err(|e| crate::native::missing_tool(e, "go", "Go"))?;
    }
    let mut flags = match env.get("GO_BUILD_FLAGS").filter(|f| !f.trim().is_empty()) {
        Some(flags) => crate::native::split_words(flags),
        None => vec!["-ldflags=-s -w".to_owned()],
    };
    flags.push(if vendored { "-mod=vendor".to_owned() } else { "-mod=mod".to_owned() });
    std::fs::create_dir_all(program.parent().expect("has a folder"))?;
    log.info(&format!("go build -trimpath {} ./forkrail_functions: {} functions ({})", flags.join(" "), functions.len(), functions.join(", ")));
    buildlog::run(
        Command::new("go").args(["build", "-trimpath"]).args(&flags).arg("-o").arg(program).arg("./forkrail_functions").current_dir(copy),
        "go build",
        &env,
        log,
    )
    .map_err(|e| crate::native::missing_tool(e, "go", "Go"))?;
    Ok(())
}

const GO_HEADER: &str = "// Code generated by forkrail: serves the project's Go functions (api/*.go), as Vercel's\n// zero-config routes do. Do not edit.\npackage main\n\n";

const GO_SERVE: &str = r#"
type route struct {
	segments []string
	handler  http.HandlerFunc
}

func main() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "3000"
	}
	if err := http.ListenAndServe("127.0.0.1:"+port, http.HandlerFunc(serve)); err != nil {
		panic(err)
	}
}

// The function the router's routes reached, else the one the path is.
func serve(w http.ResponseWriter, r *http.Request) {
	named := r.Header.Get("X-Forkrail-Function")
	r.Header.Del("X-Forkrail-Function")
	if h, ok := exact[named]; ok && named != "" {
		h(w, r)
		return
	}
	if h, ok := exact[r.URL.Path]; ok {
		h(w, r)
		return
	}
	got := strings.Split(strings.Trim(r.URL.Path, "/"), "/")
	for _, d := range dynamic {
		if len(d.segments) != len(got) {
			continue
		}
		params := map[string]string{}
		matched := true
		for i, s := range d.segments {
			if strings.HasPrefix(s, "[") && strings.HasSuffix(s, "]") {
				params[s[1:len(s)-1]] = got[i]
			} else if s != got[i] {
				matched = false
				break
			}
		}
		if matched {
			q := r.URL.Query()
			for k, v := range params {
				q.Set(k, v)
			}
			r.URL.RawQuery = q.Encode()
			d.handler(w, r)
			return
		}
	}
	// A router from before the header sends a rewritten request with its own path: with one
	// function, it can only be that one's.
	if named == "" && len(exact) > 0 {
		var only http.HandlerFunc
		distinct := 0
		seen := map[string]bool{}
		for path, h := range exact {
			key := handlerOf[path]
			if !seen[key] {
				seen[key] = true
				distinct++
				only = h
			}
		}
		if distinct == 1 {
			only(w, r)
			return
		}
	}
	w.Header().Set("Content-Type", "text/plain; charset=utf-8")
	w.WriteHeader(http.StatusNotFound)
	w.Write([]byte("The page could not be found.\n\nNOT_FOUND\n"))
}
"#;

/// Copy the project's files into `to`, leaving out what a build never needs from it.
fn copy_project(dir: &Path, to: &Path) -> Result<()> {
    for entry in ignore::WalkBuilder::new(dir)
        .standard_filters(false)
        .follow_links(false)
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | "node_modules" | ".vercel" | "target")))
        .build()
    {
        let entry = entry?;
        if !entry.path().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(dir).expect("walked from dir");
        let dest = to.join(rel);
        std::fs::create_dir_all(dest.parent().expect("has a folder"))?;
        std::fs::copy(entry.path(), &dest).with_context(|| format!("copying {}", rel.display()))?;
    }
    Ok(())
}

// ---- Rust ----

/// A Rust function: its file and the `[[bin]]` it's built as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustFunction {
    pub file: String,
    pub bin: String,
}

/// Each `api/*.rs` file's `[[bin]]` in the project's `Cargo.toml` (by its `path`), as Vercel's
/// Rust runtime needs them listed.
pub fn rust_bins(dir: &Path, files: &[String]) -> Result<Vec<RustFunction>> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).context("api/ has Rust files, but there's no Cargo.toml at the project's top")?;
    let manifest: toml::Table = toml::from_str(&text).context("Cargo.toml isn't valid TOML")?;
    let bins: Vec<(String, String)> = manifest
        .get("bin")
        .and_then(|b| b.as_array())
        .into_iter()
        .flatten()
        .filter_map(|b| Some((b.get("name")?.as_str()?.to_owned(), b.get("path")?.as_str()?.trim_start_matches("./").to_owned())))
        .collect();
    let mut out = Vec::new();
    let mut missing = Vec::new();
    for file in files {
        match bins.iter().find(|(_, path)| path == file) {
            Some((name, _)) => out.push(RustFunction { file: file.clone(), bin: name.clone() }),
            None => missing.push(file.as_str()),
        }
    }
    if !missing.is_empty() {
        let examples: Vec<String> = missing.iter().map(|f| format!("[[bin]]\nname = {:?}\npath = {f:?}", f.rsplit('/').next().unwrap_or(f).trim_end_matches(".rs").replace(['[', ']'], "_"))).collect();
        bail!("{} not listed in Cargo.toml: Vercel's Rust runtime needs each api/ file as a [[bin]], e.g.\n{}", missing.join(", "), examples.join("\n"));
    }
    let names: BTreeSet<&str> = out.iter().map(|f| f.bin.as_str()).collect();
    if names.len() != out.len() {
        bail!("two api/ files are the same [[bin]] in Cargo.toml");
    }
    Ok(out)
}

/// Whether a `Cargo.toml` depends on Vercel's `vercel_runtime` crate.
pub fn uses_vercel_runtime(dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else { return false };
    let Ok(manifest) = toml::from_str::<toml::Table>(&text) else { return false };
    manifest.get("dependencies").and_then(|d| d.as_table()).is_some_and(|d| d.contains_key("vercel_runtime") || d.contains_key("vercel-runtime"))
}

/// Build the Rust functions' binaries for Linux arm64; returns each with its program.
pub fn build_rust(dir: &Path, functions: &[RustFunction], cache: &Path, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<Vec<(RustFunction, PathBuf)>> {
    log.step(buildlog::BUILDING);
    let bins: Vec<String> = functions.iter().map(|f| f.bin.clone()).collect();
    let programs = crate::native::build_rust_bins(dir, &bins, cache, env, log)?;
    Ok(functions.iter().cloned().zip(programs).collect())
}

/// The manifest for Rust functions, each program at `.forkrail/fn/<bin>`.
pub fn rust_manifest(functions: &[RustFunction]) -> Manifest {
    Manifest {
        programs: functions
            .iter()
            .map(|f| Program {
                program: format!(".forkrail/fn/{}", f.bin),
                functions: vec![Function::of(&f.file)],
                env: [("VERCEL_DEV_PORT".to_owned(), "{port}".to_owned())].into(),
                dispatches: false,
            })
            .collect(),
    }
}

/// The manifest for Go's one program, which dispatches itself.
pub fn go_manifest(files: &[String]) -> Manifest {
    Manifest { programs: vec![Program { program: ".forkrail/functions".to_owned(), functions: files.iter().map(|f| Function::of(f)).collect(), env: BTreeMap::new(), dispatches: true }] }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-fns-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    #[test]
    fn files_as_vercel_finds_them() {
        let dir = project(&[
            ("api/index.go", ""),
            ("api/users/[id].go", ""),
            ("api/users/index.go", ""),
            ("api/_lib/db.go", ""),
            ("api/_helpers.go", ""),
            ("api/.hidden.go", ""),
            ("api/handler_test.go", ""),
            ("api/hello.rs", ""),
            ("main.go", ""),
        ]);
        assert_eq!(discover(&dir, "go"), ["api/index.go", "api/users/[id].go", "api/users/index.go"]);
        assert_eq!(discover(&dir, "rs"), ["api/hello.rs"]);
        assert!(check_paths(&discover(&dir, "go")).is_ok());
        assert!(check_paths(&["api/a.go".into(), "api/a/index.go".into()]).is_err());
        assert!(check_paths(&["api/[a].go".into(), "api/[b].go".into()]).is_err());
    }

    #[test]
    fn go_handlers() {
        let text = "// Package handler.\npackage handler\n\nimport (\n\t\"fmt\"\n\t\"net/http\"\n)\n\nfunc helper() {}\n\nfunc (s *S) Serve(w http.ResponseWriter, r *http.Request) {}\n\nfunc Handler(w http.ResponseWriter, r *http.Request) {\n\tfmt.Fprintf(w, \"hi\")\n}\n";
        assert_eq!(go_handler(text), Some(("handler".into(), "Handler".into())));
        assert_eq!(go_handler("package api\n\nfunc Index(\n\tw http.ResponseWriter,\n\tr *http.Request,\n) {}\n"), Some(("api".into(), "Index".into())));
        assert_eq!(go_handler("package api\n\nfunc Other(x int) {}\n"), None);
        assert_eq!(renamed("Handler", "api/users/[id].go"), "Handler_api_users__id__go");
        assert_eq!(rename_word(text, "Handler", "Handler_x"), text.replace("func Handler(", "func Handler_x("));
        assert_eq!(rename_word("var _ = HandlerFunc\nfunc Handler(", "Handler", "H2"), "var _ = HandlerFunc\nfunc H2(");
    }

    /// The dispatcher, built for this machine and asked over HTTP. Needs Go: `cargo test -p
    /// forkrail-cli go_dispatcher -- --ignored`. With FORKRAIL_TEST_KEEP=DIR the program and its
    /// manifest are left there, for a local router.
    #[test]
    #[ignore]
    fn go_dispatcher_serves_each_function() {
        let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-apps/go-functions");
        let files = discover(&app, "go");
        let work = crate::package::TempDir::new("go-dispatch").unwrap();
        let log = crate::buildlog::Terminal;
        let (copy, built) = prepare_go(&app, &files, work.path(), &log).unwrap();
        let program = work.path().join("functions");
        compile_go(&copy, &program, &built, &BTreeMap::new(), &log).unwrap();
        if let Ok(keep) = std::env::var("FORKRAIL_TEST_KEEP") {
            std::fs::create_dir_all(Path::new(&keep).join(".forkrail")).unwrap();
            std::fs::copy(&program, Path::new(&keep).join(".forkrail/functions")).unwrap();
            std::fs::write(Path::new(&keep).join(MANIFEST), serde_json::to_vec_pretty(&go_manifest(&built)).unwrap()).unwrap();
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut child = Command::new(&program).env("PORT", port.to_string()).spawn().unwrap();
        let ask = |path: &str, header: Option<&str>| -> String {
            for _ in 0..200 {
                if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                    use std::io::{Read, Write};
                    let extra = header.map(|h| format!("X-Forkrail-Function: {h}\r\n")).unwrap_or_default();
                    write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n{extra}Connection: close\r\n\r\n").unwrap();
                    let mut out = String::new();
                    s.read_to_string(&mut out).unwrap();
                    return out;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            panic!("the dispatcher never listened");
        };
        let index = ask("/api", None);
        let hello = ask("/api/hello?name=ada", None);
        let user = ask("/api/users/42", None);
        let named = ask("/anything", Some("/api/hello"));
        let routed = ask("/api/users/[id]?id=7", Some("/api/users/[id]"));
        let missing = ask("/api/nope", None);
        child.kill().unwrap();
        assert!(index.contains("\"function\":\"api/index.go\"") && index.contains("hi from api/_lib"), "{index}");
        assert!(hello.contains("hello, ada (from api/hello.go)"), "{hello}");
        assert!(user.contains("\"id\":\"42\""), "{user}");
        assert!(named.contains("hello, world"), "{named}");
        assert!(routed.contains("\"id\":\"7\""), "{routed}");
        assert!(missing.starts_with("HTTP/1.1 404") && missing.contains("NOT_FOUND"), "{missing}");
    }

    #[test]
    fn rust_bins_by_path() {
        let dir = project(&[("Cargo.toml", "[package]\nname = \"x\"\n\n[[bin]]\nname = \"hello\"\npath = \"api/hello.rs\"\n\n[[bin]]\nname = \"user\"\npath = \"./api/users/[id].rs\"\n")]);
        let bins = rust_bins(&dir, &["api/hello.rs".into(), "api/users/[id].rs".into()]).unwrap();
        assert_eq!(bins.iter().map(|b| b.bin.as_str()).collect::<Vec<_>>(), ["hello", "user"]);
        let err = rust_bins(&dir, &["api/other.rs".into()]).unwrap_err().to_string();
        assert!(err.contains("api/other.rs not listed") && err.contains("path = \"api/other.rs\""), "{err}");
        let manifest = serde_json::to_value(rust_manifest(&bins)).unwrap();
        assert_eq!(manifest["programs"][1]["functions"][0]["paths"], serde_json::json!(["/api/users/[id].rs", "/api/users/[id]"]));
        assert_eq!(manifest["programs"][0]["env"]["VERCEL_DEV_PORT"], "{port}");
    }
}
