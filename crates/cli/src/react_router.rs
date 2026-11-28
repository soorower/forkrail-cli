//! React Router (framework mode, 7 and later) and Remix 2 with Vite, as Vercel deploys them.
//!
//! Neither framework's own adapter writes a Build Output on Vercel: Vercel's builder
//! (`@vercel/remix-builder` 31.0.0, Apache-2.0, its "build-vite" path) packages the app's Vite build
//! once the build has run. forkrail does that work here, ported from the builder and never running
//! it, and writes what `vercel build` would write, `.vercel/output`, which build_output.rs then
//! imports as any framework's (docs/data/c16/remix-builder-notes.md has the builder read line by
//! line):
//!
//! - **The build** runs as on Vercel: `vercel.json`'s `buildCommand`, else the `vercel-build`
//!   script, else `build`, else `react-router build` / `remix build`. The app's own preset
//!   (`vercelPreset()` from `@vercel/react-router` or `@vercel/remix`, a dependency of the app
//!   that runs inside its build) writes `.vercel/react-router-build-result.json` (or
//!   `remix-build-result.json`): the server bundles, which route each bundle serves, and each one's
//!   `config` (`memory`, `maxDuration`). Without the preset Vercel guesses one bundle,
//!   `build/server/index.js`, for the index route, and so does this. A build that writes its own
//!   `.vercel/output` (version 3) is deployed as it is, as on Vercel.
//! - **One function per server bundle**, entered through `server-<name>.mjs`, written beside the
//!   bundle as the builder writes it (`SERVER_REACT_ROUTER`, `SERVER_REMIX`): React Router's takes a
//!   Web `Request` (`useWebApi`), Remix's a Node `(req, res)`. Its files are found as forkrail finds
//!   `api/` functions' (nodefns.rs): what esbuild sees the entry import from the project, and each
//!   npm package it imports, whole, with what that depends on. Vercel's nft traces single files;
//!   a whole package also has the files a package reads at run time. They stay where they are and
//!   the function lists them in `filePathMap`, as `vercel build` leaves them.
//! - **Names:** every route's path (`index`, `items/:id`, `(:lang)/about`, `docs/*`) is a name of
//!   its bundle's function, and for React Router `<path>.data` too (`_root.data` for the index),
//!   unless a prerendered file has that name. The first name of a bundle is its folder; the rest
//!   are links to it.
//! - **Routes**, in the builder's order: the skew-protection cookie (React Router, when the build
//!   has `VERCEL_SKEW_PROTECTION_ENABLED=1` and a deployment id), a rewrite to each prerendered
//!   page, the filesystem, a regex route for each path with parameters (path-to-regexp 6.1.0's,
//!   `.data` first), a catch-all to the index function (React Router) or `/` (Remix), and the
//!   year-long immutable `cache-control` on the assets folder.
//! - **Static files:** `<buildDirectory>/client`, at the site's root.
//!
//! Refused by name: Remix's classic compiler (`remix.config.js`, Vercel's other path, which swaps
//! in Vercel's fork of `@remix-run/dev`), and routes on the Edge runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::build_output;
use crate::buildlog::{self, BuildLog};
use crate::framework::{Framework, Output};

/// Which of the two frameworks: Vercel's builder serves both, with a few differences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ReactRouter,
    Remix,
}

/// Vercel's slugs for them, and their names here (`Framework::name`).
pub const SLUGS: [(&str, &str); 2] = [("react-router", "React Router"), ("remix", "Remix")];

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::ReactRouter => "React Router",
            Kind::Remix => "Remix",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Kind::ReactRouter => "react-router",
            Kind::Remix => "remix",
        }
    }

    /// The framework a build records by this name.
    pub fn of(name: &str) -> Option<Kind> {
        match name {
            "React Router" => Some(Kind::ReactRouter),
            "Remix" => Some(Kind::Remix),
            _ => None,
        }
    }

    fn from_slug(slug: &str) -> Option<Kind> {
        match slug {
            "react-router" => Some(Kind::ReactRouter),
            "remix" => Some(Kind::Remix),
            _ => None,
        }
    }

    /// The package whose version the functions are recorded with (the builder's
    /// `primaryPackageName`).
    fn package(self) -> &'static str {
        match self {
            Kind::ReactRouter => "react-router",
            Kind::Remix => "@remix-run/dev",
        }
    }

    /// What the preset writes for the builder.
    fn result_file(self) -> &'static str {
        match self {
            Kind::ReactRouter => ".vercel/react-router-build-result.json",
            Kind::Remix => ".vercel/remix-build-result.json",
        }
    }

    /// The build when the project names none.
    fn command(self) -> &'static str {
        match self {
            Kind::ReactRouter => "react-router build",
            Kind::Remix => "remix build",
        }
    }

    fn preset(self) -> &'static str {
        match self {
            Kind::ReactRouter => "@vercel/react-router",
            Kind::Remix => "@vercel/remix",
        }
    }
}

/// The framework of a project, as Vercel's detection finds it (`@vercel/frameworks`, Remix before
/// React Router): Remix by the `@remix-run/dev` package or a `remix.config.js`/`.mjs`, React
/// Router by a `vite.config.js`/`.ts` that imports `@react-router/dev/vite` or a
/// `react-router.config.js`/`.ts`. `slug` is `vercel.json`'s `framework`, which picks one.
pub fn detect(dir: &Path, slug: Option<&str>, has: &dyn Fn(&str) -> bool) -> Option<Kind> {
    if let Some(slug) = slug {
        return Kind::from_slug(slug);
    }
    if has("@remix-run/dev") || ["remix.config.js", "remix.config.mjs"].iter().any(|f| dir.join(f).is_file()) {
        return Some(Kind::Remix);
    }
    let vite = ["vite.config.js", "vite.config.ts"].iter().any(|f| std::fs::read(dir.join(f)).is_ok_and(|b| String::from_utf8_lossy(&b).contains("@react-router/dev/vite")));
    if vite || ["react-router.config.js", "react-router.config.ts"].iter().any(|f| dir.join(f).is_file()) {
        return Some(Kind::ReactRouter);
    }
    None
}

/// The framework's build, as Vercel's builder runs it. `overrides` is `vercel.json`, whose
/// `buildCommand` and `installCommand` win; its `outputDirectory` isn't read (nor is it on Vercel).
pub fn framework(dir: &Path, kind: Kind, package: Option<&Value>, overrides: Option<&Value>) -> Result<Framework> {
    if package.is_none() {
        bail!("{} needs a package.json", kind.name());
    }
    if !is_vite(dir, package) {
        match kind {
            Kind::Remix => bail!(
                "Remix's classic compiler (remix.config.js) isn't supported: forkrail deploys Remix 2 with Vite (`vitePlugin` from @remix-run/dev in vite.config), which Vercel also recommends"
            ),
            // Vercel takes such a project for a classic Remix one, and its build fails there.
            Kind::ReactRouter => bail!("React Router needs a vite.config (with `reactRouter()` from @react-router/dev/vite): it builds with Vite"),
        }
    }
    let script = |key: &str| overrides.and_then(|o| o.get(key)).and_then(Value::as_str).map(str::to_owned);
    let scripts = package.and_then(|p| p.get("scripts"));
    let has_script = |name: &str| scripts.and_then(|s| s.get(name)).is_some_and(Value::is_string);
    let manager = crate::framework::package_manager_name(dir);
    let build = match script("buildCommand") {
        Some(command) => command,
        None if has_script("vercel-build") => format!("{manager} run vercel-build"),
        None if has_script("build") => format!("{manager} run build"),
        None => kind.command().to_owned(),
    };
    Ok(Framework {
        name: kind.name(),
        // Every package in a folder of its own (pnpm's links and Yarn's Plug'n'Play flattened), so
        // the files the functions need can be copied as Node finds them.
        install: script("installCommand").unwrap_or_else(|| crate::framework::flat_install_command(dir)),
        build: Some(build),
        output: build_output::DIR.to_owned(),
        entry: "",
        public: "static",
        build_env: &[],
        install_env: &[],
        // Vercel's presets for them name no prefix, and its builder caches only `node_modules`.
        env_prefix: "",
        cache: &[],
        find: Output::BuildOutput,
        hashed: &[],
        routes: "",
        unset: &[],
    })
}

/// Vercel's `isVite`: a `vite.config` and no `remix.config`, or a `vite:build` build script, or a
/// Vite config that imports `@remix-run/dev`.
fn is_vite(dir: &Path, package: Option<&Value>) -> bool {
    let Some(vite) = ["vite.config.js", "vite.config.ts", "vite.config.mjs", "vite.config.mts"].iter().map(|f| dir.join(f)).find(|p| p.is_file()) else {
        return false;
    };
    if !["remix.config.js", "remix.config.cjs", "remix.config.mjs"].iter().any(|f| dir.join(f).is_file()) {
        return true;
    }
    if package.and_then(|p| p.pointer("/scripts/build")).and_then(Value::as_str).is_some_and(|s| s.split(|c: char| !(c.is_alphanumeric() || c == ':' || c == '_')).any(|w| w == "vite:build")) {
        return true;
    }
    std::fs::read_to_string(vite).is_ok_and(|t| t.contains("'@remix-run/dev'") || t.contains("\"@remix-run/dev\""))
}

/// Before the build: an earlier build's result file isn't this one's (Vercel's builder would read
/// it; its builds start from a clean checkout).
pub fn before_build(dir: &Path, kind: Kind) -> Result<()> {
    let file = dir.join(kind.result_file());
    match std::fs::remove_file(&file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("removing the old {}", file.display())),
        _ => Ok(()),
    }
}

// ---- The preset's result file ----

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildResult {
    build_manifest: Manifest,
    #[serde(default)]
    react_router_config: Option<FrameworkConfig>,
    #[serde(default)]
    remix_config: Option<FrameworkConfig>,
    #[serde(default)]
    vite_config: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FrameworkConfig {
    build_directory: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    #[serde(default)]
    routes: BTreeMap<String, ManifestRoute>,
    #[serde(default)]
    server_bundles: BTreeMap<String, Bundle>,
    /// In the file's order: the first route of a bundle names its function's folder.
    #[serde(default)]
    route_id_to_server_bundle_id: Ordered<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestRoute {
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    index: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Bundle {
    file: String,
    #[serde(default)]
    config: serde_json::Map<String, Value>,
}

/// A JSON object's entries in the order the file has them.
#[derive(Debug)]
struct Ordered<V>(Vec<(String, V)>);

impl<V> Default for Ordered<V> {
    fn default() -> Self {
        Ordered(Vec::new())
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for Ordered<V> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visit<V>(std::marker::PhantomData<V>);
        impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for Visit<V> {
            type Value = Ordered<V>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry()? {
                    out.push((k, v));
                }
                Ok(Ordered(out))
            }
        }
        d.deserialize_map(Visit(std::marker::PhantomData))
    }
}

/// The result file, or the builder's guess without the preset: the index route in one bundle at
/// `build/server/index.js` (none when that isn't there).
fn read_result(dir: &Path, kind: Kind, log: &dyn BuildLog) -> Result<BuildResult> {
    let file = dir.join(kind.result_file());
    match std::fs::read_to_string(&file) {
        Ok(text) => return serde_json::from_str(&text).with_context(|| format!("{} isn't the build result {}'s preset writes", file.display(), kind.preset())),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("reading {}", file.display())),
        Err(_) => {}
    }
    if !dir.join("build").is_dir() {
        bail!(
            "the build left neither {} nor a build/ folder: add `vercelPreset()` from {} to the {} config, as Vercel's docs say",
            kind.result_file(),
            kind.preset(),
            if kind == Kind::ReactRouter { "react-router.config" } else { "Vite" }
        );
    }
    log.info(&format!("warning: the `vercelPreset()` preset (from {}) wasn't found: one function serves every route, as on Vercel", kind.preset()));
    let route = |path: Option<&str>, parent: Option<&str>, index: bool| ManifestRoute { parent_id: parent.map(str::to_owned), path: path.map(str::to_owned), index };
    let mut manifest = Manifest {
        routes: BTreeMap::from([("root".to_owned(), route(Some(""), None, false)), ("routes/_index".to_owned(), route(None, Some("root"), true))]),
        ..Manifest::default()
    };
    if dir.join("build/server/index.js").is_file() {
        manifest.route_id_to_server_bundle_id = Ordered(vec![("routes/_index".to_owned(), String::new())]);
        manifest.server_bundles.insert(String::new(), Bundle { file: "build/server/index.js".to_owned(), config: serde_json::Map::new() });
    }
    let config = FrameworkConfig { build_directory: "build".to_owned() };
    Ok(match kind {
        Kind::ReactRouter => BuildResult { build_manifest: manifest, react_router_config: Some(config), ..BuildResult::default() },
        Kind::Remix => BuildResult { build_manifest: manifest, remix_config: Some(config), ..BuildResult::default() },
    })
}

// ---- Paths and routes, as the builder makes them ----

/// Vercel's `getPathFromRoute`: the function's name for a route (`index` for the root, an index
/// route or `/`; the route's and its parents' paths joined, `x?` as `(x)`, `:p?` as `(:p)`), and
/// the pattern its regex route is made from (`:p?` kept, a trailing `*` as `/:params*`).
fn path_of(id: &str, routes: &BTreeMap<String, ManifestRoute>) -> (String, String) {
    let Some(route) = routes.get(id) else { return (String::new(), String::new()) };
    let mut current = route.clone();
    if id == "root" || (current.path.as_deref().is_none_or(str::is_empty) && current.index) || current.path.as_deref() == Some("/") {
        current.path = Some("index".to_owned());
    }
    let mut parts: Vec<String> = Vec::new();
    let mut re_parts: Vec<String> = Vec::new();
    let mut at = Some(current);
    // A route's parents, up to the root (a cycle can't make it loop for ever).
    for _ in 0..=routes.len() {
        let Some(r) = at.take() else { break };
        if let Some(path) = r.path.as_deref().filter(|p| !p.is_empty()) {
            for part in path.split('/').rev() {
                match part.strip_suffix('?') {
                    Some(name) if part.starts_with(':') => {
                        parts.push(format!("({name})"));
                        re_parts.push(part.to_owned());
                    }
                    Some(name) => {
                        parts.push(format!("({name})"));
                        re_parts.push(format!("({name})?"));
                    }
                    None => {
                        parts.push(part.to_owned());
                        re_parts.push(part.to_owned());
                    }
                }
            }
        }
        at = r.parent_id.as_deref().and_then(|p| routes.get(p)).cloned();
    }
    if parts.len() > 1 && parts[0] == "index" {
        parts.remove(0);
        re_parts.remove(0);
    }
    parts.reverse();
    re_parts.reverse();
    let path = parts.join("/");
    let re = re_parts.join("/");
    let re_path = if re == "*" {
        SPLAT_PATH.to_owned()
    } else {
        match re.strip_suffix("/*") {
            Some(head) => format!("/{head}{SPLAT_PATH}"),
            None => format!("/{re}"),
        }
    };
    (path, re_path)
}

const SPLAT_PATH: &str = "/:params*";

/// The source of path-to-regexp 6.1.0's `pathToRegexp(path)` (default options: `/#?` delimiters,
/// `./` prefixes, not strict, anchored at both ends), as the builder's routes have it; `None` when
/// the path has no parameter (the filesystem finds those).
fn path_regex(path: &str) -> Result<Option<String>> {
    let tokens = parse_path(path)?;
    if !tokens.iter().any(|t| matches!(t, Token::Key { .. })) {
        return Ok(None);
    }
    let delimiter = "[\\/#\\?]";
    let mut route = String::from("^");
    for token in &tokens {
        match token {
            Token::Text(text) => route.push_str(&escape(text)),
            Token::Key { prefix, suffix, pattern, modifier } => {
                let (prefix, suffix) = (escape(prefix), escape(suffix));
                if prefix.is_empty() && suffix.is_empty() {
                    route.push_str(&format!("({pattern}){modifier}"));
                } else if modifier == "+" || modifier == "*" {
                    let m = if modifier == "*" { "?" } else { "" };
                    route.push_str(&format!("(?:{prefix}((?:{pattern})(?:{suffix}{prefix}(?:{pattern}))*){suffix}){m}"));
                } else {
                    route.push_str(&format!("(?:{prefix}({pattern}){suffix}){modifier}"));
                }
            }
            Token::Group { prefix, suffix, modifier } => route.push_str(&format!("(?:{}{}){modifier}", escape(prefix), escape(suffix))),
        }
    }
    route.push_str(&format!("{delimiter}?$"));
    Ok(Some(route))
}

#[derive(Debug, PartialEq)]
enum Token {
    Text(String),
    /// A parameter, named or a `(pattern)`.
    Key { prefix: String, suffix: String, pattern: String, modifier: String },
    /// A `{...}` group without a parameter.
    Group { prefix: String, suffix: String, modifier: String },
}

#[derive(Debug, Clone, PartialEq)]
enum Lexed {
    Modifier(char),
    Escaped(char),
    Open,
    Close,
    Name(String),
    Pattern(String),
    Char(char),
    End,
}

/// path-to-regexp 6.1.0's `lexer`.
fn lex(path: &str) -> Result<Vec<Lexed>> {
    let chars: Vec<char> = path.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' | '+' | '?' => {
                out.push(Lexed::Modifier(c));
                i += 1;
            }
            '\\' => {
                let Some(&next) = chars.get(i + 1) else { bail!("a path ends in `\\`: {path}") };
                out.push(Lexed::Escaped(next));
                i += 2;
            }
            '{' => {
                out.push(Lexed::Open);
                i += 1;
            }
            '}' => {
                out.push(Lexed::Close);
                i += 1;
            }
            ':' => {
                let mut j = i + 1;
                let mut name = String::new();
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    name.push(chars[j]);
                    j += 1;
                }
                if name.is_empty() {
                    bail!("missing parameter name at {i} in {path}");
                }
                out.push(Lexed::Name(name));
                i = j;
            }
            '(' => {
                let mut count = 1;
                let mut pattern = String::new();
                let mut j = i + 1;
                if chars.get(j) == Some(&'?') {
                    bail!("pattern cannot start with \"?\" at {j} in {path}");
                }
                while j < chars.len() {
                    if chars[j] == '\\' {
                        pattern.push(chars[j]);
                        if let Some(&n) = chars.get(j + 1) {
                            pattern.push(n);
                        }
                        j += 2;
                        continue;
                    }
                    if chars[j] == ')' {
                        count -= 1;
                        if count == 0 {
                            j += 1;
                            break;
                        }
                    } else if chars[j] == '(' {
                        count += 1;
                        if chars.get(j + 1) != Some(&'?') {
                            bail!("capturing groups are not allowed at {j} in {path}");
                        }
                    }
                    pattern.push(chars[j]);
                    j += 1;
                }
                if count != 0 {
                    bail!("unbalanced pattern at {i} in {path}");
                }
                if pattern.is_empty() {
                    bail!("missing pattern at {i} in {path}");
                }
                out.push(Lexed::Pattern(pattern));
                i = j;
            }
            _ => {
                out.push(Lexed::Char(c));
                i += 1;
            }
        }
    }
    out.push(Lexed::End);
    Ok(out)
}

/// path-to-regexp 6.1.0's `parse`.
fn parse_path(path: &str) -> Result<Vec<Token>> {
    const DEFAULT_PATTERN: &str = "[^\\/#\\?]+?";
    let lexed = lex(path)?;
    let mut i = 0;
    let mut out = Vec::new();
    let mut text = String::new();
    let take = |i: &mut usize, want: fn(&Lexed) -> Option<String>| -> Option<String> {
        let got = lexed.get(*i).and_then(want);
        if got.is_some() {
            *i += 1;
        }
        got
    };
    let char_of = |t: &Lexed| if let Lexed::Char(c) = t { Some(c.to_string()) } else { None };
    let escaped_of = |t: &Lexed| if let Lexed::Escaped(c) = t { Some(c.to_string()) } else { None };
    let name_of = |t: &Lexed| if let Lexed::Name(n) = t { Some(n.clone()) } else { None };
    let pattern_of = |t: &Lexed| if let Lexed::Pattern(p) = t { Some(p.clone()) } else { None };
    let modifier_of = |t: &Lexed| if let Lexed::Modifier(m) = t { Some(m.to_string()) } else { None };
    let open_of = |t: &Lexed| (*t == Lexed::Open).then(String::new);
    let close_of = |t: &Lexed| (*t == Lexed::Close).then(String::new);
    let end_of = |t: &Lexed| (*t == Lexed::End).then(String::new);
    let consume_text = |i: &mut usize| {
        let mut s = String::new();
        while let Some(v) = take(i, char_of).or_else(|| take(i, escaped_of)) {
            s.push_str(&v);
        }
        s
    };
    while i < lexed.len() {
        let ch = take(&mut i, char_of);
        let name = take(&mut i, name_of);
        let pattern = take(&mut i, pattern_of);
        if name.is_some() || pattern.is_some() {
            // Only `.` and `/` before a parameter are its prefix (`indexOf` finds "" too).
            let mut prefix = ch.unwrap_or_default();
            if !"./".contains(prefix.as_str()) {
                text.push_str(&prefix);
                prefix = String::new();
            }
            if !text.is_empty() {
                out.push(Token::Text(std::mem::take(&mut text)));
            }
            out.push(Token::Key { prefix, suffix: String::new(), pattern: pattern.unwrap_or_else(|| DEFAULT_PATTERN.to_owned()), modifier: take(&mut i, modifier_of).unwrap_or_default() });
            continue;
        }
        if let Some(value) = ch.or_else(|| take(&mut i, escaped_of)) {
            text.push_str(&value);
            continue;
        }
        if !text.is_empty() {
            out.push(Token::Text(std::mem::take(&mut text)));
        }
        if take(&mut i, open_of).is_some() {
            let prefix = consume_text(&mut i);
            let name = take(&mut i, name_of).unwrap_or_default();
            let pattern = take(&mut i, pattern_of).unwrap_or_default();
            let suffix = consume_text(&mut i);
            if take(&mut i, close_of).is_none() {
                bail!("unexpected {:?} in {path}, expected `}}`", lexed.get(i));
            }
            let modifier = take(&mut i, modifier_of).unwrap_or_default();
            if name.is_empty() && pattern.is_empty() {
                out.push(Token::Group { prefix, suffix, modifier });
            } else {
                let pattern = if !name.is_empty() && pattern.is_empty() { DEFAULT_PATTERN.to_owned() } else { pattern };
                out.push(Token::Key { prefix, suffix, pattern, modifier });
            }
            continue;
        }
        if take(&mut i, end_of).is_none() {
            bail!("unexpected {:?} in {path}", lexed.get(i));
        }
    }
    Ok(out)
}

/// path-to-regexp's `escapeString`.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if ".+*?=^!:${}()[]|/\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The skew-protection route React Router's functions get from the builder when the build has
/// both variables: documents set the `__vdpl` cookie the routers pin a visitor's later requests by.
fn skew_route(env: &BTreeMap<String, String>) -> Option<Value> {
    let id = env.get("VERCEL_DEPLOYMENT_ID").filter(|id| !id.is_empty())?;
    if env.get(crate::skew::ENV).map(String::as_str) != Some("1") {
        return None;
    }
    Some(json!({
        "src": "/(.*)",
        "has": [{"type": "header", "key": "Sec-Fetch-Dest", "value": "document"}],
        "headers": {"Set-Cookie": format!("__vdpl={id}; Path=/; SameSite=Strict; Secure; HttpOnly")},
        "continue": true
    }))
}

/// What the builder makes of a result file: each output name and the bundle it runs (in the order
/// they're made), and the routes.
#[derive(Debug, PartialEq)]
struct Plan {
    names: Vec<(String, String)>,
    routes: Vec<Value>,
}

fn plan(kind: Kind, manifest: &Manifest, static_files: &BTreeSet<String>, assets_dir: &str, env: &BTreeMap<String, String>) -> Result<Plan> {
    let react_router = kind == Kind::ReactRouter;
    let mut names: Vec<(String, String)> = Vec::new();
    let mut rewrites = Vec::new();
    let mut regex_routes = Vec::new();
    let mut index_function = false;
    let set = |names: &mut Vec<(String, String)>, path: String, bundle: &str| match names.iter_mut().find(|(p, _)| *p == path) {
        Some(entry) => entry.1 = bundle.to_owned(),
        None => names.push((path, bundle.to_owned())),
    };
    for (id, bundle) in &manifest.route_id_to_server_bundle_id.0 {
        let (path, re_path) = path_of(id, &manifest.routes);
        if path.is_empty() {
            continue;
        }
        if react_router {
            let candidates = if path == "index" { vec!["index.html".to_owned()] } else { vec![format!("{path}.html"), format!("{path}/index.html")] };
            if let Some(html) = candidates.into_iter().find(|c| static_files.contains(c)) {
                rewrites.push(json!({"src": if path == "index" { "^/$".to_owned() } else { format!("^/{path}/?$") }, "dest": format!("/{html}")}));
                // The index keeps its function: the catch-all needs it.
                if path != "index" {
                    continue;
                }
            }
        }
        if !manifest.server_bundles.contains_key(bundle) {
            bail!("could not determine server bundle for {id:?}");
        }
        set(&mut names, path.clone(), bundle);
        if react_router {
            if path == "index" {
                index_function = true;
            }
            let mut data = vec![format!("{path}.data")];
            if path == "index" {
                data.push("_root.data".to_owned());
            }
            for name in data {
                if !static_files.contains(&name) {
                    set(&mut names, name, bundle);
                }
            }
        }
        if let Some(src) = path_regex(&re_path)? {
            if react_router && let Some(data) = path_regex(&format!("{re_path}.data"))? {
                regex_routes.push(json!({"src": data, "dest": format!("{path}.data")}));
            }
            regex_routes.push(json!({"src": src, "dest": path}));
        }
    }
    let mut routes = Vec::new();
    if react_router && let Some(skew) = skew_route(env) {
        routes.push(skew);
    }
    routes.extend(rewrites);
    routes.push(json!({"handle": "filesystem"}));
    routes.extend(regex_routes);
    routes.push(json!({"src": "/(.*)", "dest": if react_router && index_function { "index" } else { "/" }}));
    routes.push(json!({"handle": "hit"}));
    routes.push(json!({"src": format!("^/{assets_dir}/(.*)$"), "headers": {"cache-control": "public, max-age=31536000, immutable"}, "continue": true}));
    Ok(Plan { names, routes })
}

// ---- The server entries (ported from the builder's `defaults/`, Apache-2.0) ----

/// `defaults/server-react-router.mjs`: a Web handler, for Node and Edge alike. A custom server
/// entry's default export is a handler already; otherwise the build is React Router's, and React
/// Router 7.9+ with `future.v8_middleware` wants a context made for each request.
const SERVER_REACT_ROUTER: &str = "\
// Written by forkrail beside React Router's server build, as Vercel's builder writes its own
// (@vercel/remix-builder, defaults/server-react-router.mjs): the function's Web handler.
import * as RR from 'react-router';
import * as built from '__BUILD__';
const build = built.default || built;

export default typeof build === 'function'
  ? build
  : (() => {
      const handler = RR.createRequestHandler(build);
      return (request) => (build.future?.v8_middleware ? handler(request, new RR.RouterContextProvider()) : handler(request));
    })();
";

/// `defaults/server-node.mjs`: Remix's Node `(req, res)` handler. The request's URL is the
/// visitor's (`x-forwarded-host` or `host`, `x-forwarded-proto` or https), its body streamed in;
/// the response streamed out. Native fetch with single fetch, or when `VERCEL_REMIX_NATIVE_FETCH=1`.
const SERVER_REMIX: &str = "\
// Written by forkrail beside Remix's server build, as Vercel's builder writes its own
// (@vercel/remix-builder, defaults/server-node.mjs): the function's Node handler.
import { createRequestHandler, createReadableStreamFromReadable, writeReadableStreamToWritable, installGlobals } from '@remix-run/node';
import * as build from '__BUILD__';

installGlobals({
  nativeFetch:
    (parseInt(process.versions.node, 10) >= 20 && process.env.VERCEL_REMIX_NATIVE_FETCH === '1') ||
    build.future.unstable_singleFetch ||
    build.future.v3_singleFetch,
});

const handleRequest = createRequestHandler(build.default || build, process.env.NODE_ENV);

function toWebHeaders(nodeHeaders) {
  const headers = new Headers();
  for (const key in nodeHeaders) {
    const value = nodeHeaders[key];
    if (Array.isArray(value)) for (const v of value) headers.append(key, v);
    else headers.append(key, value);
  }
  return headers;
}

function toRequest(req, res) {
  const host = req.headers['x-forwarded-host'] || req.headers.host;
  const protocol = req.headers['x-forwarded-proto'] || 'https';
  const url = new URL(req.url, `${protocol}://${host}`);
  const controller = new AbortController();
  res.on('close', () => controller.abort());
  const init = { method: req.method, headers: toWebHeaders(req.headers), signal: controller.signal };
  if (req.method !== 'GET' && req.method !== 'HEAD') {
    init.body = createReadableStreamFromReadable(req);
    init.duplex = 'half';
  }
  return new Request(url.href, init);
}

export default async (req, res) => {
  const response = await handleRequest(toRequest(req, res));
  res.statusMessage = response.statusText;
  res.writeHead(response.status, response.statusText, response.headers.raw?.() || [...response.headers].flat());
  if (response.body) await writeReadableStreamToWritable(response.body, res);
  else res.end();
};
";

// ---- Packaging ----

/// After the build: the Build Output Vercel's builder and CLI would make of it, written to
/// `.vercel/output` for build_output.rs. A build that wrote one itself keeps it.
pub fn package(dir: &Path, kind: Kind, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
    package_with(dir, kind, env, log, &trace)
}

/// What finds a bundle's files: each bundle's id → its files, from the handlers by bundle id.
type Tracer<'a> = &'a dyn Fn(&Path, &BTreeMap<String, String>, &BTreeMap<String, String>, &dyn BuildLog) -> Result<BTreeMap<String, BTreeSet<String>>>;

fn package_with(dir: &Path, kind: Kind, env: &BTreeMap<String, String>, log: &dyn BuildLog, tracer: Tracer) -> Result<()> {
    let out = dir.join(build_output::DIR);
    let config = out.join("config.json");
    if config.is_file() {
        let version = std::fs::read_to_string(&config).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.get("version").and_then(Value::as_u64));
        if version != Some(3) {
            bail!("the \"version\" property in \".vercel/output/config.json\" must be 3 (received {})", version.map_or("none".to_owned(), |v| v.to_string()));
        }
        log.info(&format!("the build wrote its own Build Output ({}): deploying that", build_output::DIR));
        return Ok(());
    }
    let result = read_result(dir, kind, log)?;
    let project = dir.canonicalize().with_context(|| format!("resolving {}", dir.display()))?;
    let build_dir = result.react_router_config.as_ref().or(result.remix_config.as_ref()).map_or("build", |c| c.build_directory.as_str());
    let build_dir = within(&project, build_dir).with_context(|| format!("{}'s buildDirectory", kind.result_file()))?;
    let client = build_dir.join("client");
    let assets_dir = result.vite_config.as_ref().and_then(|v| v.pointer("/build/assetsDir")).and_then(Value::as_str).filter(|d| !d.is_empty()).unwrap_or("assets").to_owned();
    let manifest = &result.build_manifest;
    for (id, bundle) in &manifest.server_bundles {
        let runtime = bundle.config.get("runtime").and_then(Value::as_str).unwrap_or("nodejs");
        if runtime == "edge" || runtime == "experimental-edge" {
            bail!(
                "{}'s server bundle {id:?} runs on the Edge runtime (a route's `export const config = {{ runtime: \"edge\" }}`), which forkrail doesn't deploy for {} yet: remove it to run the route on Node",
                kind.name(),
                kind.name()
            );
        }
    }
    let statics = list_files(&client)?;
    let keys: BTreeSet<String> = statics.keys().cloned().collect();
    let plan = plan(kind, manifest, &keys, &assets_dir, env)?;

    // Each bundle that some name runs: its entry, written beside it, and the files it needs.
    let version = package_version(dir, kind.package());
    let runtime = node_runtime(env);
    let mut used: Vec<&str> = Vec::new();
    for (_, bundle) in &plan.names {
        if !used.contains(&bundle.as_str()) {
            used.push(bundle);
        }
    }
    let mut handlers: BTreeMap<String, String> = BTreeMap::new();
    for id in &used {
        let bundle = &manifest.server_bundles[*id];
        let file = within(&project, &bundle.file).with_context(|| format!("{}'s server bundle {id:?}", kind.result_file()))?;
        if !file.is_file() {
            bail!("{} names the server bundle {}, which the build didn't leave", kind.result_file(), bundle.file);
        }
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let handler = file.with_file_name(format!("server-{stem}.mjs"));
        let template = match kind {
            Kind::ReactRouter => SERVER_REACT_ROUTER,
            Kind::Remix => SERVER_REMIX,
        };
        std::fs::write(&handler, template.replace("__BUILD__", &format!("./{stem}.js"))).with_context(|| format!("writing {}", handler.display()))?;
        handlers.insert((*id).to_owned(), relative(&project, &handler)?);
    }
    let traced = tracer(dir, &handlers, env, log)?;

    // `.vercel/output`, as `vercel build` writes it without `--standalone`.
    let functions = out.join("functions");
    std::fs::create_dir_all(&functions).with_context(|| format!("creating {}", functions.display()))?;
    let mut real: BTreeMap<&str, String> = BTreeMap::new();
    for (name, id) in &plan.names {
        let folder = functions.join(format!("{name}.func"));
        if let Some(parent) = folder.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        if let Some(first) = real.get(id.as_str()) {
            let target = pathdiff(&functions.join(format!("{first}.func")), folder.parent().unwrap_or(&functions));
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &folder).with_context(|| format!("linking {} to {}", folder.display(), target.display()))?;
            // Windows makes links only for administrators (or in developer mode): a copy of the
            // folder, which holds only its .vc-config.json, does the same.
            #[cfg(not(unix))]
            crate::build::copy_tree(&functions.join(format!("{first}.func")), &folder).with_context(|| format!("copying {} to {}", target.display(), folder.display()))?;
            continue;
        }
        let bundle = &manifest.server_bundles[id];
        let mut vc = json!({
            "operationType": "SSR",
            "handler": handlers[id],
            "runtime": runtime,
            "architecture": if std::env::consts::ARCH == "aarch64" { "arm64" } else { "x86_64" },
            "environment": {},
            "supportsResponseStreaming": true,
            "framework": {"slug": kind.slug(), "version": version},
            "shouldDisableAutomaticFetchInstrumentation": env.get("VERCEL_TRACING_DISABLE_AUTOMATIC_FETCH_INSTRUMENTATION").map(String::as_str) == Some("1"),
            "launcherType": "Nodejs",
            "shouldAddHelpers": false,
            "shouldAddSourcemapSupport": false,
            "filePathMap": traced[id].iter().map(|f| (f.clone(), Value::String(f.clone()))).collect::<serde_json::Map<_, _>>(),
        });
        for key in ["regions", "memory", "maxDuration"] {
            if let Some(value) = bundle.config.get(key) {
                vc[key] = value.clone();
            }
        }
        if kind == Kind::ReactRouter {
            vc["useWebApi"] = Value::Bool(true);
        }
        std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
        std::fs::write(folder.join(".vc-config.json"), serde_json::to_vec_pretty(&vc)?).with_context(|| format!("writing {}", folder.display()))?;
        real.insert(id, name.clone());
    }
    // The static files, but a name a function has (the builder's output map keeps the function).
    let static_dir = out.join("static");
    let names: BTreeSet<&str> = plan.names.iter().map(|(n, _)| n.as_str()).collect();
    for (rel, from) in &statics {
        if names.contains(rel.as_str()) {
            continue;
        }
        let to = static_dir.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        if std::fs::hard_link(from, &to).is_err() {
            std::fs::copy(from, &to).with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
    }
    let config = json!({
        "version": 3,
        "routes": plan.routes,
        // The builder names both frameworks `remix` here; each function says which it is.
        "framework": {"slug": "remix", "version": version},
    });
    std::fs::write(out.join("config.json"), serde_json::to_vec_pretty(&config)?).context("writing .vercel/output/config.json")?;
    log.info(&format!(
        "{}: {} server bundle{}, {} names, {} static files",
        kind.name(),
        real.len(),
        if real.len() == 1 { "" } else { "s" },
        plan.names.len(),
        statics.len()
    ));
    Ok(())
}

/// The files each bundle's entry needs, relative to the project: `package.json` (for its
/// `"type"`), what esbuild reads of the project's own following its imports (the bundle and its
/// chunks), and every file of the npm packages it imports and what those depend on
/// (nodefns.rs's `package_dirs`).
fn trace(dir: &Path, handlers: &BTreeMap<String, String>, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if handlers.is_empty() {
        return Ok(out);
    }
    let work = crate::package::TempDir::new("trace")?;
    let meta = work.path().join("meta.json");
    let mut command = crate::nodeapp::esbuild(dir);
    command
        .args(handlers.values())
        .args(["--bundle", "--platform=node", "--packages=external", "--format=esm", "--target=esnext", "--log-level=error", "--outbase=."])
        .arg(format!("--outdir={}", work.path().join("out").display()))
        .arg(format!("--metafile={}", meta.display()))
        .current_dir(dir);
    buildlog::run(&mut command, "esbuild", env, log).context("reading what the server bundles import")?;
    let text = std::fs::read_to_string(&meta).context("reading esbuild's metafile")?;
    let meta: Value = serde_json::from_str(&text).context("esbuild's metafile isn't JSON")?;
    let inputs = meta.get("inputs").and_then(Value::as_object).cloned().unwrap_or_default();
    let package_json = dir.join("package.json").is_file();
    for output in meta.get("outputs").and_then(Value::as_object).into_iter().flat_map(|o| o.values()) {
        let Some(entry) = output.get("entryPoint").and_then(Value::as_str) else { continue };
        let Some((id, _)) = handlers.iter().find(|(_, h)| h.as_str() == entry) else { continue };
        let mut files = BTreeSet::new();
        let mut packages = BTreeSet::new();
        for input in output.get("inputs").and_then(Value::as_object).into_iter().flat_map(|i| i.keys()) {
            if input.starts_with("../") || input.contains(':') {
                bail!("the server bundle {} imports {input}, outside the project", handlers[id]);
            }
            files.insert(input.clone());
            for import in inputs.get(input).and_then(|i| i.get("imports")).and_then(Value::as_array).into_iter().flatten() {
                if import.get("external").and_then(Value::as_bool) == Some(true)
                    && let Some(name) = import.get("path").and_then(Value::as_str).and_then(crate::nodefns::package_name)
                {
                    packages.insert(name);
                }
            }
        }
        if package_json {
            files.insert("package.json".to_owned());
        }
        for package in crate::nodefns::package_dirs(dir, &packages) {
            for file in package_files(dir, &package)? {
                files.insert(file);
            }
        }
        out.insert(id.clone(), files);
    }
    for id in handlers.keys() {
        if !out.contains_key(id) {
            bail!("esbuild read nothing for the server bundle {}", handlers[id]);
        }
    }
    Ok(out)
}

/// Every file of the package folder `package` (relative to `dir`, `node_modules/...`), but the
/// packages nested in it (their own `node_modules`, each traced on its own when needed).
fn package_files(dir: &Path, package: &str) -> Result<Vec<String>> {
    let root = dir.join(package);
    let mut out = Vec::new();
    let walk = ignore::WalkBuilder::new(&root)
        .standard_filters(false)
        .follow_links(true)
        .filter_entry(|e| !(e.depth() > 0 && e.file_name() == "node_modules" && e.file_type().is_some_and(|t| t.is_dir())))
        .build();
    for entry in walk {
        let entry = entry.with_context(|| format!("listing {package}"))?;
        if !entry.path().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(&root).expect("walked from it").to_string_lossy().replace('\\', "/");
        out.push(format!("{package}/{rel}"));
    }
    Ok(out)
}

/// The files under `dir` by their path in it, `/`-separated (dot files too, as the builder's glob
/// takes them).
fn list_files(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in ignore::WalkBuilder::new(dir).standard_filters(false).follow_links(true).build() {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        if !entry.path().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(dir).expect("walked from it").to_string_lossy().replace('\\', "/");
        out.insert(rel, entry.path().to_path_buf());
    }
    Ok(out)
}

/// `path` (absolute, or relative to the project), which must be inside the project.
fn within(project: &Path, path: &str) -> Result<PathBuf> {
    let joined = project.join(path);
    let resolved = match joined.canonicalize() {
        Ok(p) => p,
        // Not there yet: its folder must be.
        Err(_) => match (joined.parent().and_then(|p| p.canonicalize().ok()), joined.file_name()) {
            (Some(parent), Some(name)) => parent.join(name),
            _ => bail!("{path} isn't there"),
        },
    };
    if !resolved.starts_with(project) {
        bail!("{path} is outside the project");
    }
    Ok(resolved)
}

fn relative(project: &Path, path: &Path) -> Result<String> {
    Ok(path.strip_prefix(project).with_context(|| format!("{} is outside the project", path.display()))?.to_string_lossy().replace('\\', "/"))
}

/// `to` relative to the folder `from` (both under the same root), as a link names its target.
fn pathdiff(to: &Path, from: &Path) -> PathBuf {
    let to: Vec<_> = to.components().collect();
    let from: Vec<_> = from.components().collect();
    let common = to.iter().zip(&from).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for part in &to[common..] {
        out.push(part.as_os_str());
    }
    out
}

/// The installed version of `package`, as the builder records it with each function.
fn package_version(dir: &Path, package: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("node_modules").join(package).join("package.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()?.get("version")?.as_str().map(str::to_owned)
}

/// The Node the build ran with, as Lambda names it (`nodejs24.x`): Vercel writes its project's
/// Node there. Only recorded; the deployment's runtime is forkrail's choice.
fn node_runtime(env: &BTreeMap<String, String>) -> String {
    let mut command = Command::new("node");
    command.args(["-p", "process.versions.node"]);
    if let Some(path) = env.get("PATH") {
        command.env("PATH", path);
    }
    let major = command.output().ok().filter(|o| o.status.success()).and_then(|o| String::from_utf8(o.stdout).ok()).and_then(|v| v.trim().split('.').next().map(str::to_owned));
    format!("nodejs{}.x", major.unwrap_or_else(|| "22".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes(list: &[(&str, Option<&str>, Option<&str>, bool)]) -> BTreeMap<String, ManifestRoute> {
        list.iter().map(|(id, parent, path, index)| ((*id).to_owned(), ManifestRoute { parent_id: parent.map(str::to_owned), path: path.map(str::to_owned), index: *index })).collect()
    }

    /// fw-react-router's routes, as its preset's result file has them.
    fn app_routes() -> BTreeMap<String, ManifestRoute> {
        routes(&[
            ("root", None, Some(""), false),
            ("routes/home", Some("root"), None, true),
            ("routes/ssr", Some("root"), Some("ssr"), false),
            ("routes/item", Some("root"), Some("items/:id"), false),
            ("routes/about", Some("root"), Some(":lang?/about"), false),
            ("routes/docs", Some("root"), Some("docs/*"), false),
            ("routes/limited", Some("root"), None, false),
            ("routes/limited-child", Some("routes/limited"), Some("limited/child"), false),
            ("routes/opt", Some("root"), Some("en?/page"), false),
            ("routes/splat", Some("root"), Some("*"), false),
            ("routes/nested", Some("routes/ssr"), None, true),
        ])
    }

    #[test]
    fn paths_as_the_builder_names_them() {
        let r = app_routes();
        let cases = [
            ("routes/home", "index", "/index"),
            ("root", "index", "/index"),
            ("routes/ssr", "ssr", "/ssr"),
            ("routes/item", "items/:id", "/items/:id"),
            ("routes/about", "(:lang)/about", "/:lang?/about"),
            ("routes/docs", "docs/*", "/docs/:params*"),
            ("routes/limited-child", "limited/child", "/limited/child"),
            ("routes/opt", "(en)/page", "/(en)?/page"),
            ("routes/splat", "*", "/:params*"),
            ("routes/nested", "ssr", "/ssr"),
        ];
        for (id, path, re) in cases {
            assert_eq!(path_of(id, &r), (path.to_owned(), re.to_owned()), "{id}");
        }
    }

    #[test]
    fn regexes_are_path_to_regexp_6_1_0s() {
        // The sources Vercel's builder wrote for fw-react-router (docs/data/c16/fw-react-router).
        let cases = [
            ("/items/:id", Some(r"^\/items(?:\/([^\/#\?]+?))[\/#\?]?$")),
            ("/items/:id.data", Some(r"^\/items(?:\/([^\/#\?]+?))\.data[\/#\?]?$")),
            ("/:lang?/about", Some(r"^(?:\/([^\/#\?]+?))?\/about[\/#\?]?$")),
            ("/:lang?/about.data", Some(r"^(?:\/([^\/#\?]+?))?\/about\.data[\/#\?]?$")),
            ("/docs/:params*", Some(r"^\/docs(?:\/((?:[^\/#\?]+?)(?:\/(?:[^\/#\?]+?))*))?[\/#\?]?$")),
            ("/docs/:params*.data", Some(r"^\/docs(?:\/((?:[^\/#\?]+?)(?:\/(?:[^\/#\?]+?))*))?\.data[\/#\?]?$")),
            ("/ssr", None),
            ("/index", None),
            ("/(en)?/page", Some(r"^(?:\/(en))?\/page[\/#\?]?$")),
            ("/a-b_c/:x", Some(r"^\/a-b_c(?:\/([^\/#\?]+?))[\/#\?]?$")),
        ];
        for (path, want) in cases {
            assert_eq!(path_regex(path).unwrap().as_deref(), want, "{path}");
        }
        assert!(path_regex("/a/:").is_err());
    }

    fn app_manifest() -> Manifest {
        let bundle = |file: &str, config: Value| Bundle { file: file.to_owned(), config: config.as_object().cloned().unwrap_or_default() };
        let mut routes = app_routes();
        routes.insert("routes/static".into(), ManifestRoute { parent_id: Some("root".into()), path: Some("static".into()), index: false });
        routes.insert("routes/heavy".into(), ManifestRoute { parent_id: Some("root".into()), path: Some("heavy".into()), index: false });
        routes.insert("routes/api.hello".into(), ManifestRoute { parent_id: Some("root".into()), path: Some("api/hello".into()), index: false });
        Manifest {
            routes,
            server_bundles: BTreeMap::from([
                ("a".to_owned(), bundle("build/server/a/index.js", json!({"runtime": "nodejs"}))),
                ("h".to_owned(), bundle("build/server/h/index.js", json!({"runtime": "nodejs", "memory": 1024, "maxDuration": 30}))),
                ("l".to_owned(), bundle("build/server/l/index.js", json!({"runtime": "nodejs", "maxDuration": 60}))),
            ]),
            route_id_to_server_bundle_id: Ordered(
                [
                    ("routes/home", "a"),
                    ("routes/ssr", "a"),
                    ("routes/heavy", "h"),
                    ("routes/static", "a"),
                    ("routes/item", "a"),
                    ("routes/about", "a"),
                    ("routes/docs", "a"),
                    ("routes/limited-child", "l"),
                    ("routes/api.hello", "a"),
                ]
                .iter()
                .map(|(r, b)| ((*r).to_owned(), (*b).to_owned()))
                .collect(),
            ),
        }
    }

    #[test]
    fn react_router_plan_is_the_builders() {
        let statics: BTreeSet<String> = ["static/index.html", "static.data", "favicon.ico", "assets/x.js"].iter().map(|s| (*s).to_owned()).collect();
        let plan = plan(Kind::ReactRouter, &app_manifest(), &statics, "assets", &BTreeMap::new()).unwrap();
        let names: Vec<&str> = plan.names.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "index", "index.data", "_root.data", "ssr", "ssr.data", "heavy", "heavy.data", "items/:id", "items/:id.data", "(:lang)/about", "(:lang)/about.data", "docs/*", "docs/*.data",
                "limited/child", "limited/child.data", "api/hello", "api/hello.data"
            ]
        );
        assert_eq!(plan.names.iter().find(|(n, _)| n == "heavy").unwrap().1, "h");
        // The order and shapes of fw-react-router's config.json, without the CLI's own routes.
        let want = json!([
            {"src": "^/static/?$", "dest": "/static/index.html"},
            {"handle": "filesystem"},
            {"src": r"^\/items(?:\/([^\/#\?]+?))\.data[\/#\?]?$", "dest": "items/:id.data"},
            {"src": r"^\/items(?:\/([^\/#\?]+?))[\/#\?]?$", "dest": "items/:id"},
            {"src": r"^(?:\/([^\/#\?]+?))?\/about\.data[\/#\?]?$", "dest": "(:lang)/about.data"},
            {"src": r"^(?:\/([^\/#\?]+?))?\/about[\/#\?]?$", "dest": "(:lang)/about"},
            {"src": r"^\/docs(?:\/((?:[^\/#\?]+?)(?:\/(?:[^\/#\?]+?))*))?\.data[\/#\?]?$", "dest": "docs/*.data"},
            {"src": r"^\/docs(?:\/((?:[^\/#\?]+?)(?:\/(?:[^\/#\?]+?))*))?[\/#\?]?$", "dest": "docs/*"},
            {"src": "/(.*)", "dest": "index"},
            {"handle": "hit"},
            {"src": "^/assets/(.*)$", "headers": {"cache-control": "public, max-age=31536000, immutable"}, "continue": true}
        ]);
        assert_eq!(Value::Array(plan.routes), want);
        // Skew protection: the cookie route first, with both variables only.
        let env = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<BTreeMap<_, _>>();
        let on = plan_of(Kind::ReactRouter, &env(&[("VERCEL_SKEW_PROTECTION_ENABLED", "1"), ("VERCEL_DEPLOYMENT_ID", "dpl_test123")]));
        assert_eq!(
            on.routes[0],
            json!({"src": "/(.*)", "has": [{"type": "header", "key": "Sec-Fetch-Dest", "value": "document"}], "headers": {"Set-Cookie": "__vdpl=dpl_test123; Path=/; SameSite=Strict; Secure; HttpOnly"}, "continue": true})
        );
        assert_eq!(on.routes.len(), 5);
        assert_eq!(plan_of(Kind::ReactRouter, &env(&[("VERCEL_SKEW_PROTECTION_ENABLED", "1")])).routes[0], json!({"handle": "filesystem"}), "no id, no cookie");
        assert_eq!(plan_of(Kind::Remix, &env(&[("VERCEL_SKEW_PROTECTION_ENABLED", "1"), ("VERCEL_DEPLOYMENT_ID", "d")])).routes[0], json!({"handle": "filesystem"}));
    }

    fn plan_of(kind: Kind, env: &BTreeMap<String, String>) -> Plan {
        let mut m = app_manifest();
        m.route_id_to_server_bundle_id.0.retain(|(r, _)| r == "routes/home" || r == "routes/ssr");
        plan(kind, &m, &BTreeSet::new(), "assets", env).unwrap()
    }

    #[test]
    fn remix_plan_is_the_builders() {
        // fw-remix: no `.data` names, no prerenders, the catch-all to `/`.
        let remix = plan(Kind::Remix, &app_manifest(), &BTreeSet::from(["static/index.html".to_owned()]), "assets", &BTreeMap::new()).unwrap();
        let names: Vec<&str> = remix.names.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["index", "ssr", "heavy", "static", "items/:id", "(:lang)/about", "docs/*", "limited/child", "api/hello"]);
        assert_eq!(remix.routes[0], json!({"handle": "filesystem"}));
        assert_eq!(remix.routes[1], json!({"src": r"^\/items(?:\/([^\/#\?]+?))[\/#\?]?$", "dest": "items/:id"}));
        assert_eq!(remix.routes[remix.routes.len() - 3], json!({"src": "/(.*)", "dest": "/"}));
        // A prerendered index keeps its function; a `.data` file shadows its name.
        let statics = BTreeSet::from(["index.html".to_owned(), "ssr.data".to_owned()]);
        let rr = plan_of_with(Kind::ReactRouter, &statics);
        assert_eq!(rr.routes[0], json!({"src": "^/$", "dest": "/index.html"}));
        assert!(rr.names.iter().any(|(n, _)| n == "index") && !rr.names.iter().any(|(n, _)| n == "ssr.data"));
        // SPA mode (no bundles): only the filesystem and the catch-all to `/`.
        let spa = plan(Kind::ReactRouter, &Manifest::default(), &BTreeSet::from(["index.html".to_owned()]), "assets", &BTreeMap::new()).unwrap();
        assert!(spa.names.is_empty());
        assert_eq!(spa.routes[..2], [json!({"handle": "filesystem"}), json!({"src": "/(.*)", "dest": "/"})]);
    }

    fn plan_of_with(kind: Kind, statics: &BTreeSet<String>) -> Plan {
        let mut m = app_manifest();
        m.route_id_to_server_bundle_id.0.retain(|(r, _)| r == "routes/home" || r == "routes/ssr");
        plan(kind, &m, statics, "assets", &BTreeMap::new()).unwrap()
    }

    #[test]
    fn result_files_keep_their_order() {
        let text = r#"{"buildManifest": {"routes": {}, "serverBundles": {"b": {"id": "b", "file": "x.js", "config": {"runtime": "nodejs"}}},
            "routeIdToServerBundleId": {"z": "b", "a": "b", "m": "b"}}, "reactRouterConfig": {"buildDirectory": "/x/build", "future": {}}, "viteConfig": {"build": {"assetsDir": "static"}}}"#;
        let r: BuildResult = serde_json::from_str(text).unwrap();
        let order: Vec<&str> = r.build_manifest.route_id_to_server_bundle_id.0.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(order, ["z", "a", "m"]);
        assert_eq!(r.react_router_config.unwrap().build_directory, "/x/build");
    }

    fn project(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-rr-{}", forkrail_common::random_hex(6)));
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detection_as_vercels() {
        let none = |_: &str| false;
        let rr = project(&[("vite.config.ts", "import { reactRouter } from \"@react-router/dev/vite\";")]);
        assert_eq!(detect(&rr, None, &none), Some(Kind::ReactRouter));
        let rr = project(&[("react-router.config.ts", "export default {}")]);
        assert_eq!(detect(&rr, None, &none), Some(Kind::ReactRouter));
        let remix = project(&[("vite.config.ts", "@react-router/dev/vite")]);
        assert_eq!(detect(&remix, None, &|d| d == "@remix-run/dev"), Some(Kind::Remix), "Remix comes first");
        assert_eq!(detect(&project(&[("remix.config.js", "")]), None, &none), Some(Kind::Remix));
        assert_eq!(detect(&project(&[("vite.config.ts", "")]), None, &none), None);
        assert_eq!(detect(&project(&[]), Some("react-router"), &none), Some(Kind::ReactRouter));
        // The classic compiler is refused; Remix with Vite is built.
        let package = json!({"scripts": {"build": "remix build"}});
        let classic = project(&[("remix.config.js", "module.exports = {}")]);
        assert!(framework(&classic, Kind::Remix, Some(&package), None).unwrap_err().to_string().contains("classic compiler"));
        let both = project(&[("remix.config.js", ""), ("vite.config.ts", "import { vitePlugin } from '@remix-run/dev'")]);
        assert!(framework(&both, Kind::Remix, Some(&package), None).is_ok());
        let vite = project(&[("vite.config.ts", "")]);
        let f = framework(&vite, Kind::Remix, Some(&json!({"scripts": {"build": "remix vite:build", "vercel-build": "x"}})), None).unwrap();
        assert_eq!((f.build.as_deref(), f.find, f.output.as_str()), (Some("npm run vercel-build"), Output::BuildOutput, ".vercel/output"));
        let f = framework(&vite, Kind::ReactRouter, Some(&json!({})), Some(&json!({"installCommand": "pnpm i"}))).unwrap();
        assert_eq!((f.build.as_deref(), f.install.as_str()), (Some("react-router build"), "pnpm i"));
        assert!(framework(&project(&[]), Kind::ReactRouter, Some(&json!({})), None).unwrap_err().to_string().contains("vite.config"));
    }

    struct Quiet;
    impl BuildLog for Quiet {
        fn line(&self, _: buildlog::Stream, _: &str) {}
    }

    /// A tracer that finds what nft finds for these apps' functions, without esbuild: the app's
    /// `package.json`, the entry, its bundle and the one package each imports.
    fn fake_trace(_: &Path, handlers: &BTreeMap<String, String>, _: &BTreeMap<String, String>, _: &dyn BuildLog) -> Result<BTreeMap<String, BTreeSet<String>>> {
        Ok(handlers
            .iter()
            .map(|(id, h)| {
                let bundle = h.rsplit_once('/').map_or(String::new(), |(d, _)| format!("{d}/index.js"));
                (id.clone(), BTreeSet::from(["package.json".to_owned(), h.clone(), bundle, "node_modules/pkg/index.js".to_owned()]))
            })
            .collect())
    }

    /// fw-react-router's build as its preset leaves it (docs/data/c16/fw-react-router): three
    /// bundles, a prerendered page, the assets.
    fn react_router_app() -> PathBuf {
        let rr = "nodejs_eyJydW50aW1lIjoibm9kZWpzIn0";
        let heavy = "nodejs_eyJtYXhEdXJhdGlvbiI6MzAsIm1lbW9yeSI6MTAyNCwicnVudGltZSI6Im5vZGVqcyJ9";
        let limited = "nodejs_eyJtYXhEdXJhdGlvbiI6NjAsInJ1bnRpbWUiOiJub2RlanMifQ";
        let route = |id: &str, parent: &str, path: Option<&str>| match path {
            Some(p) => json!({"id": id, "parentId": parent, "path": p, "config": {"runtime": "nodejs"}}),
            None => json!({"id": id, "parentId": parent, "index": true, "config": {"runtime": "nodejs"}}),
        };
        let mut routes = serde_json::Map::new();
        routes.insert("root".into(), json!({"id": "root", "path": "", "config": {}}));
        // In the file's order (serde_json's maps sort their keys), as the preset writes it.
        let mut ids: Vec<String> = Vec::new();
        for (id, path, bundle) in [
            ("routes/home", None, rr),
            ("routes/ssr", Some("ssr"), rr),
            ("routes/form", Some("form"), rr),
            ("routes/heavy", Some("heavy"), heavy),
            ("routes/static", Some("static"), rr),
            ("routes/item", Some("items/:id"), rr),
            ("routes/about", Some(":lang?/about"), rr),
            ("routes/docs", Some("docs/*"), rr),
            ("routes/limited-child", Some("limited/child"), limited),
            ("routes/api.hello", Some("api/hello"), rr),
        ] {
            let parent = if id == "routes/limited-child" { "routes/limited" } else { "root" };
            routes.insert(id.into(), route(id, parent, path));
            ids.push(format!("{}: {}", json!(id), json!(bundle)));
        }
        routes.insert("routes/limited".into(), json!({"id": "routes/limited", "parentId": "root", "config": {"maxDuration": 60}}));
        let bundles = json!({
            rr: {"id": rr, "file": format!("build/server/{rr}/index.js"), "config": {"runtime": "nodejs"}},
            heavy: {"id": heavy, "file": format!("build/server/{heavy}/index.js"), "config": {"maxDuration": 30, "memory": 1024, "runtime": "nodejs"}},
            limited: {"id": limited, "file": format!("build/server/{limited}/index.js"), "config": {"maxDuration": 60, "runtime": "nodejs"}},
        });
        let dir = project(&[
            ("package.json", r#"{"type": "module"}"#),
            ("node_modules/react-router/package.json", r#"{"version": "8.5.1"}"#),
            ("node_modules/pkg/index.js", ""),
            ("build/client/static/index.html", "<p>static</p>"),
            ("build/client/static.data", "[]"),
            ("build/client/assets/entry-abc.js", ""),
            ("build/client/favicon.ico", ""),
            (&format!("build/server/{rr}/index.js"), ""),
            (&format!("build/server/{heavy}/index.js"), ""),
            (&format!("build/server/{limited}/index.js"), ""),
        ]);
        let result = format!(
            r#"{{"buildManifest": {{"routes": {}, "serverBundles": {bundles}, "routeIdToServerBundleId": {{{}}}}}, "reactRouterConfig": {{"buildDirectory": {}, "future": {{}}}}, "viteConfig": {{"build": {{"assetsDir": "assets"}}}}}}"#,
            Value::Object(routes),
            ids.join(", "),
            json!(dir.canonicalize().unwrap().join("build")),
        );
        std::fs::create_dir_all(dir.join(".vercel")).unwrap();
        std::fs::write(dir.join(".vercel/react-router-build-result.json"), result).unwrap();
        dir
    }

    fn route_of(i: &build_output::Imported, statics: &[&str], path: &str) -> (forkrail_common::rules::Outcome, Option<String>, Vec<(String, String)>) {
        use forkrail_common::rules::{Engine, Files, Request};
        struct Static(Vec<String>);
        impl Files for Static {
            fn has(&self, path: &str) -> bool {
                self.0.iter().any(|p| p == path)
            }
        }
        let engine = Engine::new(&i.program).unwrap();
        let mut headers = hyper::HeaderMap::new();
        headers.insert("sec-fetch-dest", "document".parse().unwrap());
        let files = Static(statics.iter().map(|s| (*s).to_owned()).collect());
        let d = engine.route(&Request { method: "GET", path, query: None, host: "x", headers: &headers }, &files);
        (d.outcome, d.function, d.headers)
    }

    /// The whole way, as `forkrail build` goes: the preset's result → `.vercel/output` as `vercel
    /// build` writes it (fw-react-router-skew's names, links and routes) → build_output.rs's
    /// import, merged with `vercel.json`, every path routed as on Vercel.
    #[test]
    fn react_router_imports_as_vercel_builds_it() {
        use forkrail_common::rules::Outcome;
        let dir = react_router_app();
        let env: BTreeMap<String, String> = [("VERCEL_SKEW_PROTECTION_ENABLED", "1"), ("VERCEL_DEPLOYMENT_ID", "dpl_test123")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        package_with(&dir, Kind::ReactRouter, &env, &Quiet, &fake_trace).unwrap();
        let out = dir.join(build_output::DIR);
        let link = |name: &str| std::fs::read_link(out.join("functions").join(format!("{name}.func"))).ok().map(|p| p.to_string_lossy().into_owned());
        assert_eq!(link("index"), None, "the first route's name is the real folder");
        assert_eq!(link("_root.data").as_deref(), Some("index.func"));
        assert_eq!(link("items/:id").as_deref(), Some("../index.func"));
        assert_eq!(link("(:lang)/about.data").as_deref(), Some("../index.func"));
        assert_eq!(link("heavy.data").as_deref(), Some("heavy.func"));
        assert_eq!(link("limited/child.data").as_deref(), Some("child.func"));
        assert!(!out.join("functions/static.func").exists() && !out.join("functions/static.data.func").exists(), "a prerendered page has no function");
        assert!(out.join("static/static/index.html").is_file() && out.join("static/assets/entry-abc.js").is_file());
        let vc: Value = serde_json::from_slice(&std::fs::read(out.join("functions/heavy.func/.vc-config.json")).unwrap()).unwrap();
        assert_eq!((&vc["memory"], &vc["maxDuration"], &vc["useWebApi"], &vc["shouldAddHelpers"]), (&json!(1024), &json!(30), &json!(true), &json!(false)));
        assert_eq!(vc["handler"], json!("build/server/nodejs_eyJtYXhEdXJhdGlvbiI6MzAsIm1lbW9yeSI6MTAyNCwicnVudGltZSI6Im5vZGVqcyJ9/server-index.mjs"));
        assert_eq!(vc["framework"], json!({"slug": "react-router", "version": "8.5.1"}));
        let entry = std::fs::read_to_string(dir.join(vc["handler"].as_str().unwrap())).unwrap();
        assert!(entry.contains("import * as built from './index.js'") && entry.contains("RR.createRequestHandler(build)"));

        let vercel_json = json!({"headers": [{"source": "/api/(.*)", "headers": [{"key": "x-from-vercel-json", "value": "1"}]}]});
        let imported = build_output::import(&dir, build_output::read(&out).unwrap(), vercel_json.as_object().unwrap(), "React Router").unwrap();
        assert_eq!(imported.functions, ["/heavy", "/index", "/limited/child"]);
        assert_eq!(imported.framework.as_deref(), Some("remix"), "Vercel's builder names both frameworks remix");
        let floor = imported.floor.unwrap();
        assert_eq!((floor.memory_mb, floor.max_duration_s), (Some(1024), Some(60)));
        assert_eq!((imported.program.max_duration.get("/heavy.data"), imported.program.max_duration.get("/limited/child")), (Some(&30), Some(&60)));
        // One copy of the project's files serves the three functions, each through its entry.
        assert!(imported.files.iter().all(|f| f.rel.starts_with(".forkrail/project/")), "{:?}", imported.files.iter().map(|f| &f.rel).collect::<Vec<_>>());
        assert_eq!(imported.files.iter().filter(|f| f.rel == ".forkrail/project/node_modules/pkg/index.js").count(), 1);
        assert!(imported.dispatcher.contains("cwd: fileURLToPath(at(\"project/\"))"), "{}", imported.dispatcher);
        assert_eq!(imported.dispatcher.matches("web: true").count(), 3);
        // Vercel's order: the skew cookie, vercel.json's header, the prerender rewrite, the filesystem.
        let froms: Vec<String> = imported.program.routes.iter().map(|r| match r {
            forkrail_common::rules::Route::Source(s) => s.dest.clone().or_else(|| s.headers.first().map(|h| h.key.clone())).unwrap_or_default(),
            forkrail_common::rules::Route::Handle { handle } => format!("{handle:?}"),
        }).collect();
        assert_eq!(froms[..4], ["Set-Cookie", "x-from-vercel-json", "/static/index.html", "Filesystem"]);
        let statics = ["/static/index.html", "/static.data", "/assets/entry-abc.js", "/favicon.ico"];
        let function = |path: &str| route_of(&imported, &statics, path).1;
        assert_eq!(function("/items/42").as_deref(), Some("/items/:id"));
        assert_eq!(function("/items/42.data").as_deref(), Some("/items/:id.data"));
        assert_eq!(function("/en/about").as_deref(), Some("/(:lang)/about"));
        assert_eq!(function("/about").as_deref(), Some("/(:lang)/about"));
        assert_eq!(function("/docs/a/b").as_deref(), Some("/docs/*"));
        assert_eq!(function("/docs").as_deref(), Some("/docs/*"));
        assert_eq!(function("/heavy").as_deref(), Some("/heavy"));
        assert_eq!(function("/ssr/").as_deref(), Some("/ssr/"));
        assert_eq!(function("/_root.data").as_deref(), Some("/_root.data"));
        assert_eq!(function("/nope").as_deref(), Some("/index"), "the catch-all");
        assert_eq!(route_of(&imported, &statics, "/static").0, Outcome::Static { file: "/static/index.html".into() });
        assert_eq!(route_of(&imported, &statics, "/static.data").0, Outcome::Static { file: "/static.data".into() });
        let (outcome, _, headers) = route_of(&imported, &statics, "/assets/entry-abc.js");
        assert_eq!(outcome, Outcome::Static { file: "/assets/entry-abc.js".into() });
        assert!(headers.contains(&("cache-control".to_owned(), "public, max-age=31536000, immutable".to_owned())), "{headers:?}");
        assert!(headers.iter().any(|(k, v)| k == "set-cookie" && v.starts_with("__vdpl=dpl_test123;")), "{headers:?}");
        // A second build reads its own result, not this one's.
        before_build(&dir, Kind::ReactRouter).unwrap();
        assert!(!dir.join(".vercel/react-router-build-result.json").exists());
    }

    #[test]
    fn remix_imports_as_vercel_builds_it() {
        let node = "nodejs-eyJydW50aW1lIjoibm9kZWpzIn0";
        let heavy = "nodejs-eyJtYXhEdXJhdGlvbiI6MzAsIm1lbW9yeSI6MTAyNCwicnVudGltZSI6Im5vZGVqcyJ9";
        let dir = project(&[
            ("package.json", r#"{"type": "module"}"#),
            ("node_modules/@remix-run/dev/package.json", r#"{"version": "2.17.5"}"#),
            ("node_modules/pkg/index.js", ""),
            ("build/client/assets/entry-abc.js", ""),
            ("build/client/favicon.ico", ""),
            (&format!("build/server/{node}/index.js"), ""),
            (&format!("build/server/{heavy}/index.js"), ""),
        ]);
        let result = format!(r#"{{"buildManifest": {{"routes": {}, "serverBundles": {}, "routeIdToServerBundleId": {{"routes/api.hello": "{node}", "routes/_index": "{node}", "routes/heavy": "{heavy}", "routes/items.$id": "{node}"}}}}, "remixConfig": {{"buildDirectory": "build"}}}}"#,
            json!({
                    "root": {"id": "root", "path": ""},
                    "routes/api.hello": {"id": "routes/api.hello", "parentId": "root", "path": "api/hello"},
                    "routes/_index": {"id": "routes/_index", "parentId": "root", "index": true},
                    "routes/heavy": {"id": "routes/heavy", "parentId": "root", "path": "heavy"},
                    "routes/items.$id": {"id": "routes/items.$id", "parentId": "root", "path": "items/:id"}
            }),
            json!({
                node: {"id": node, "file": format!("build/server/{node}/index.js"), "config": {"runtime": "nodejs"}},
                heavy: {"id": heavy, "file": format!("build/server/{heavy}/index.js"), "config": {"maxDuration": 30, "memory": 1024, "runtime": "nodejs"}}
            }),
        );
        std::fs::create_dir_all(dir.join(".vercel")).unwrap();
        std::fs::write(dir.join(".vercel/remix-build-result.json"), result).unwrap();
        let env: BTreeMap<String, String> = [("VERCEL_SKEW_PROTECTION_ENABLED", "1"), ("VERCEL_DEPLOYMENT_ID", "d")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        package_with(&dir, Kind::Remix, &env, &Quiet, &fake_trace).unwrap();
        let out = dir.join(build_output::DIR);
        assert!(std::fs::read_link(out.join("functions/api/hello.func")).is_err(), "fw-remix's real folder is api/hello.func");
        assert_eq!(std::fs::read_link(out.join("functions/index.func")).unwrap(), PathBuf::from("api/hello.func"));
        assert_eq!(std::fs::read_link(out.join("functions/items/:id.func")).unwrap(), PathBuf::from("../api/hello.func"));
        assert!(!out.join("functions/index.data.func").exists(), "no .data names for Remix");
        let config: Value = serde_json::from_slice(&std::fs::read(out.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["routes"], json!([
            {"handle": "filesystem"},
            {"src": r"^\/items(?:\/([^\/#\?]+?))[\/#\?]?$", "dest": "items/:id"},
            {"src": "/(.*)", "dest": "/"},
            {"handle": "hit"},
            {"src": "^/assets/(.*)$", "headers": {"cache-control": "public, max-age=31536000, immutable"}, "continue": true}
        ]), "fw-remix's routes, no skew route for Remix");
        assert_eq!(config["framework"], json!({"slug": "remix", "version": "2.17.5"}));
        let vc: Value = serde_json::from_slice(&std::fs::read(out.join("functions/api/hello.func/.vc-config.json")).unwrap()).unwrap();
        assert!(vc.get("useWebApi").is_none() && vc["shouldAddHelpers"] == json!(false));
        let entry = std::fs::read_to_string(dir.join(vc["handler"].as_str().unwrap())).unwrap();
        assert!(entry.contains("import * as build from './index.js'") && entry.contains("x-forwarded-host"));
        let imported = build_output::import(&dir, build_output::read(&out).unwrap(), &serde_json::Map::new(), "Remix").unwrap();
        assert_eq!(imported.functions, ["/api/hello", "/heavy"]);
        assert!(!imported.dispatcher.contains("web: true"), "Remix's functions take Node's (req, res)");
        assert_eq!(route_of(&imported, &[], "/nope").1.as_deref(), Some("/"), "the catch-all to `/` finds the index function");
        assert_eq!(route_of(&imported, &[], "/items/7").1.as_deref(), Some("/items/:id"));
        assert_eq!(route_of(&imported, &[], "/heavy.data").1.as_deref(), Some("/"));
    }

    /// Without the preset: one bundle at build/server/index.js for the index route, as Vercel
    /// guesses (fw-react-router-nopreset); without a build/ folder, a clear refusal.
    #[test]
    fn without_the_preset() {
        let dir = project(&[("package.json", "{}"), ("build/server/index.js", ""), ("build/client/favicon.ico", "")]);
        package_with(&dir, Kind::ReactRouter, &BTreeMap::new(), &Quiet, &fake_trace).unwrap();
        let out = dir.join(build_output::DIR);
        let names: BTreeSet<String> = std::fs::read_dir(out.join("functions")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, BTreeSet::from(["index.func".to_owned(), "index.data.func".to_owned(), "_root.data.func".to_owned()]));
        let vc: Value = serde_json::from_slice(&std::fs::read(out.join("functions/index.func/.vc-config.json")).unwrap()).unwrap();
        assert_eq!(vc["handler"], json!("build/server/server-index.mjs"));
        let empty = project(&[("package.json", "{}")]);
        assert!(package_with(&empty, Kind::ReactRouter, &BTreeMap::new(), &Quiet, &fake_trace).unwrap_err().to_string().contains("vercelPreset()"));
        // A build that wrote its own Build Output keeps it; another version is refused.
        let own = project(&[(".vercel/output/config.json", r#"{"version": 3}"#)]);
        package_with(&own, Kind::Remix, &BTreeMap::new(), &Quiet, &fake_trace).unwrap();
        let old = project(&[(".vercel/output/config.json", r#"{"version": 2}"#)]);
        assert!(package_with(&old, Kind::Remix, &BTreeMap::new(), &Quiet, &fake_trace).unwrap_err().to_string().contains("must be 3"));
        // An Edge bundle is refused by name; a bundle outside the project too.
        let edge = project(&[(".vercel/react-router-build-result.json", r#"{"buildManifest": {"serverBundles": {"edge_x": {"file": "build/server/edge_x/index.js", "config": {"runtime": "edge"}}}}, "reactRouterConfig": {"buildDirectory": "build"}}"#)]);
        assert!(package_with(&edge, Kind::ReactRouter, &BTreeMap::new(), &Quiet, &fake_trace).unwrap_err().to_string().contains("Edge runtime"));
        let outside = project(&[(".vercel/remix-build-result.json", r#"{"buildManifest": {}, "remixConfig": {"buildDirectory": "/etc"}}"#)]);
        assert!(format!("{:#}", package_with(&outside, Kind::Remix, &BTreeMap::new(), &Quiet, &fake_trace).unwrap_err()).contains("outside the project"));
    }

    #[test]
    fn links_name_their_targets_relatively() {
        let f = Path::new("/o/functions");
        assert_eq!(pathdiff(&f.join("index.func"), f), PathBuf::from("index.func"));
        assert_eq!(pathdiff(&f.join("index.func"), &f.join("items")), PathBuf::from("../index.func"));
        assert_eq!(pathdiff(&f.join("api/hello.func"), &f.join("api")), PathBuf::from("hello.func"));
    }
}
