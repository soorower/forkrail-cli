//! A Next.js static export (`output: 'export'`), deployed as a static site the way Vercel deploys
//! one. Next writes the whole site as files (`out/`, or the folder `distDir` names) and there's no
//! server to run, so the exported folder is the deployment's static files, served as Vercel's Next
//! builder serves an export (`@vercel/next` 30.1.0, the path it takes when the build left
//! `export-detail.json`):
//!
//! - **Pages by their names.** The builder drops `.html` from every page's name, so `/about` is
//!   `about.html` (`about/index.html` with `trailingSlash`), `/` is `index.html`, and
//!   `/about.html` is found nowhere. forkrail keeps the names and serves them with `cleanUrls`'
//!   lookup, without its redirects, which does the same.
//! - **Next's own routes.** Its routes manifest's headers, redirects (the trailing slash's among
//!   them) and rewrites go around the files as the builder converts them, each matched by Next's
//!   own regex: on Vercel they apply to an export too (Next's warning that they don't is printed
//!   only where it isn't built on Vercel).
//! - **404.** `/404` answers 404, and a path nothing serves gets the export's `404.html`.
//! - **`/_next/static/`** is cached for good, as Next's hashed files are (service workers aside).
//! - **A base path.** Next writes the export without it while its pages ask for everything under
//!   it, so the files are served under it (`/docs/about`, `/docs/_next/static/…`) and the site works
//!   as `next start` serves it. Vercel's builder puts the export's files at the top instead, where
//!   a page's own scripts and links aren't found: the one place this differs on purpose.
//!
//! Middleware and API routes don't run in an export, on Vercel either (Next's build says so in its
//! log). Images: Next refuses an export whose pages use `next/image`'s default loader unless
//! `images.unoptimized` is set, so its pages never ask for `/_next/image`; Vercel's build passes
//! there because Next sees it builds on Vercel (`NOW_BUILDER`), which a forkrail build never sets.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use forkrail_common::rules::{Condition, SourceRoute, convert};
use serde_json::{Map, Value, json};

use crate::next::{self, NAME};
use crate::package::TempDir;

/// What Next writes in its dist folder when an export ends: `outDirectory` and `success`.
const DETAIL: &str = "export-detail.json";

/// A finished export.
pub struct Export {
    /// The exported folder: `out`, or the folder `distDir` names (Next then keeps its build in
    /// `.next`).
    pub dir: PathBuf,
    /// The Next that built it, for the log.
    pub version: String,
    pub base_path: String,
    pub trailing_slash: bool,
    /// The config's own `deploymentId`, when it names one.
    pub deployment_id: Option<String>,
    /// The routes Vercel's builder puts around the files, in `routes` form, as a preset's.
    pub routes: Value,
    /// How many of Next's routes are the config's own (`redirects()`, `rewrites()`, `headers()`),
    /// which Next's build says won't work in an export: they do here, as on Vercel.
    pub own_routes: usize,
    /// The static files, laid out by the paths they're served at: the export itself, or with a
    /// base path a scratch folder holding it under that path.
    pub static_dir: PathBuf,
    /// The path prefixes whose files are cached for good, and those inside them that aren't.
    pub hashed: Vec<String>,
    pub revalidated: Vec<String>,
    _scratch: Option<TempDir>,
}

/// The export the build in `dir` made, if it made one: None for a build of Next's server. `named`
/// is where the dist folder is looked for first (`next::find_dist`).
pub fn find(dir: &Path, named: &str) -> Result<Option<Export>> {
    let Some(dist) = next::find_dist(dir, named) else { return Ok(None) };
    let Some(config) = config_of(&dist)? else { return Ok(None) };
    let folder = folder(dir, &dist)?;
    let routes_manifest = json_file(&dist.join("routes-manifest.json"))?.unwrap_or(Value::Null);
    let base = config.get("basePath").and_then(Value::as_str).unwrap_or_default().trim_end_matches('/').to_owned();
    let has_404 = folder.join("404.html").is_file() || folder.join("404/index.html").is_file();
    let routes = routes(&routes_manifest, &base, has_404)?;
    let own_routes = own_routes(&routes_manifest);
    let (static_dir, scratch) = if base.is_empty() {
        (folder.clone(), None)
    } else {
        let scratch = TempDir::new("next-export")?;
        let root = scratch.path().join("static");
        let under = root.join(base.trim_start_matches('/'));
        for (rel, abs) in next::scan(&folder)? {
            next::link_or_copy(&abs, &under.join(&rel))?;
        }
        std::fs::create_dir_all(&root)?;
        (root, Some(scratch))
    };
    let hashed = vec![format!("{base}/_next/static/")];
    let revalidated = hashed.iter().map(|h| format!("{h}{}", next::SERVICE_WORKERS)).collect();
    Ok(Some(Export {
        dir: folder,
        version: next::installed(dir).map(|(text, _)| text).unwrap_or_else(|_| "unknown".to_owned()),
        base_path: base,
        trailing_slash: config.get("trailingSlash").and_then(Value::as_bool).unwrap_or(false),
        deployment_id: config.get("deploymentId").and_then(Value::as_str).filter(|d| !d.is_empty()).map(str::to_owned),
        routes,
        own_routes,
        static_dir,
        hashed,
        revalidated,
        _scratch: scratch,
    }))
}

/// The build's config (`required-server-files.json`'s), when it's an export's.
pub fn config_of(dist: &Path) -> Result<Option<Value>> {
    let required = json_file(&dist.join("required-server-files.json"))?.unwrap_or(Value::Null);
    let config = required.get("config").cloned().unwrap_or(Value::Null);
    Ok((config.get("output").and_then(Value::as_str) == Some("export")).then_some(config))
}

/// The folder the export went to, as Next noted it when it finished (`export-detail.json`), as
/// Vercel's builder finds it. A build made in another folder (`--prebuilt`) names that folder's,
/// whose last part is the same here.
pub fn folder(dir: &Path, dist: &Path) -> Result<PathBuf> {
    let path = dist.join(DETAIL);
    let Some(detail) = json_file(&path)? else {
        bail!("this {NAME} app is a static export (`output: 'export'`), but the build left no {}: Next writes it when its export ends, so the build command must run `next build`", path.display());
    };
    if detail.get("success").and_then(Value::as_bool) != Some(true) {
        bail!("{NAME}'s static export didn't finish ({} says so): the build's log above says why", path.display());
    }
    let named = detail.get("outDirectory").and_then(Value::as_str).with_context(|| format!("{} names no outDirectory", path.display()))?;
    let at = Path::new(named);
    if at.is_dir() {
        return Ok(at.to_path_buf());
    }
    match at.file_name().map(|name| dir.join(name)).filter(|here| here.is_dir()) {
        Some(here) => Ok(here),
        None => bail!("{NAME} exported the site to {named}, which isn't there"),
    }
}

fn json_file(path: &Path) -> Result<Option<Value>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("{} isn't valid JSON", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The routes Vercel's builder puts around an export's files, from Next's routes manifest, as far
/// as they change what a visitor gets: Next's headers, redirects and the rewrites before the
/// files; `/404` answered 404; after the files, `/_next/data/` (which only the export's own files
/// answer) and Next's other rewrites; and for paths nothing serves, the export's 404 page.
pub fn routes(manifest: &Value, base: &str, has_404: bool) -> Result<Value> {
    let b = regex_text(base);
    let not_found = format!("{base}/404");
    let mut out: Vec<Value> = Vec::new();
    for (i, item) in list(manifest.get("headers")).iter().enumerate() {
        out.push(headers(item, &format!("headers[{i}]"))?);
    }
    for (i, item) in list(manifest.get("redirects")).iter().enumerate() {
        out.push(redirect(item, &format!("redirects[{i}]"))?);
    }
    // Before Next 13 `rewrites` was one list, run after the files.
    let rewrites = manifest.get("rewrites");
    let (before, after, fallback) = match rewrites {
        Some(Value::Array(_)) => (None, rewrites, None),
        Some(r) => (r.get("beforeFiles"), r.get("afterFiles"), r.get("fallback")),
        None => (None, None, None),
    };
    // The builder makes these go on to the files rather than check them (Next 10.2.3 and later).
    for (i, item) in list(before).iter().enumerate() {
        let mut route = rewrite(item, &format!("rewrites.beforeFiles[{i}]"))?;
        if let Some(obj) = route.as_object_mut()
            && obj.remove("check").is_some()
        {
            obj.insert("continue".to_owned(), Value::Bool(true));
            obj.insert("override".to_owned(), Value::Bool(true));
        }
        out.push(route);
    }
    out.push(json!({ "src": format!("^{b}/404/?$"), "status": 404, "continue": true }));
    out.push(json!({ "handle": "filesystem" }));
    // The builder's no-op rewrite and 404 for `/_next/data/`: none of Next's rewrites takes it.
    out.push(json!({ "src": format!("^{b}/_next/data/(.*)$"), "dest": not_found, "status": 404 }));
    for (i, item) in list(after).iter().enumerate() {
        out.push(rewrite(item, &format!("rewrites.afterFiles[{i}]"))?);
    }
    for (i, item) in list(fallback).iter().enumerate() {
        out.push(rewrite(item, &format!("rewrites.fallback[{i}]"))?);
    }
    let on_match = list(manifest.get("onMatchHeaders"));
    if !on_match.is_empty() {
        out.push(json!({ "handle": "hit" }));
        for (i, item) in on_match.iter().enumerate() {
            out.push(headers(item, &format!("onMatchHeaders[{i}]"))?);
        }
    }
    if has_404 {
        out.push(json!({ "handle": "error" }));
        out.push(json!({ "src": format!("^{b}/.*$"), "dest": not_found, "status": 404 }));
    }
    Ok(Value::Array(out))
}

/// The config's own redirects, rewrites and headers in a routes manifest: all but Next's internal
/// ones (its trailing-slash redirects).
fn own_routes(manifest: &Value) -> usize {
    let rewrites = match manifest.get("rewrites") {
        Some(Value::Array(list)) => list.len(),
        Some(r) => ["beforeFiles", "afterFiles", "fallback"].iter().map(|k| list(r.get(*k)).len()).sum(),
        None => 0,
    };
    let redirects = list(manifest.get("redirects")).iter().filter(|r| r.get("internal").and_then(Value::as_bool) != Some(true)).count();
    redirects + rewrites + list(manifest.get("headers")).len()
}

fn list(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).cloned().unwrap_or_default()
}

/// A path as a regex matches it literally.
fn regex_text(path: &str) -> String {
    forkrail_common::rules::p2r::escape(path).replace("\\/", "/")
}

fn text<'a>(item: &'a Value, key: &str, what: &str) -> Result<&'a str> {
    item.get(key).and_then(Value::as_str).with_context(|| format!("{NAME}'s routes manifest: {what} has no `{key}`"))
}

fn conditions(item: &Value, key: &str, what: &str) -> Result<Vec<Condition>> {
    match item.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(list) => serde_json::from_value(list.clone()).with_context(|| format!("{NAME}'s routes manifest: {what}.{key}")),
    }
}

fn redirect(item: &Value, what: &str) -> Result<Value> {
    let route = convert::redirect(
        convert::Redirect {
            source: text(item, "source", what)?,
            destination: text(item, "destination", what)?,
            permanent: item.get("permanent").and_then(Value::as_bool),
            status: item.get("statusCode").and_then(Value::as_u64).and_then(|c| u16::try_from(c).ok()),
            has: conditions(item, "has", what)?,
            missing: conditions(item, "missing", what)?,
        },
        what.to_owned(),
    )
    .map_err(|e| anyhow::anyhow!("{NAME}'s routes manifest: {what}: {e}"))?;
    Ok(as_routes_item(route, item))
}

fn rewrite(item: &Value, what: &str) -> Result<Value> {
    let route = convert::rewrite(
        convert::Rewrite {
            source: text(item, "source", what)?,
            destination: text(item, "destination", what)?,
            status: item.get("statusCode").and_then(Value::as_u64).and_then(|c| u16::try_from(c).ok()),
            has: conditions(item, "has", what)?,
            missing: conditions(item, "missing", what)?,
        },
        what.to_owned(),
    )
    .map_err(|e| anyhow::anyhow!("{NAME}'s routes manifest: {what}: {e}"))?;
    Ok(as_routes_item(route, item))
}

fn headers(item: &Value, what: &str) -> Result<Value> {
    let mut pairs = Vec::new();
    for h in list(item.get("headers")) {
        pairs.push((text(&h, "key", what)?.to_owned(), text(&h, "value", what)?.to_owned()));
    }
    let route = convert::headers(
        convert::Headers { source: text(item, "source", what)?, headers: pairs, has: conditions(item, "has", what)?, missing: conditions(item, "missing", what)? },
        what.to_owned(),
    )
    .map_err(|e| anyhow::anyhow!("{NAME}'s routes manifest: {what}: {e}"))?;
    Ok(as_routes_item(route, item))
}

/// A converted route as a `routes` item, matched by Next's own regex for it when the manifest has
/// one, as Vercel's builder matches it (its `updateRouteSrc`).
fn as_routes_item(route: SourceRoute, item: &Value) -> Value {
    let mut obj = Map::new();
    let src = item.get("regex").and_then(Value::as_str).map(str::to_owned).unwrap_or(route.src);
    obj.insert("src".to_owned(), Value::String(src));
    if let Some(dest) = route.dest {
        obj.insert("dest".to_owned(), Value::String(dest));
    }
    if !route.headers.is_empty() {
        obj.insert("headers".to_owned(), Value::Object(route.headers.into_iter().map(|h| (h.key, Value::String(h.value))).collect()));
    }
    if let Some(status) = route.status {
        obj.insert("status".to_owned(), Value::from(status));
    }
    if route.cont {
        obj.insert("continue".to_owned(), Value::Bool(true));
    }
    if route.check {
        obj.insert("check".to_owned(), Value::Bool(true));
    }
    for (key, conditions) in [("has", route.has), ("missing", route.missing)] {
        if !conditions.is_empty() {
            obj.insert(key.to_owned(), serde_json::to_value(conditions).expect("conditions serialize"));
        }
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, files: &[(&str, &str)]) {
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    const MANIFEST: &str = r#"{"version": 3, "basePath": "", "redirects": [
        {"source": "/:path+/", "destination": "/:path+", "internal": true, "statusCode": 308, "regex": "^(?:/((?:[^/]+?)(?:/(?:[^/]+?))*))/$"},
        {"source": "/old-blog/:slug", "destination": "/blog/:slug", "statusCode": 307, "regex": "^(?!/_next)/old-blog(?:/([^/]+?))(?:/)?$"}],
      "headers": [{"source": "/about", "headers": [{"key": "x-about", "value": "yes"}], "regex": "^/about(?:/)?$"}],
      "rewrites": {"beforeFiles": [{"source": "/first", "destination": "/blog/first", "regex": "^/first(?:/)?$"}],
        "afterFiles": [{"source": "/alias", "destination": "/about", "regex": "^/alias(?:/)?$", "has": [{"type": "query", "key": "a"}]}], "fallback": []}}"#;

    /// An export as `next build` leaves one: the build in `.next`, the site in `site` (`out` unless
    /// a `distDir` names another), and where it went noted in `export-detail.json`.
    fn exported(dir: &Path, config: &str, site: &str, manifest: &str) {
        let out = dir.join(site);
        write(
            dir,
            &[
                (".next/BUILD_ID", "b1\n"),
                (".next/required-server-files.json", &format!(r#"{{"version": 1, "config": {config}}}"#)),
                (".next/routes-manifest.json", manifest),
                (".next/export-detail.json", &format!(r#"{{"version": 1, "outDirectory": {:?}, "success": true}}"#, out.to_string_lossy())),
                ("node_modules/next/package.json", r#"{"version": "16.4.6"}"#),
            ],
        );
        write(&out, &[("index.html", "home"), ("about.html", "about"), ("about.txt", "rsc"), ("404.html", "missing"), ("_next/static/chunks/a.js", "js"), ("_next/static/service-worker/sw.js", "sw")]);
    }

    fn project(name: &str) -> TempDir {
        TempDir::new(name).unwrap()
    }

    #[test]
    fn an_export_is_found_where_next_noted_it() {
        let dir = project("export-found");
        exported(dir.path(), r#"{"distDir": ".next", "output": "export", "trailingSlash": true}"#, "out", MANIFEST);
        let export = find(dir.path(), ".next").unwrap().expect("an export");
        assert_eq!(export.dir, dir.path().join("out"));
        assert_eq!(export.static_dir, export.dir, "without a base path the export is served as it is");
        assert_eq!((export.version.as_str(), export.base_path.as_str(), export.trailing_slash), ("16.4.6", "", true));
        assert_eq!(export.hashed, ["/_next/static/"]);
        assert_eq!(export.revalidated, ["/_next/static/service-worker/"]);
        assert_eq!(export.own_routes, 4, "the trailing slash's redirect is Next's own");
        // `next::check` takes it as a finished build, where it used to refuse it.
        next::check(dir.path(), ".next").unwrap();

        // A `distDir` names the export's folder; Next keeps the build in `.next`.
        let custom = project("export-distdir");
        exported(custom.path(), r#"{"distDir": ".next", "output": "export"}"#, "build/site", MANIFEST);
        assert_eq!(find(custom.path(), ".next").unwrap().unwrap().dir, custom.path().join("build/site"));
        // Built in another folder (`--prebuilt`): the folder by the same name here.
        let moved = project("export-moved");
        exported(moved.path(), r#"{"output": "export"}"#, "out", MANIFEST);
        write(moved.path(), &[(".next/export-detail.json", r#"{"version": 1, "outDirectory": "/elsewhere/app/out", "success": true}"#)]);
        assert_eq!(find(moved.path(), ".next").unwrap().unwrap().dir, moved.path().join("out"));
        // Not an export: a server's build is left to next.rs.
        let server = project("export-server");
        exported(server.path(), r#"{"output": "standalone"}"#, "out", MANIFEST);
        assert!(find(server.path(), ".next").unwrap().is_none());
        assert!(find(project("export-nothing").path(), ".next").unwrap().is_none());
    }

    #[test]
    fn an_export_that_didnt_finish_says_so() {
        let dir = project("export-failed");
        exported(dir.path(), r#"{"output": "export"}"#, "out", MANIFEST);
        write(dir.path(), &[(".next/export-detail.json", r#"{"version": 1, "outDirectory": "/x/out", "success": false}"#)]);
        assert!(find(dir.path(), ".next").err().unwrap().to_string().contains("didn't finish"));
        assert!(next::check(dir.path(), ".next").unwrap_err().to_string().contains("didn't finish"));
        std::fs::remove_file(dir.path().join(".next/export-detail.json")).unwrap();
        assert!(next::check(dir.path(), ".next").unwrap_err().to_string().contains("export-detail.json"));
        // Noted, but not there.
        write(dir.path(), &[(".next/export-detail.json", r#"{"version": 1, "outDirectory": "/x/gone", "success": true}"#)]);
        assert!(find(dir.path(), ".next").err().unwrap().to_string().contains("isn't there"));
    }

    #[test]
    fn a_base_path_puts_the_files_under_it() {
        let dir = project("export-base");
        exported(dir.path(), r#"{"output": "export", "basePath": "/docs", "deploymentId": "v42"}"#, "out", MANIFEST);
        let export = find(dir.path(), ".next").unwrap().unwrap();
        // Next writes the export without the base path; its pages ask for everything under it.
        assert_eq!(std::fs::read_to_string(export.static_dir.join("docs/about.html")).unwrap(), "about");
        assert!(export.static_dir.join("docs/_next/static/chunks/a.js").is_file());
        assert!(!export.static_dir.join("about.html").exists());
        assert_eq!(export.hashed, ["/docs/_next/static/"]);
        assert_eq!(export.deployment_id.as_deref(), Some("v42"));
    }

    #[test]
    fn the_routes_are_vercels_builders_for_an_export() {
        let all = routes(&serde_json::from_str(MANIFEST).unwrap(), "", true).unwrap();
        let list = all.as_array().unwrap();
        // Next's headers, then its redirects, each matched by Next's own regex, the destination's
        // parameters by number as the builder converts them.
        assert_eq!(list[0], json!({"src": "^/about(?:/)?$", "headers": {"x-about": "yes"}, "continue": true}));
        assert_eq!(list[1], json!({"src": "^(?:/((?:[^/]+?)(?:/(?:[^/]+?))*))/$", "headers": {"Location": "/$1"}, "status": 308}));
        assert_eq!(list[2], json!({"src": "^(?!/_next)/old-blog(?:/([^/]+?))(?:/)?$", "headers": {"Location": "/blog/$1"}, "status": 307}));
        // A rewrite before the files goes on to them, as the builder makes it.
        assert_eq!(list[3], json!({"src": "^/first(?:/)?$", "dest": "/blog/first", "continue": true, "override": true}));
        assert_eq!(list[4], json!({"src": "^/404/?$", "status": 404, "continue": true}));
        assert_eq!(list[5], json!({"handle": "filesystem"}));
        assert_eq!(list[6], json!({"src": "^/_next/data/(.*)$", "dest": "/404", "status": 404}));
        // A `has` key the destination doesn't use goes on in its query, as routing-utils passes it.
        assert_eq!(list[7], json!({"src": "^/alias(?:/)?$", "dest": "/about?a=$a", "check": true, "has": [{"type": "query", "key": "a"}]}));
        assert_eq!(&list[8..], [json!({"handle": "error"}), json!({"src": "^/.*$", "dest": "/404", "status": 404})]);
        // Without a 404 page, nothing for the error phase; with a base path, every route under it.
        let none = routes(&json!({}), "/docs", false).unwrap();
        assert_eq!(none, json!([{"src": "^/docs/404/?$", "status": 404, "continue": true}, {"handle": "filesystem"}, {"src": "^/docs/_next/data/(.*)$", "dest": "/docs/404", "status": 404}]));
        // Rewrites as one list (before Next 13) run after the files.
        let old = routes(&json!({"rewrites": [{"source": "/a", "destination": "/b"}]}), "", false).unwrap();
        assert_eq!(old[3], json!({"src": "^/a$", "dest": "/b", "check": true}));
        // A manifest item without what a route needs is refused, naming it.
        let broken = routes(&json!({"redirects": [{"source": "/a"}]}), "", false).unwrap_err().to_string();
        assert!(broken.contains("redirects[0]") && broken.contains("destination"), "{broken}");
    }
}
