//! What the router needs of a Next.js build (N3), made from N1's manifest (`next.rs`) at build:
//! the config object `static/next/<id>.json` (`forkrail_common::next::NextConfig`) and, when Next
//! optimizes images itself, its image settings for the router's image function, as Vercel's own
//! builder passes them (the same keys of `images-manifest.json`, `/_next/image` answered by the
//! platform). Both go in the build folder; publishing uploads them with the static files and
//! puts the deployment's `next` entry in the routing table.

use std::path::Path;

use anyhow::{Context, Result};
use forkrail_common::images::ImagesConfig;
use forkrail_common::next::{self as router, HeaderRule, Matcher, Middleware, NextConfig, Prerender, Route, RouteKind};
use forkrail_common::routes::{NextRef, NextShape};
use forkrail_common::rules::Condition;
use serde_json::Value;

use crate::next::{FunctionKind, Manifest, PrerenderKind};

/// What a Next build gives the router.
pub struct Prepared {
    pub entry: NextRef,
    /// Next's image settings, for `/_next/image` at the router; none when Next doesn't optimize
    /// (`unoptimized`, a custom loader or path).
    pub images: Option<ImagesConfig>,
}

/// The config and image settings of `manifest`, the config written into `out`; `server` is the
/// one the build packaged (Next's own, or forkrail's of Next's entrypoints).
pub fn prepare(manifest: &Manifest, out: &Path, server: crate::next_handlers::Server) -> Result<Prepared> {
    let images = images(manifest)?;
    let config = config(manifest, images.is_some());
    config.validate().map_err(|e| anyhow::anyhow!("the Next.js manifest: {e}"))?;
    let json = serde_json::to_vec(&config)?;
    let id = NextConfig::id(&json);
    let path = out.join(router::config_key(&id));
    std::fs::create_dir_all(path.parent().expect("keys have a folder"))?;
    std::fs::write(&path, &json).with_context(|| format!("writing {}", path.display()))?;
    let shape = match server {
        crate::next_handlers::Server::Next => NextShape::Server,
        crate::next_handlers::Server::Direct => NextShape::Handlers,
    };
    Ok(Prepared { entry: NextRef { shape, config: id }, images })
}

/// `has`/`missing` as Next wrote them. One that can't be read is left out: the matcher then
/// matches more requests, never fewer.
fn conditions(value: Option<&Value>) -> Vec<Condition> {
    value.and_then(|v| v.as_array()).map(|items| items.iter().filter_map(|c| serde_json::from_value(c.clone()).ok()).collect()).unwrap_or_default()
}

/// The pattern of each entry of a `routes-manifest.json` list.
fn regexes(list: Option<&Value>) -> Vec<String> {
    list.and_then(Value::as_array).map(|items| items.iter().filter_map(|r| r.get("regex")?.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

pub fn config(manifest: &Manifest, images: bool) -> NextConfig {
    let routes_manifest = &manifest.routes;
    let dynamic: std::collections::HashMap<&str, &str> = routes_manifest
        .get("dynamicRoutes")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|r| Some((r.get("page")?.as_str()?, r.get("regex")?.as_str()?))).collect())
        .unwrap_or_default();
    // Next's own pages (`/_not-found`, `/_global-error`, `/_error`, `/_app`) aren't paths a
    // visitor asks for: Next answers them by its own rules.
    let routes = manifest
        .functions
        .iter()
        .filter(|f| !f.pathname.starts_with("/_"))
        .map(|f| Route {
            path: f.pathname.clone(),
            page: f.page.clone(),
            kind: match f.kind {
                FunctionKind::AppPage => RouteKind::AppPage,
                FunctionKind::AppRoute => RouteKind::AppRoute,
                FunctionKind::Page => RouteKind::Page,
                FunctionKind::Api => RouteKind::Api,
            },
            // A dynamic route without a pattern of Next's is never matched by path: its pages go
            // to Next.
            regex: f.pathname.contains('[').then(|| dynamic.get(f.pathname.as_str()).map_or_else(|| "^$".to_owned(), |r| (*r).to_owned())),
            max_duration_s: f.max_duration,
        })
        .collect();
    let prerenders = manifest
        .prerenders
        .iter()
        .filter(|p| matches!(p.kind, PrerenderKind::Page | PrerenderKind::Route | PrerenderKind::Shell))
        .map(|p| Prerender {
            path: p.pathname.clone(),
            route: p.route.clone(),
            revalidate: p.revalidate,
            expire: p.expire,
            allow_query: p.allow_query.clone(),
            shell: p.kind == PrerenderKind::Shell || p.postponed.is_some(),
        })
        .collect();
    let rewrites = routes_manifest.get("rewrites");
    // Before Next 13 `rewrites` was one list: afterFiles.
    let (before_files, after_files) = match rewrites {
        Some(Value::Array(_)) => (None, rewrites),
        Some(r) => (r.get("beforeFiles"), r.get("afterFiles")),
        None => (None, None),
    };
    let mut before = regexes(routes_manifest.get("redirects"));
    before.extend(regexes(before_files));
    let after = regexes(after_files);
    let fallback = regexes(rewrites.and_then(|r| r.get("fallback")));
    let next_url_rsc = [before_files, after_files, rewrites.and_then(|r| r.get("fallback"))].into_iter().flatten().filter_map(Value::as_array).flatten().any(|r| {
        conditions(r.get("has")).iter().any(|c| c.key.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("next-url")))
    });
    let headers = routes_manifest
        .get("headers")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|h| {
                    Some(HeaderRule {
                        source: h.get("source")?.as_str()?.to_owned(),
                        regex: h.get("regex")?.as_str()?.to_owned(),
                        has: conditions(h.get("has")),
                        missing: conditions(h.get("missing")),
                        headers: h.get("headers")?.as_array()?.iter().filter_map(|kv| Some((kv.get("key")?.as_str()?.to_owned(), kv.get("value")?.as_str()?.to_owned()))).collect(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    NextConfig {
        format: router::FORMAT,
        version: manifest.next_version.clone(),
        build_id: manifest.build_id.clone(),
        deployment_id: manifest.deployment_id.clone(),
        preview_id: manifest.preview.id.clone(),
        base_path: manifest.base_path.clone(),
        trailing_slash: manifest.trailing_slash,
        i18n: manifest.i18n.as_ref().is_some_and(|v| !v.is_null()),
        cache_components: manifest.cache_components,
        middleware: manifest.middleware.as_ref().map(|m| Middleware {
            runtime: m.runtime.clone(),
            matchers: m.matchers.iter().map(|x| Matcher { regexp: x.regexp.clone(), has: conditions(x.has.as_ref()), missing: conditions(x.missing.as_ref()) }).collect(),
        }),
        routes,
        prerenders,
        before,
        after,
        headers,
        next_url_rsc,
        images,
        fallback,
        asset_prefix: asset_prefix_path(&manifest.asset_prefix, &manifest.base_path),
        // Public files only (`/_next/static` never needs the router's 404 to stay away).
        server_files: manifest.static_files.iter().filter(|f| !f.router && !f.immutable).map(|f| f.pathname.clone()).collect(),
        not_found: not_found(manifest),
        known: routes_manifest.get("staticRoutes").and_then(Value::as_array).map(|r| r.iter().filter_map(|x| Some(x.get("page")?.as_str()?.to_owned())).collect()).unwrap_or_default(),
        known_dynamic: regexes(routes_manifest.get("dynamicRoutes")),
    }
}

/// The path of `assetPrefix` (a URL's or a path's), when there's one other than the base path.
fn asset_prefix_path(prefix: &str, base: &str) -> String {
    let path = match prefix.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |at| &rest[at..]),
        None if prefix.starts_with("//") => prefix[2..].find('/').map_or("", |at| &prefix[2 + at..]),
        None => prefix,
    };
    let path = path.split(['?', '#']).next().unwrap_or("").trim_end_matches('/');
    if path.starts_with('/') && path != base { path.to_owned() } else { String::new() }
}

/// The build's not-found page: App Router's `/_not-found` (the router serves it only while the
/// store holds a fresh copy, so one Next renders per request, or a PPR shell, stays Next's), or a
/// Pages Router `/404` the build made that never goes stale.
fn not_found(manifest: &Manifest) -> Option<router::NotFound> {
    let made = |path: &str| manifest.prerenders.iter().find(|p| p.pathname == path && p.kind == PrerenderKind::Page && p.revalidate.is_none() && p.postponed.is_none());
    if let Some(f) = manifest.functions.iter().find(|f| f.pathname == "/_not-found" && f.kind == FunctionKind::AppPage) {
        return Some(router::NotFound { path: f.pathname.clone(), page: f.page.clone(), kind: RouteKind::AppPage });
    }
    made("/404").filter(|p| p.router == crate::next::Router::Pages).map(|p| router::NotFound { path: p.pathname.clone(), page: p.pathname.clone(), kind: RouteKind::Page })
}

/// Next's image settings as the router's image function takes them: what Vercel's builder passes
/// from `images-manifest.json` (its patterns are already regular expressions there). None when
/// Next doesn't optimize at `/_next/image`.
pub fn images(manifest: &Manifest) -> Result<Option<ImagesConfig>> {
    let Some(images) = manifest.images.as_ref().and_then(Value::as_object) else { return Ok(None) };
    let text = |key: &str| images.get(key).and_then(Value::as_str);
    if images.get("unoptimized").and_then(Value::as_bool) == Some(true) || text("loader").is_some_and(|l| l != "default") || text("path").is_some_and(|p| p != "/_next/image" && p != format!("{}/_next/image", manifest.base_path)) {
        return Ok(None);
    }
    let mut sizes: Vec<u64> = match images.get("sizes").and_then(Value::as_array) {
        Some(s) => s.iter().filter_map(Value::as_u64).collect(),
        None => ["deviceSizes", "imageSizes"].iter().flat_map(|k| images.get(*k).and_then(Value::as_array).into_iter().flatten()).filter_map(Value::as_u64).collect(),
    };
    sizes.dedup();
    let mut value = serde_json::Map::new();
    value.insert("sizes".into(), sizes.into());
    for key in ["domains", "remotePatterns", "localPatterns", "qualities", "minimumCacheTTL", "formats", "dangerouslyAllowSVG", "contentSecurityPolicy", "contentDispositionType"] {
        if let Some(v) = images.get(key).filter(|v| !v.is_null()) {
            value.insert(key.into(), v.clone());
        }
    }
    // Vercel's schema starts its remote patterns' protocol without the colon Next allows.
    if let Some(Value::Array(patterns)) = value.get_mut("remotePatterns") {
        for p in patterns.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(Value::String(proto)) = p.get_mut("protocol") {
                *proto = proto.trim_end_matches(':').to_owned();
            }
        }
    }
    forkrail_common::images::parse(&Value::Object(value)).map(Some).map_err(|e| anyhow::anyhow!("Next.js's image settings: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// next-app's manifest as N1 writes it (Next 16.4.6), cut down.
    fn manifest() -> Manifest {
        serde_json::from_value(serde_json::json!({
            "format": 1, "nextVersion": "16.4.6", "source": "adapter", "buildId": "b1", "basePath": "", "assetPrefix": "",
            "trailingSlash": false, "appDir": "", "distDir": ".next", "entry": ".forkrail/next.mjs",
            "preview": {"id": "0123456789abcdef0123456789abcdef"},
            "middleware": {"runtime": "nodejs", "matchers": [{"source": "/isr", "regexp": "^(?:\\/(_next\\/data\\/[^/]{1,}))?\\/isr(\\.json)?[\\/#\\?]?$",
                "missing": [{"type": "header", "key": "x-prerender-revalidate", "value": "0123456789abcdef0123456789abcdef"}]}]},
            "routes": {
                "redirects": [{"source": "/old-blog/:slug", "destination": "/blog/:slug", "statusCode": 308, "regex": "^(?!/_next)/old-blog(?:/([^/]+?))(?:/)?$"}],
                "headers": [{"source": "/ssr", "headers": [{"key": "x-from-config", "value": "yes"}], "regex": "^/ssr(?:/)?$"}],
                "rewrites": {"beforeFiles": [{"source": "/hi", "destination": "/api/hello", "regex": "^/hi(?:/)?$"}],
                    "afterFiles": [{"source": "/docs/:path*", "destination": "/ssr", "regex": "^/docs(?:/((?:[^/]+?)(?:/(?:[^/]+?))*))?(?:/)?$"}], "fallback": []},
                "dynamicRoutes": [{"page": "/blog/[slug]", "regex": "^/blog/([^/]+?)(?:/)?$"}]
            },
            "functions": [
                {"pathname": "/isr", "page": "/isr/page", "kind": "appPage", "runtime": "nodejs"},
                {"pathname": "/blog/[slug]", "page": "/blog/[slug]/page", "kind": "appPage", "runtime": "nodejs", "maxDuration": 30},
                {"pathname": "/api/hello", "page": "/api/hello/route", "kind": "appRoute", "runtime": "nodejs"}
            ],
            "prerenders": [
                {"pathname": "/isr", "route": "/isr", "page": "/isr/page", "router": "app", "kind": "page", "revalidate": 30, "files": []},
                {"pathname": "/blog/[slug]", "route": "/blog/[slug]", "router": "app", "kind": "fallback", "revalidate": 60, "files": []},
                {"pathname": "/blog/first", "route": "/blog/[slug]", "router": "app", "kind": "page", "revalidate": 60, "files": []}
            ],
            "prerenderFiles": "package", "staticFiles": []
        }))
        .unwrap()
    }

    #[test]
    fn the_routers_config_from_the_manifest() {
        let config = config(&manifest(), false);
        assert_eq!(config.preview_id, "0123456789abcdef0123456789abcdef");
        assert_eq!(config.routes[1].regex.as_deref(), Some("^/blog/([^/]+?)(?:/)?$"));
        assert_eq!(config.routes[1].max_duration_s, Some(30));
        assert_eq!(config.routes[0].regex, None);
        assert_eq!(config.prerenders.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(), ["/isr", "/blog/first"], "a fallback isn't a page");
        assert_eq!(config.before, ["^(?!/_next)/old-blog(?:/([^/]+?))(?:/)?$", "^/hi(?:/)?$"]);
        assert_eq!(config.after.len(), 1);
        assert_eq!(config.headers[0].headers, [("x-from-config".to_owned(), "yes".to_owned())]);
        let matcher = &config.middleware.as_ref().unwrap().matchers[0];
        assert_eq!(matcher.missing.len(), 1);
        // The router reads it as Next would.
        let site = forkrail_common::next::Site::compile(config).unwrap();
        let none = hyper::HeaderMap::new();
        let req = |path| forkrail_common::rules::Request { method: "GET", path, query: None, host: "a.test", headers: &none };
        assert!(site.middleware_runs(&req("/isr")));
        assert!(site.lookup(&req("/blog/first")).is_some());
        assert_eq!(site.max_duration("/blog/x"), Some(30));
        // Written by content.
        let dir = tempfile_dir();
        let prepared = prepare(&manifest(), &dir, crate::next_handlers::Server::Next).unwrap();
        assert!(dir.join(forkrail_common::next::config_key(&prepared.entry.config)).is_file());
        assert_eq!(prepare(&manifest(), &dir, crate::next_handlers::Server::Next).unwrap().entry, prepared.entry);
        assert_eq!(prepare(&manifest(), &dir, crate::next_handlers::Server::Direct).unwrap().entry.shape, NextShape::Handlers);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-next-router-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn asset_prefixes_and_the_not_found_page() {
        assert_eq!(asset_prefix_path("https://example.vercel.sh/custom-asset-prefix", ""), "/custom-asset-prefix");
        assert_eq!(asset_prefix_path("https://cdn.example.com", ""), "");
        assert_eq!(asset_prefix_path("/cdn/", ""), "/cdn");
        assert_eq!(asset_prefix_path("//cdn.example.com/x/", ""), "/x");
        assert_eq!(asset_prefix_path("/shop", "/shop"), "");
        let mut m = manifest();
        assert_eq!(config(&m, false).not_found, None);
        m.functions.push(serde_json::from_value(serde_json::json!({"pathname": "/_not-found", "page": "/_not-found/page", "kind": "appPage", "runtime": "nodejs"})).unwrap());
        assert_eq!(config(&m, false).not_found.map(|n| n.page), Some("/_not-found/page".to_owned()));
        assert_eq!(config(&m, false).known_dynamic, ["^/blog/([^/]+?)(?:/)?$"]);
    }

    #[test]
    fn images_as_vercel_passes_them() {
        let manifest_images = serde_json::json!({
            "deviceSizes": [640, 750], "imageSizes": [32], "path": "/_next/image", "loader": "default", "loaderFile": "",
            "domains": [], "minimumCacheTTL": 14400, "formats": ["image/webp"], "dangerouslyAllowSVG": false,
            "contentSecurityPolicy": "script-src 'none'; frame-src 'none'; sandbox;", "contentDispositionType": "attachment",
            "localPatterns": [{"pathname": "^(?:(?!(?:^|\\/)\\.{1,2}(?:\\/|$))(?:(?:(?!(?:^|\\/)\\.{1,2}(?:\\/|$)).)*?)\\/?)$", "search": ""}],
            "remotePatterns": [{"protocol": "https:", "hostname": "^(?:(?!\\.)(?=.)[^/]*?\\.saleor\\.cloud\\/?)$", "pathname": "^(?:.*)$"}],
            "qualities": [75], "unoptimized": false, "sizes": [640, 750, 32]
        });
        let mut m = manifest();
        m.images = Some(manifest_images.clone());
        let config = images(&m).unwrap().unwrap();
        assert_eq!(config.sizes, vec![640, 750, 32]);
        assert_eq!(config.qualities, Some(vec![75]));
        assert_eq!((config.minimum_cache_ttl, config.content_disposition_type.as_str()), (14400, "attachment"));
        assert_eq!(config.remote_patterns[0].protocol.as_deref(), Some("https"));
        let mut off = manifest_images.clone();
        off["unoptimized"] = true.into();
        m.images = Some(off);
        assert!(images(&m).unwrap().is_none());
        let mut custom = manifest_images;
        custom["loader"] = "cloudinary".into();
        m.images = Some(custom);
        assert!(images(&m).unwrap().is_none());
    }
}
