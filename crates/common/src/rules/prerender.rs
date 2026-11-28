//! A deployment's prerenders: Vercel's Incremental Static Regeneration (ISR) as a Build Output
//! declares it, one `<path>.prerender-config.json` beside a function. The router keeps such a
//! function's answers as pages and serves them until they expire, then serves the old page while
//! the function makes a new one (`crates/router/src/cache.rs`). Read from Vercel's Build Output
//! API pages ("Primitives", "Features"; 2026-11-16) and from what the frameworks' adapters expect
//! (docs/c16-frameworks.md, F4).
//!
//! What the function gets differs from a plain function's request, as on Vercel:
//! - the query string only with `passQuery`, and then only the `allowQuery` parameters (seen on
//!   Vercel: `?v=1&other=2` reached a SvelteKit page as `?v=1`);
//! - without `passQuery`, the captures of the route that sent the request there, in
//!   `x-now-route-matches` (a form-encoded query string, filtered by `allowQuery`), which Nitro
//!   (`__isr_route`), Next.js and Vercel's examples read the page's path from.

use serde::{Deserialize, Serialize};

/// The request header that carries a route's captures to a prerender function.
pub const ROUTE_MATCHES_HEADER: &str = "x-now-route-matches";
/// A request with this header set to the prerender's bypass token makes the page again now
/// (on-demand revalidation), on GET or HEAD.
pub const REVALIDATE_HEADER: &str = "x-prerender-revalidate";
/// With `REVALIDATE_HEADER`: only if a page is cached already (Next.js's `res.revalidate` with
/// `unstable_onlyGenerated`); otherwise 404 without running the function.
pub const REVALIDATE_IF_GENERATED_HEADER: &str = "x-prerender-revalidate-if-generated";
/// The cookie whose value is the bypass token in Draft Mode: the function runs every time.
pub const BYPASS_COOKIE: &str = "__prerender_bypass";

/// One prerender's settings, as the router needs them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prerender {
    /// The function it belongs to (`/isr/[slug]`): the path whose `.prerender-config.json` it is.
    pub path: String,
    /// Seconds a page stays fresh once made; `None` (`expiration: false`) until it's revalidated
    /// on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiration: Option<u64>,
    /// Prerenders of one group are revalidated together on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u64>,
    /// The SHA-256 of `bypassToken`, in hex: the token itself stays in the build (the function has
    /// it), and the program the routers read only needs to recognise it.
    #[serde(default, rename = "bypassHash", skip_serializing_if = "Option::is_none")]
    pub bypass_hash: Option<String>,
    /// The query parameters that make pages apart: `None` all of them, empty none.
    #[serde(default, rename = "allowQuery", skip_serializing_if = "Option::is_none")]
    pub allow_query: Option<Vec<String>>,
    /// The function gets the query string (`allow_query`'s parameters only).
    #[serde(default, rename = "passQuery", skip_serializing_if = "std::ops::Not::not")]
    pub pass_query: bool,
    /// The page served while none has been made yet: its key in the deployment's static manifest
    /// (`forkrail_common::statics::fallback_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// The fallback's status and headers (`initialStatus`, `initialHeaders`).
    #[serde(default, rename = "initialStatus", skip_serializing_if = "Option::is_none")]
    pub initial_status: Option<u16>,
    #[serde(default, rename = "initialHeaders", skip_serializing_if = "Vec::is_empty")]
    pub initial_headers: Vec<(String, String)>,
    /// Show the body of an error answer as it is (`exposeErrBody`).
    #[serde(default, rename = "exposeErrBody", skip_serializing_if = "std::ops::Not::not")]
    pub expose_err_body: bool,
}

impl Prerender {
    fn allowed(&self, key: &str) -> bool {
        self.allow_query.as_ref().is_none_or(|allowed| allowed.iter().any(|a| a == key))
    }

    /// The query parameters that make this request's page apart from others, from its query string
    /// (encoded, as the function would get it): the allowed pairs, sorted, so their order in the
    /// URL doesn't matter. Empty when none count.
    pub fn key_query(&self, query: &str) -> String {
        let mut pairs: Vec<&str> = query.split('&').filter(|p| !p.is_empty() && self.allowed(&decoded_key(p))).collect();
        pairs.sort_unstable();
        pairs.join("&")
    }

    /// The query string the function gets: none without `passQuery`, else the allowed pairs in
    /// their order.
    pub fn function_query(&self, query: &str) -> String {
        if !self.pass_query {
            return String::new();
        }
        query.split('&').filter(|p| !p.is_empty() && self.allowed(&decoded_key(p))).collect::<Vec<_>>().join("&")
    }

    /// `x-now-route-matches` for a request the routes sent here with these captures (`None`: it
    /// reached the function by its own path, so there are none, and no header, as Vercel's
    /// on-demand ISR example expects). Not sent with `passQuery`, where the query carries them.
    /// The allowed captures only, form-encoded; empty when none are allowed (Next.js tells that
    /// apart from a missing header).
    pub fn route_matches(&self, captures: Option<&[(String, String)]>) -> Option<String> {
        if self.pass_query {
            return None;
        }
        let captures = captures?;
        let mut out = String::new();
        for (key, value) in captures.iter().filter(|(k, _)| self.allowed(k)) {
            if !out.is_empty() {
                out.push('&');
            }
            form_encode(key, &mut out);
            out.push('=');
            form_encode(value, &mut out);
        }
        Some(out)
    }
}

/// A query pair's key, percent-decoded (`a%5Bb%5D=1` is `a[b]`), as `allowQuery` names it.
fn decoded_key(pair: &str) -> String {
    let key = pair.split('=').next().unwrap_or(pair);
    super::convert::percent_decode(&key.replace('+', " "))
}

/// As `encodeURIComponent` writes it, which `URLSearchParams` and `querystring.parse` both read.
fn form_encode(text: &str, out: &mut String) {
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

/// A route's captures as `x-now-route-matches` carries them: each group by its number (from 1),
/// and a named group by its name too. Values are the path as the client sent it, decoded except
/// for an encoded slash, which stays a separate thing from a path's own slashes (the one reading
/// that fits both Next.js's and Nitro's expectations; Vercel's own proxy isn't open source).
pub fn captures(groups: &[Option<String>], names: &[Option<String>]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, group) in groups.iter().enumerate().skip(1) {
        let Some(value) = group else { continue };
        let value = decode_but_slash(value);
        out.push((i.to_string(), value.clone()));
        if let Some(Some(name)) = names.get(i) {
            out.push((name.clone(), value));
        }
    }
    out
}

fn decode_but_slash(value: &str) -> String {
    if !value.contains('%') {
        return value.to_owned();
    }
    // Decode each piece between encoded slashes, and put them back as they were.
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.to_ascii_lowercase().find("%2f") {
        out.push_str(&super::convert::percent_decode(&rest[..at]));
        out.push_str(&rest[at..at + 3]);
        rest = &rest[at + 3..];
    }
    out.push_str(&super::convert::percent_decode(rest));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prerender(allow: Option<&[&str]>, pass: bool) -> Prerender {
        Prerender { path: "/p".into(), allow_query: allow.map(|a| a.iter().map(|s| s.to_string()).collect()), pass_query: pass, ..Default::default() }
    }

    #[test]
    fn allow_query_filters_the_key_and_the_function_query() {
        // SvelteKit's `/isr/[slug]` with `allowQuery: ['v']`: `other` makes no new page and the
        // function never sees it (seen on Vercel).
        let p = prerender(Some(&["__pathname", "v"]), true);
        assert_eq!(p.key_query("v=1&other=2&__pathname=%2Fisr%2Fone"), "__pathname=%2Fisr%2Fone&v=1");
        assert_eq!(p.key_query("__pathname=%2Fisr%2Fone&v=1"), p.key_query("v=1&__pathname=%2Fisr%2Fone"));
        assert_eq!(p.function_query("v=1&other=2&__pathname=%2Fisr%2Fone"), "v=1&__pathname=%2Fisr%2Fone");
        // `allowQuery: []`: one page whatever the query.
        let none = prerender(Some(&[]), true);
        assert_eq!(none.key_query("a=1&b=2"), "");
        assert_eq!(none.function_query("a=1"), "");
        // Missing: every query its own page; without passQuery the function gets none.
        let all = prerender(None, false);
        assert_eq!(all.key_query("b=2&a=1"), "a=1&b=2");
        assert_eq!(all.function_query("a=1"), "");
        // Keys are compared decoded.
        assert_eq!(prerender(Some(&["a b"]), true).function_query("a+b=1&a%20b=2&c=3"), "a+b=1&a%20b=2");
    }

    #[test]
    fn route_matches_as_frameworks_read_them() {
        // Vercel's prerender-functions example: `/blog/(?<slug>[^/]*)`, `allowQuery: ["slug"]`.
        let groups = vec![Some("/blog/three".to_owned()), Some("three".to_owned())];
        let names = vec![None, Some("slug".to_owned())];
        let caps = captures(&groups, &names);
        assert_eq!(caps, vec![("1".into(), "three".into()), ("slug".into(), "three".into())]);
        assert_eq!(prerender(Some(&["slug"]), false).route_matches(Some(&caps)).as_deref(), Some("slug=three"));
        // Nitro: no allowQuery, the path in `__isr_route`.
        let groups = vec![Some("/isr/a b".to_owned()), Some("/isr/a%20b".to_owned())];
        let names = vec![None, Some("__isr_route".to_owned())];
        let caps = captures(&groups, &names);
        assert_eq!(prerender(None, false).route_matches(Some(&caps)).as_deref(), Some("1=%2Fisr%2Fa%20b&__isr_route=%2Fisr%2Fa%20b"));
        // Filtered to nothing: empty, not missing. Reached by its own path: missing.
        assert_eq!(prerender(Some(&[]), false).route_matches(Some(&caps)).as_deref(), Some(""));
        assert_eq!(prerender(None, false).route_matches(None), None);
        // With passQuery the query carries them.
        assert_eq!(prerender(None, true).route_matches(Some(&caps)), None);
    }

    #[test]
    fn encoded_slashes_stay_encoded() {
        assert_eq!(decode_but_slash("hello%2Fworld%20x"), "hello%2Fworld x");
        assert_eq!(decode_but_slash("100%25off"), "100%off");
        assert_eq!(decode_but_slash("plain"), "plain");
    }
}
