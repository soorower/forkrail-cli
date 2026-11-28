//! Routing Middleware for projects that aren't a Build Output (static sites, functions in `api/`),
//! as Vercel finds and builds it (`@vercel/fs-detectors` and `@vercel/node` in Vercel CLI 65.16.0;
//! docs/data/c16/fw-vite-middleware/notes.md):
//!
//! - `vercel.json` `proxy: { entrypoint, matcher }`: any `.js`/`.ts` file of the project, Node
//!   runtime. Wins over the file convention.
//! - Else a `middleware.js` or `middleware.ts` at the project's root (not for Next.js, whose own
//!   server runs its middleware, nor Storybook), Node unless its `config.runtime` says `edge`.
//! - Its `config.matcher` (or `proxy.matcher`, never both) becomes the route's pattern
//!   (`forkrail_common::rules::middleware::route`); none matches every path.
//!
//! The file is bundled with esbuild into one ES module (`.forkrail/fn/<name>.mjs`), npm packages
//! included, with Vercel's resolve conditions for its runtime (Edge: `edge-light`, `browser`, ...),
//! and served by the deployment's dispatcher, which the router calls by its name. Both runtimes run
//! on Node, as Vercel now runs middleware by default.
//!
//! The `config` export is read without running the file, as Vercel reads it statically: its object
//! literal is evaluated alone (in a context with nothing in it), so a matcher computed from
//! anything else is refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::buildlog::{self, BuildLog};
use forkrail_common::rules::SourceRoute;

/// The file convention's names, at the project's root.
const ROOT_FILES: [&str; 2] = ["middleware.js", "middleware.ts"];
/// Frameworks that run root middleware themselves (Next.js) or that Vercel doesn't give it.
const OWN_MIDDLEWARE: [&str; 2] = ["Next.js", "Storybook"];
/// Frameworks Vercel refuses `proxy` for: they build their own routing middleware.
const NO_PROXY: [&str; 2] = ["Next.js", "Astro"];
/// Vercel's resolve conditions for an Edge function's imports (`@vercel/node`).
const EDGE_CONDITIONS: &str = "edge-light,worker,browser,module,import,require";
/// Lambda's oldest Node, which the bundle must run on.
const TARGET: &str = "node22";

/// A project's middleware, found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Relative to the project, forward slashes.
    pub entry: String,
    /// Its function's name: `middleware` for the file convention, `proxy` for `vercel.json`'s.
    pub name: &'static str,
    /// The paths it runs on; `None` every path.
    pub matcher: Option<Vec<String>>,
    /// Bundled for Vercel's Edge runtime (its `config.runtime` says so); run on Node all the same.
    pub edge: bool,
}

impl Found {
    /// Its function's path, as the router names it.
    pub fn function(&self) -> String {
        format!("/{}", self.name)
    }

    /// Its route, to go in the program where Vercel's `mergeRoutes` puts it.
    pub fn route(&self) -> Result<SourceRoute> {
        forkrail_common::rules::middleware::route(self.name, self.matcher.as_deref()).map_err(anyhow::Error::msg)
    }

    /// Where its bundle goes in the package's `app/`.
    pub fn bundle(&self) -> String {
        format!("{}/{}.mjs", crate::build_output::FUNCTIONS_DIR, self.name)
    }

    /// Its entry in a dispatcher at `.forkrail/functions.mjs`, as `forkrail.buildOutput` and
    /// `forkrail.functions` take one (crates/runtime/src/shim.mjs).
    pub fn dispatcher_entry(&self) -> String {
        let name = serde_json::to_string(&self.function()).expect("a string");
        let target = serde_json::to_string(&format!("./fn/{}.mjs", self.name)).expect("a string");
        format!("{{ name: {name}, paths: [{name}], helpers: false, edge: {}, load: () => import(new URL({target}, import.meta.url).href) }}", self.edge)
    }
}

/// The project's middleware, if it has one: `vercel.json`'s `proxy`, else a root `middleware.js`
/// or `.ts`. `framework` is the framework's name when one builds the project.
pub fn find(dir: &Path, config: Option<&Value>, framework: Option<&str>) -> Result<Option<Found>> {
    if let Some(proxy) = config.and_then(|c| c.get("proxy")) {
        if let Some(name) = framework.filter(|f| NO_PROXY.contains(f)) {
            bail!("`proxy`: {name} builds its own routing middleware, so Vercel refuses `proxy` for it too");
        }
        let Some(proxy) = proxy.as_object() else { bail!("`proxy` must be an object with `entrypoint`") };
        if let Some(key) = proxy.keys().find(|k| !["entrypoint", "matcher"].contains(&k.as_str())) {
            bail!("`proxy` has an unknown property `{key}`");
        }
        let Some(entry) = proxy.get("entrypoint").and_then(Value::as_str) else { bail!("`proxy.entrypoint` must be the middleware's file") };
        check_entry(entry)?;
        if !dir.join(entry).is_file() {
            bail!("`proxy.entrypoint`: {entry} isn't a file in the project");
        }
        let matcher = match proxy.get("matcher") {
            None => None,
            Some(m) => Some(matchers(m).context("`proxy.matcher`")?),
        };
        let own = read_config(dir, entry)?;
        if matcher.is_some() && own.matcher.is_some() {
            bail!("{entry}: `proxy.matcher` in vercel.json and `config.matcher` in the file; give the matcher in one place, as Vercel asks");
        }
        if own.edge {
            bail!("{entry}: `proxy` runs on Node; its `config.runtime` can't be `edge` (Vercel refuses it too)");
        }
        return Ok(Some(Found { entry: entry.to_owned(), name: "proxy", matcher: matcher.or(own.matcher), edge: false }));
    }
    if framework.is_some_and(|f| OWN_MIDDLEWARE.contains(&f)) {
        return Ok(None);
    }
    let found: Vec<&str> = ROOT_FILES.iter().copied().filter(|f| dir.join(f).is_file()).collect();
    let Some(entry) = found.first() else { return Ok(None) };
    if found.len() > 1 {
        bail!("the project has both middleware.js and middleware.ts; keep one");
    }
    let own = read_config(dir, entry)?;
    Ok(Some(Found { entry: (*entry).to_owned(), name: "middleware", matcher: own.matcher, edge: own.edge }))
}

/// `proxy.entrypoint`'s rules (Vercel's `validateProxy`): a relative path to a `.js` or `.ts` file,
/// no `.`/`..` segments, no query, fragment or control characters. Python's `module:attr` form
/// isn't run by forkrail.
fn check_entry(entry: &str) -> Result<()> {
    if entry.contains(':') && !entry.contains('/') && !entry.ends_with(".js") && !entry.ends_with(".ts") {
        bail!("`proxy.entrypoint` {entry:?}: Python middleware isn't supported yet");
    }
    let ok = !entry.is_empty()
        && !entry.starts_with(['/', '\\'])
        && !entry.contains(['?', '#', '\\'])
        && !entry.chars().any(char::is_control)
        && entry.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
        && (entry.ends_with(".js") || entry.ends_with(".ts"))
        && !entry.ends_with(".d.ts");
    if !ok {
        bail!("`proxy.entrypoint` {entry:?} must be a relative path to a .js or .ts file");
    }
    Ok(())
}

/// A matcher value: one path or a non-empty list, each starting with `/`.
fn matchers(value: &Value) -> Result<Vec<String>> {
    let list: Vec<String> = match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) if !items.is_empty() => items.iter().map(|i| i.as_str().map(str::to_owned)).collect::<Option<_>>().context("each matcher must be a path")?,
        _ => bail!("a matcher is a path or a list of paths (Vercel takes no `{{ source, has }}` objects outside Next.js)"),
    };
    if let Some(bad) = list.iter().find(|m| !m.starts_with('/')) {
        bail!("matchers must start with \"/\": {bad:?} doesn't");
    }
    Ok(list)
}

/// What a middleware file's `config` export says.
#[derive(Debug, Default, PartialEq, Eq)]
struct OwnConfig {
    matcher: Option<Vec<String>>,
    edge: bool,
}

/// Read `export const config = { ... }` from the file without running it.
fn read_config(dir: &Path, entry: &str) -> Result<OwnConfig> {
    let source = std::fs::read_to_string(dir.join(entry)).with_context(|| format!("reading {entry}"))?;
    let Some(literal) = config_literal(&source) else { return Ok(OwnConfig::default()) };
    let value = evaluate(&literal).with_context(|| format!("{entry}: its `config` must be plain values Vercel can read without running the file"))?;
    let Some(object) = value.as_object() else { bail!("{entry}: `config` must be an object") };
    let matcher = match object.get("matcher") {
        None | Some(Value::Null) => None,
        Some(m) => Some(matchers(m).with_context(|| format!("{entry}: `config.matcher`"))?),
    };
    let edge = match object.get("runtime").and_then(Value::as_str) {
        None | Some("nodejs") => false,
        Some("edge" | "experimental-edge") => true,
        Some(other) => bail!("{entry}: `config.runtime` {other:?} isn't one of `nodejs`, `edge`"),
    };
    Ok(OwnConfig { matcher, edge })
}

/// The object literal of `export const config = ...` in a JavaScript or TypeScript file (a type
/// annotation and `as const` or `satisfies` after it allowed), outside strings and comments.
fn config_literal(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'"' | b'\'' | b'`') => i = skip_string(bytes, i, quote),
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = skip_line(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_block(bytes, i),
            b'e' if source[i..].starts_with("export") && (i == 0 || !ident(bytes[i - 1])) => {
                if let Some(literal) = config_after(source, i + 6) {
                    return Some(literal);
                }
                i += 6;
            }
            _ => i += 1,
        }
    }
    None
}

/// `const config ... = { ... }` right after an `export`.
fn config_after(source: &str, from: usize) -> Option<String> {
    let rest = source[from..].trim_start().strip_prefix("const")?.trim_start().strip_prefix("config")?;
    if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '$') {
        return None;
    }
    let eq = rest.find('=')?;
    let value_at = source.len() - rest.len() + eq + 1;
    let open = value_at + source[value_at..].find(|c: char| !c.is_whitespace())?;
    if source.as_bytes()[open] != b'{' {
        return None;
    }
    balanced(source, open).map(|end| source[open..end].to_owned())
}

fn ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Past the string starting at `at` (its quote).
fn skip_string(bytes: &[u8], at: usize, quote: u8) -> usize {
    let mut i = at + 1;
    while i < bytes.len() && bytes[i] != quote {
        if bytes[i] == b'\\' {
            i += 1;
        }
        i += 1;
    }
    i + 1
}

fn skip_line(bytes: &[u8], at: usize) -> usize {
    let mut i = at;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block(bytes: &[u8], at: usize) -> usize {
    let mut i = at + 2;
    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
        i += 1;
    }
    i + 2
}

/// The end of the `{ ... }` starting at `open`, past its closing brace.
fn balanced(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'[' | b'(' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' | b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
                i += 1;
            }
            quote @ (b'"' | b'\'' | b'`') => i = skip_string(bytes, i, quote),
            b'/' if bytes.get(i + 1) == Some(&b'/') => i = skip_line(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_block(bytes, i),
            _ => i += 1,
        }
    }
    None
}

/// An object literal's value, evaluated by Node in a context with no globals of the project's.
fn evaluate(literal: &str) -> Result<Value> {
    const SCRIPT: &str = "const vm = require('node:vm'); let text = ''; process.stdin.on('data', (d) => (text += d)); \
        process.stdin.on('end', () => { const v = vm.runInNewContext('(' + text + ')', Object.create(null), { timeout: 1000 }); \
        process.stdout.write(JSON.stringify(v === undefined ? null : v)); });";
    use std::io::Write as _;
    let mut child = Command::new("node")
        .args(["-e", SCRIPT])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running node to read the middleware's config")?;
    child.stdin.take().expect("piped").write_all(literal.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().find(|l| l.contains("Error")).unwrap_or("it didn't evaluate");
        bail!("{}", line.trim());
    }
    serde_json::from_slice(&out.stdout).context("its config isn't plain values")
}

/// Bundle the middleware into `work`: the file for the package's `.forkrail/fn/<name>.mjs`.
pub fn build(dir: &Path, found: &Found, work: &Path, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<PathBuf> {
    let out = work.join(format!("{}.mjs", found.name));
    let mut command = crate::nodeapp::esbuild(dir);
    command
        .arg(&found.entry)
        .args(["--bundle", "--format=esm", "--log-level=warning", "--sourcemap=inline", "--legal-comments=none"])
        .arg(format!("--target={TARGET}"))
        .arg(format!("--outfile={}", out.display()))
        .current_dir(dir);
    if found.edge {
        // As Vercel bundles an Edge function: Web builds of packages; Node's modules only by
        // `node:` name, which Node itself serves here.
        command.args(["--platform=neutral", "--main-fields=browser,module,main", "--external:node:*"]).arg(format!("--conditions={EDGE_CONDITIONS}"));
    } else {
        // CommonJS packages bundled into an ES module still `require` Node's own modules.
        command.args(["--platform=node", "--banner:js=import { createRequire as __forkrailRequire } from 'node:module'; const require = __forkrailRequire(import.meta.url);"]);
    }
    log.info(&format!("bundling the Routing Middleware {} with esbuild ({} runtime, run on Node)", found.entry, if found.edge { "Edge" } else { "Node" }));
    buildlog::run(&mut command, "esbuild", env, log).context("bundling the middleware")?;
    Ok(out)
}

/// The dispatcher of a deployment whose only function is its middleware (a static site).
pub fn dispatcher_source(found: &Found) -> String {
    format!(
        "// Written by forkrail: the project's Routing Middleware, served by forkrail's runtime (shim.mjs),\n\
         // which the router calls by its name.\n\
         const serve = globalThis[Symbol.for('forkrail.buildOutput')];\n\
         if (typeof serve !== 'function') throw new Error(\"forkrail: this app runs under forkrail's runtime\");\n\
         serve({{ functions: [\n  {},\n] }});\n",
        found.dispatcher_entry()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-mw-{}", forkrail_common::random_hex(6)));
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_config_literal_is_found_without_running_the_file() {
        let source = "import { next } from '@vercel/functions'\n// export const config = { nope: 1 }\nexport const config: Config = {\n  matcher: ['/', '/items/:id(\\\\d{1,})', \"/a}b\"], // a } in a comment\n  runtime: 'edge',\n} as const\nexport default function m() {}\n";
        let literal = config_literal(source).unwrap();
        assert!(literal.starts_with("{\n  matcher") && literal.ends_with('}'), "{literal}");
        assert_eq!(config_literal("export const configured = { a: 1 }"), None);
        assert_eq!(config_literal("export default function m() {}"), None);
    }

    #[test]
    fn the_file_convention_and_proxy_are_found_as_vercel_finds_them() {
        // The Vite app's middleware, as written (test-apps/fw-vite-middleware).
        let source = "export const config = {\n  matcher: ['/', '/old', '/api/:path*', '/items/:id(\\\\d{1,})'],\n}\nexport default function middleware(request) {}\n";
        let dir = project(&[("middleware.ts", source)]);
        let found = find(&dir, None, Some("Vite")).unwrap().unwrap();
        assert_eq!(found, Found { entry: "middleware.ts".into(), name: "middleware", matcher: Some(vec!["/".into(), "/old".into(), "/api/:path*".into(), "/items/:id(\\d{1,})".into()]), edge: false });
        assert_eq!(found.route().unwrap().middleware.as_deref(), Some("/middleware"));
        // Next.js runs its own.
        assert_eq!(find(&dir, None, Some("Next.js")).unwrap(), None);
        // `proxy` wins, with its matcher.
        let dir = project(&[("middleware.ts", source), ("src/proxy.ts", "export default () => {}")]);
        let config = serde_json::json!({"proxy": {"entrypoint": "src/proxy.ts", "matcher": "/x"}});
        let found = find(&dir, Some(&config), Some("Vite")).unwrap().unwrap();
        assert_eq!((found.name, found.entry.as_str(), found.matcher.clone()), ("proxy", "src/proxy.ts", Some(vec!["/x".to_owned()])));
        assert!(found.dispatcher_entry().contains(r#"name: "/proxy", paths: ["/proxy"], helpers: false, edge: false"#), "{}", found.dispatcher_entry());
    }

    #[test]
    fn mistakes_are_refused_as_vercel_refuses_them() {
        let refused = |files: &[(&str, &str)], config: Value, framework: &str, expect: &str| {
            let dir = project(files);
            let err = find(&dir, Some(&config), Some(framework)).unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains(expect), "{config}: {text}");
        };
        let file = [("proxy.ts", "export const config = { matcher: '/a', runtime: 'edge' }")];
        refused(&file, serde_json::json!({"proxy": {"entrypoint": "proxy.ts"}}), "Vite", "can't be `edge`");
        refused(&file, serde_json::json!({"proxy": {"entrypoint": "proxy.ts", "matcher": "/b"}}), "Vite", "in one place");
        refused(&file, serde_json::json!({"proxy": {"entrypoint": "../proxy.ts"}}), "Vite", "relative path");
        refused(&file, serde_json::json!({"proxy": {"entrypoint": "nope.ts"}}), "Vite", "isn't a file");
        refused(&file, serde_json::json!({"proxy": {"entrypoint": "proxy.ts"}}), "Astro", "builds its own");
        refused(&file, serde_json::json!({"proxy": {"entrypoint": "app:proxy"}}), "Vite", "Python");
        refused(&[("p.ts", "")], serde_json::json!({"proxy": {"entrypoint": "p.ts", "matcher": ["x"]}}), "Vite", "start with");
        refused(&[("p.ts", "const m = '/a'; export const config = { matcher: m }")], serde_json::json!({"proxy": {"entrypoint": "p.ts"}}), "Vite", "plain values");
        let dir = project(&[("middleware.js", ""), ("middleware.ts", "")]);
        assert!(find(&dir, None, None).unwrap_err().to_string().contains("both"));
    }
}
