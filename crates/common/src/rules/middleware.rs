//! Vercel's Routing Middleware: a route with `middlewarePath` runs a function before routing goes
//! on, and that function's answer says what happens next. The router calls it (crates/router's
//! `middleware.rs`); this is what both the router and the CLI need to agree on: the route's shape,
//! the matcher compiler, and the headers of the protocol.
//!
//! The protocol, as `@vercel/functions` (3.11.3, `middleware.js`) and Next.js's `NextResponse`
//! speak it and Vercel's proxy answers it (seen on forkrail-check-fw-vite-middleware, 2026-11-16;
//! docs/data/c16/fw-vite-middleware/notes.md):
//! - `x-middleware-next: 1`: routing goes on from the route after the middleware's.
//! - `x-middleware-rewrite: <url>`: routing goes on with that URL's path and query in place of the
//!   request's (the function still gets the path the client asked for, as with any rewrite); a URL
//!   on another host is proxied there.
//! - `x-middleware-override-headers: a,b` with `x-middleware-request-a: ...`: the request goes on
//!   with exactly those headers (an empty value removes one; any not listed are dropped), but for
//!   the ones the platform owns.
//! - Other headers of a `next` or `rewrite` answer land on the final answer, as a route's headers
//!   do: `set-cookie` (and the auth challenges) added to the final answer's own.
//! - Neither: the middleware's answer is the response. A 3xx with `Location` is answered as Vercel
//!   answers a redirect (its own "Redirecting..." body), whatever body the middleware gave.
//! - The middleware runs once per request, also when its rewrite lands on a path it matches.

use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};

use super::{SourceRoute, p2r};

pub const NEXT: &str = "x-middleware-next";
pub const REWRITE: &str = "x-middleware-rewrite";
pub const OVERRIDE_HEADERS: &str = "x-middleware-override-headers";
pub const REQUEST_PREFIX: &str = "x-middleware-request-";
/// Every header of the protocol starts so; none of them reaches the client, and a client's own
/// are taken off its request.
pub const PREFIX: &str = "x-middleware-";
/// Matchers, the most a route compiles: Vercel's own limit on `routes` items' patterns is far
/// lower in characters (`MAX_PATTERN` applies to the joined pattern).
const MAX_MATCHERS: usize = 256;

/// A route's `middlewarePath` as a function path (`_middleware` → `/_middleware`), and its
/// `middlewareRawSrc` checked (the matchers it was made from, kept by Vercel for its logs only).
pub fn route_function(route: &Map<String, Value>, what: &str) -> Result<Option<String>, String> {
    if let Some(raw) = route.get("middlewareRawSrc") {
        let ok = raw.as_array().is_some_and(|list| list.iter().all(Value::is_string));
        if !ok {
            return Err(format!("{what}.middlewareRawSrc must be a list of paths"));
        }
    }
    let Some(path) = route.get("middlewarePath") else { return Ok(None) };
    let Some(path) = path.as_str() else { return Err(format!("{what}.middlewarePath must be text")) };
    let path = path.trim_start_matches('/');
    let ok = !path.is_empty() && path.len() <= 1024 && path.split('/').all(|s| !s.is_empty() && s != "." && s != "..") && !path.contains(['?', '#', '\\']);
    if !ok {
        return Err(format!("{what}.middlewarePath {path:?} isn't a function's path"));
    }
    Ok(Some(format!("/{path}")))
}

/// Vercel's matcher compiler (`@vercel/build-utils`' `getRegExpFromMatchers`): each matcher a
/// path-to-regexp 6.1.0 path with its defaults (not strict, so one trailing `/`, `#` or `?`
/// matches too), the sources joined with `|`; none matches every path. `/` also matches `/index`
/// unless that is listed itself.
pub fn matcher_source(matchers: Option<&[String]>) -> Result<String, String> {
    let Some(matchers) = matchers else { return Ok("^/.*$".to_owned()) };
    if matchers.is_empty() {
        return Err("the middleware's matcher is an empty list".to_owned());
    }
    if matchers.len() > MAX_MATCHERS {
        return Err(format!("the middleware has {} matchers; forkrail takes up to {MAX_MATCHERS}", matchers.len()));
    }
    let mut sources = Vec::new();
    for matcher in matchers {
        if !matcher.starts_with('/') {
            return Err(format!("the middleware's matcher values must start with \"/\": {matcher:?} doesn't"));
        }
        sources.push(path_regex(matcher).map_err(|e| format!("the middleware's matcher {matcher:?}: {e}"))?);
        if matcher == "/" && !matchers.iter().any(|m| m == "/index") {
            sources.push(path_regex("/index")?);
        }
    }
    let joined = sources.join("|");
    if joined.chars().count() > super::MAX_PATTERN {
        return Err(format!("the middleware's matchers come to a pattern longer than {} characters", super::MAX_PATTERN));
    }
    Ok(joined)
}

/// `pathToRegexp(path).source` with path-to-regexp 6.1.0's defaults: parameters stop at `/`, `#`
/// and `?`, and the end may have one of those after it.
fn path_regex(path: &str) -> Result<String, String> {
    use std::fmt::Write as _;
    let tokens = p2r::parse(path, "/#?")?;
    let mut route = String::from("^");
    for token in &tokens {
        match token {
            p2r::Token::Text(t) => route.push_str(&p2r::escape(t)),
            p2r::Token::Key(k) => {
                let (prefix, suffix) = (p2r::escape(&k.prefix), p2r::escape(&k.suffix));
                if k.pattern.is_empty() {
                    let _ = write!(route, "(?:{prefix}{suffix}){}", k.modifier);
                } else if prefix.is_empty() && suffix.is_empty() {
                    let _ = write!(route, "({}){}", k.pattern, k.modifier);
                } else if k.modifier == "+" || k.modifier == "*" {
                    let m = if k.modifier == "*" { "?" } else { "" };
                    let _ = write!(route, "(?:{prefix}((?:{p})(?:{suffix}{prefix}(?:{p}))*){suffix}){m}", p = k.pattern);
                } else {
                    let _ = write!(route, "(?:{prefix}({}){suffix}){}", k.pattern, k.modifier);
                }
            }
        }
    }
    route.push_str("[\\/#\\?]?$");
    Ok(route)
}

/// The route Vercel's builder writes for a project's middleware (`@vercel/node`): the matchers'
/// pattern, run before the filesystem, matched with regard to case (seen: `/OLD` doesn't run a
/// middleware matching `/old`).
pub fn route(function: &str, matchers: Option<&[String]>) -> Result<SourceRoute, String> {
    Ok(SourceRoute {
        src: matcher_source(matchers)?,
        cont: true,
        override_: true,
        case_sensitive: Some(true),
        middleware: Some(format!("/{}", function.trim_start_matches('/'))),
        from: Some("middleware".to_owned()),
        ..Default::default()
    })
}

/// What a middleware's answer says to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Go on routing.
    Next,
    /// Go on routing with this URL's path and query.
    Rewrite(Destination),
    /// Answer with a redirect to `location`, as Vercel answers one.
    Redirect { status: u16, location: String },
    /// The middleware's answer is the response.
    Answer,
}

/// Where a rewrite goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A path of this deployment (still percent-encoded), and the query string it brings (none:
    /// the request goes on without one).
    Here { path: String, query: Option<String> },
    /// A URL on another host, proxied.
    Elsewhere(String),
}

/// A middleware's answer, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub verdict: Verdict,
    /// The request headers to go on with in place of the request's, when the answer gives them:
    /// each lowercase name with its value, `None` to remove it.
    pub request_headers: Option<Vec<(String, Option<String>)>>,
    /// What a `next` or `rewrite` answer adds to the final response, in order, lowercase names.
    pub response_headers: Vec<(String, String)>,
}

/// Headers of a middleware's `next` or `rewrite` answer that don't go on to the final response:
/// they describe the middleware's own (empty) answer.
const OWN_HEADERS: [&str; 6] = ["date", "connection", "keep-alive", "content-length", "transfer-encoding", "content-type"];

/// Read a middleware's answer: its status and headers, and the host the request came to (a
/// rewrite to a URL on it stays in the deployment). Header names are lowercase in a `HeaderMap`.
pub fn read(status: u16, headers: &HeaderMap, host: &str) -> Reading {
    let value = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    let request_headers = value(OVERRIDE_HEADERS).map(|list| {
        let mut out: Vec<(String, Option<String>)> = Vec::new();
        for name in list.split(',').map(|n| n.trim().to_ascii_lowercase()).filter(|n| !n.is_empty()) {
            if out.iter().any(|(n, _)| *n == name) {
                continue;
            }
            let given = headers.get(format!("{REQUEST_PREFIX}{name}")).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
            out.push((name, given.filter(|v| !v.is_empty())));
        }
        out
    });
    let verdict = if let Some(url) = value(REWRITE).filter(|v| !v.is_empty()) {
        Verdict::Rewrite(destination(url, host))
    } else if value(NEXT) == Some("1") {
        Verdict::Next
    } else if matches!(status, 301 | 302 | 303 | 307 | 308)
        && let Some(location) = value("location").filter(|l| !l.is_empty())
    {
        Verdict::Redirect { status, location: location.to_owned() }
    } else {
        Verdict::Answer
    };
    let mut response_headers = Vec::new();
    if matches!(verdict, Verdict::Next | Verdict::Rewrite(_)) {
        for (name, value) in headers {
            let name = name.as_str();
            // The runtime's own (`x-forkrail-worker`, ...) describe the middleware's call.
            if name.starts_with(PREFIX) || name.starts_with("x-forkrail-") || OWN_HEADERS.contains(&name) {
                continue;
            }
            response_headers.push((name.to_owned(), String::from_utf8_lossy(value.as_bytes()).into_owned()));
        }
    }
    Reading { verdict, request_headers, response_headers }
}

/// Where `x-middleware-rewrite`'s value goes: a URL on the request's own host (any scheme, the
/// default ports) is a path here; a path alone is one too; any other URL is elsewhere.
pub fn destination(value: &str, host: &str) -> Destination {
    let value = value.split('#').next().unwrap_or("");
    let lower = value.get(..8).unwrap_or(value).to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &value[8..]
    } else if lower.starts_with("http://") {
        &value[7..]
    } else {
        return here(value);
    };
    let (authority, path_and_query) = match rest.find(['/', '?']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let bare = |a: &str| a.trim_end_matches(":443").trim_end_matches(":80").trim_end_matches('.').to_ascii_lowercase();
    if bare(authority) == bare(host) {
        here(path_and_query)
    } else {
        Destination::Elsewhere(value.to_owned())
    }
}

fn here(path_and_query: &str) -> Destination {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q.to_owned()).filter(|q| !q.is_empty())),
        None => (path_and_query, None),
    };
    let path = if path.starts_with('/') { path.to_owned() } else { format!("/{path}") };
    Destination::Here { path, query }
}

/// Headers a middleware can't change or remove on the request that goes on: the connection's
/// own (Vercel's list), and those the router sets for every request (the client's address and
/// location, the request id), which a middleware copying the request's headers gives back as
/// they were anyway.
pub fn keeps(name: &str) -> bool {
    const CONNECTION: [&str; 8] = ["host", "connection", "content-length", "transfer-encoding", "keep-alive", "te", "upgrade", "trailer"];
    const ROUTER: [&str; 4] = ["x-forwarded-for", "x-forwarded-host", "x-forwarded-proto", "x-real-ip"];
    CONNECTION.contains(&name) || ROUTER.contains(&name) || name.starts_with("x-forkrail-") || name.starts_with("x-vercel-")
}

/// The request's headers as the middleware said to go on with them (`Reading::request_headers`):
/// every header not listed is removed, each listed one set or removed, but for those it `keeps`.
pub fn override_request(headers: &mut HeaderMap, given: &[(String, Option<String>)]) {
    let listed = |name: &str| given.iter().any(|(n, _)| n == name);
    let gone: Vec<HeaderName> = headers.keys().filter(|k| !keeps(k.as_str()) && !listed(k.as_str())).cloned().collect();
    for name in gone {
        headers.remove(name);
    }
    for (name, value) in given {
        if keeps(name) {
            continue;
        }
        let Ok(header) = HeaderName::from_bytes(name.as_bytes()) else { continue };
        match value.as_deref().and_then(|v| HeaderValue::from_str(v).ok()) {
            Some(v) => {
                headers.insert(header, v);
            }
            None => {
                headers.remove(header);
            }
        }
    }
}

/// Headers several of which can stand side by side on one response: a middleware's go beside the
/// final answer's own.
pub fn adds_beside(name: &str) -> bool {
    matches!(name, "set-cookie" | "www-authenticate" | "proxy-authenticate")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut out = HeaderMap::new();
        for (k, v) in pairs {
            out.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        out
    }

    #[test]
    fn matchers_compile_as_vercel_build_wrote_them() {
        // docs/data/c16/fw-vite-middleware/output/config.json, vercel build 65.16.0.
        let matchers: Vec<String> = ["/", "/old", "/rewrite", "/geo", "/api/:path*", "/items/:id(\\d{1,})", "/vj-old"].iter().map(|s| s.to_string()).collect();
        let want = r"^\/[\/#\?]?$|^\/index[\/#\?]?$|^\/old[\/#\?]?$|^\/rewrite[\/#\?]?$|^\/geo[\/#\?]?$|^\/api(?:\/((?:[^\/#\?]+?)(?:\/(?:[^\/#\?]+?))*))?[\/#\?]?$|^\/items(?:\/(\d{1,}))[\/#\?]?$|^\/vj-old[\/#\?]?$";
        assert_eq!(matcher_source(Some(&matchers)).unwrap(), want);
        assert_eq!(matcher_source(None).unwrap(), "^/.*$");
        // `/index` listed itself isn't added twice.
        let both = vec!["/".to_owned(), "/index".to_owned()];
        assert_eq!(matcher_source(Some(&both)).unwrap(), r"^\/[\/#\?]?$|^\/index[\/#\?]?$");
        assert!(matcher_source(Some(&["old".to_owned()])).is_err());
        assert!(matcher_source(Some(&[])).is_err());
        // Vercel's negative lookahead example.
        let lookahead = vec!["/((?!api|_next/static|favicon.ico).*)".to_owned()];
        assert_eq!(matcher_source(Some(&lookahead)).unwrap(), r"^(?:\/((?!api|_next/static|favicon.ico).*))[\/#\?]?$");
    }

    #[test]
    fn a_middleware_route_is_the_builders() {
        let r = route("middleware", Some(&["/old".to_owned()])).unwrap();
        assert_eq!(r.middleware.as_deref(), Some("/middleware"));
        assert!(r.cont && r.override_);
        assert_eq!(r.case_sensitive, Some(true));
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["middlewarePath"], "/middleware");
        assert_eq!(json["override"], true);
    }

    #[test]
    fn route_function_reads_middleware_path() {
        let obj = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(route_function(&obj(serde_json::json!({"middlewarePath": "_middleware"})), "r").unwrap().as_deref(), Some("/_middleware"));
        assert_eq!(route_function(&obj(serde_json::json!({"src": "/"})), "r").unwrap(), None);
        assert!(route_function(&obj(serde_json::json!({"middlewarePath": "../x"})), "r").is_err());
        assert!(route_function(&obj(serde_json::json!({"middlewarePath": ""})), "r").is_err());
        assert!(route_function(&obj(serde_json::json!({"middlewarePath": "m", "middlewareRawSrc": "/"})), "r").is_err());
        assert!(route_function(&obj(serde_json::json!({"middlewarePath": "m", "middlewareRawSrc": ["/"]})), "r").is_ok());
    }

    #[test]
    fn next_with_request_and_response_headers() {
        // What `next({ headers: { 'x-mw-response': 'next' }, request: { headers } })` sends.
        let h = map(&[
            ("x-middleware-next", "1"),
            ("x-mw-response", "next"),
            ("x-middleware-override-headers", "accept,x-mw-request"),
            ("x-middleware-request-accept", "*/*"),
            ("x-middleware-request-x-mw-request", "hello"),
            ("content-length", "0"),
            ("date", "now"),
            ("x-forkrail-worker", "w1"),
            ("set-cookie", "a=1"),
        ]);
        let r = read(200, &h, "app.test");
        assert_eq!(r.verdict, Verdict::Next);
        assert_eq!(r.request_headers, Some(vec![("accept".into(), Some("*/*".into())), ("x-mw-request".into(), Some("hello".into()))]));
        assert_eq!(r.response_headers, vec![("x-mw-response".to_owned(), "next".to_owned()), ("set-cookie".to_owned(), "a=1".to_owned())]);
    }

    #[test]
    fn rewrites_here_and_elsewhere() {
        let h = map(&[("x-middleware-rewrite", "https://app.test/api/echo?item=42")]);
        assert_eq!(read(200, &h, "app.test").verdict, Verdict::Rewrite(Destination::Here { path: "/api/echo".into(), query: Some("item=42".into()) }));
        // A local router's plain HTTP and a port the host already has are the same origin.
        assert_eq!(destination("http://App.Test:443/a", "app.test"), Destination::Here { path: "/a".into(), query: None });
        assert_eq!(destination("https://app.test", "app.test"), Destination::Here { path: "/".into(), query: None });
        assert_eq!(destination("/b?x=1#frag", "app.test"), Destination::Here { path: "/b".into(), query: Some("x=1".into()) });
        assert_eq!(destination("https://other.test/c?d=1", "app.test"), Destination::Elsewhere("https://other.test/c?d=1".into()));
        assert_eq!(destination("https://app.test.evil.test/c", "app.test"), Destination::Elsewhere("https://app.test.evil.test/c".into()));
        assert_eq!(destination("https://app.test@evil.test/c", "app.test"), Destination::Elsewhere("https://app.test@evil.test/c".into()));
        // A rewrite says go on even without `x-middleware-next`, as `rewrite()` sends it.
        let both = map(&[("x-middleware-rewrite", "/x"), ("x-middleware-next", "1")]);
        assert!(matches!(read(200, &both, "app.test").verdict, Verdict::Rewrite(_)));
    }

    #[test]
    fn answers_and_redirects() {
        let json = map(&[("content-type", "application/json")]);
        let r = read(200, &json, "app.test");
        assert_eq!(r.verdict, Verdict::Answer);
        assert!(r.response_headers.is_empty());
        let redirect = map(&[("location", "https://app.test/")]);
        assert_eq!(read(307, &redirect, "app.test").verdict, Verdict::Redirect { status: 307, location: "https://app.test/".into() });
        // A 3xx without a location, or a 304, is an answer like any other.
        assert_eq!(read(302, &HeaderMap::new(), "app.test").verdict, Verdict::Answer);
        assert_eq!(read(304, &redirect, "app.test").verdict, Verdict::Answer);
        // `next` wins over a redirect status: Vercel's protocol reads its own header first.
        let next = map(&[("x-middleware-next", "1"), ("location", "/x")]);
        assert_eq!(read(307, &next, "app.test").verdict, Verdict::Next);
    }

    #[test]
    fn the_request_goes_on_with_the_listed_headers() {
        let mut req = map(&[
            ("accept", "text/html"),
            ("x-drop-me", "1"),
            ("cookie", "a=1"),
            ("x-forwarded-for", "1.2.3.4"),
            ("x-real-ip", "1.2.3.4"),
            ("x-vercel-ip-country", "BD"),
            ("x-forkrail-request-id", "abc"),
            ("content-length", "5"),
        ]);
        // `new Headers(request.headers)`, one set, one deleted, then a stranger's attempt to change
        // the client's address.
        let given = vec![
            ("accept".to_owned(), Some("text/html".to_owned())),
            ("cookie".to_owned(), None),
            ("x-mw-request".to_owned(), Some("hello".to_owned())),
            ("x-forwarded-for".to_owned(), Some("9.9.9.9".to_owned())),
        ];
        override_request(&mut req, &given);
        let names: Vec<&str> = req.keys().map(HeaderName::as_str).collect();
        assert!(!names.contains(&"x-drop-me") && !names.contains(&"cookie"));
        assert_eq!(req["x-mw-request"], "hello");
        assert_eq!(req["accept"], "text/html");
        assert_eq!(req["x-forwarded-for"], "1.2.3.4");
        assert_eq!(req["x-real-ip"], "1.2.3.4");
        assert_eq!(req["x-vercel-ip-country"], "BD");
        assert_eq!(req["x-forkrail-request-id"], "abc");
        assert_eq!(req["content-length"], "5");
    }

    #[test]
    fn a_listed_header_without_a_value_is_removed() {
        let h = map(&[("x-middleware-next", "1"), ("x-middleware-override-headers", "a, b"), ("x-middleware-request-a", "")]);
        assert_eq!(read(200, &h, "h").request_headers, Some(vec![("a".into(), None), ("b".into(), None)]));
        let none = map(&[("x-middleware-next", "1")]);
        assert_eq!(read(200, &none, "h").request_headers, None);
    }
}

#[cfg(test)]
mod engine_tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::super::{Engine, Files, Function, Outcome, Program, Request, Resume, Step};

    struct Fs(HashSet<&'static str>);

    impl Files for Fs {
        fn has(&self, path: &str) -> bool {
            self.0.contains(path)
        }
    }

    /// test-apps/fw-vite-middleware: its vercel.json, its middleware's matchers, a function at
    /// `/api/echo`.
    fn vite_program() -> Program {
        let config = json!({
            "redirects": [{ "source": "/vj-old", "destination": "/", "permanent": false }],
            "rewrites": [{ "source": "/vj-rewrite", "destination": "/rewritten.html" }],
            "headers": [{ "source": "/(.*)", "headers": [{ "key": "x-from-vercel-json", "value": "1" }] }]
        });
        let matchers: Vec<String> = ["/", "/old", "/rewrite", "/geo", "/api/:path*", "/items/:id(\\d{1,})", "/vj-old"].iter().map(|s| s.to_string()).collect();
        let route = super::route("middleware", Some(&matchers)).unwrap();
        let mut program = Program::with_middleware(config.as_object().unwrap(), &Function::default(), None, &[route]).unwrap().unwrap();
        program.function = vec!["/api/echo".to_owned()];
        program
    }

    /// Its engine and static files.
    fn vite() -> (Engine, Fs) {
        (Engine::new(&vite_program()).unwrap(), Fs(["/index.html", "/rewritten.html"].into_iter().collect()))
    }

    fn request<'a>(path: &'a str, query: Option<&'a str>, headers: &'a http::HeaderMap) -> Request<'a> {
        Request { method: "GET", path, query, host: "app.test", headers }
    }

    #[test]
    fn the_route_goes_after_vercel_json_redirects_and_headers_and_before_the_filesystem() {
        // As `vercel build` ordered them (docs/data/c16/fw-vite-middleware/notes.md).
        let order: Vec<String> = vite_program()
            .routes
            .iter()
            .map(|r| match r {
                super::super::Route::Handle { .. } => "filesystem".to_owned(),
                super::super::Route::Source(s) => s.from.clone().unwrap_or_default(),
            })
            .collect();
        assert_eq!(order, ["redirects[0]", "headers[0]", "middleware", "filesystem", "rewrites[0]"]);
    }

    #[test]
    fn a_vercel_json_redirect_comes_first() {
        let (engine, files) = vite();
        let headers = http::HeaderMap::new();
        let Step::Decided(d) = engine.step(&request("/vj-old", None, &headers), &files) else { panic!("the middleware ran") };
        assert_eq!(d.outcome, Outcome::Redirect { location: "/".into() });
        assert!(d.headers.is_empty(), "the headers route comes after the redirect: {:?}", d.headers);
    }

    #[test]
    fn next_goes_on_to_the_file_with_the_headers_so_far() {
        let (engine, files) = vite();
        let headers = http::HeaderMap::new();
        let req = request("/", None, &headers);
        let Step::Middleware { function, paused, .. } = engine.step(&req, &files) else { panic!("no middleware") };
        assert_eq!(function, "/middleware");
        let Step::Decided(d) = engine.resume(&req, &files, paused, Resume::Next) else { panic!("twice") };
        assert_eq!(d.outcome, Outcome::Static { file: "/index.html".into() });
        assert_eq!(d.headers, vec![("x-from-vercel-json".to_owned(), "1".to_owned())]);
    }

    #[test]
    fn a_rewrite_goes_on_with_its_path_once() {
        let (engine, files) = vite();
        let headers = http::HeaderMap::new();
        let req = request("/items/42", Some("x=1"), &headers);
        let Step::Middleware { paused, .. } = engine.step(&req, &files) else { panic!("no middleware") };
        // The router resumes with the rewrite's query in place of the client's.
        let rewritten = request("/items/42", Some("item=42"), &headers);
        let Step::Decided(d) = engine.resume(&rewritten, &files, paused, Resume::Rewrite { path: "/api/echo".into() }) else {
            panic!("the middleware ran again on its own rewrite")
        };
        assert_eq!(d.outcome, Outcome::Function { query: None });
        assert_eq!(d.function.as_deref(), Some("/api/echo"));
        // A rewrite to a file, and to a path nothing serves.
        let req = request("/rewrite", None, &headers);
        let Step::Middleware { paused, .. } = engine.step(&req, &files) else { panic!() };
        let Step::Decided(d) = engine.resume(&req, &files, paused, Resume::Rewrite { path: "/rewritten.html".into() }) else { panic!() };
        assert_eq!(d.outcome, Outcome::Static { file: "/rewritten.html".into() });
        let Step::Middleware { paused, .. } = engine.step(&req, &files) else { panic!() };
        let Step::Decided(d) = engine.resume(&req, &files, paused, Resume::Rewrite { path: "/nothing".into() }) else { panic!() };
        assert_eq!(d.status, Some(404));
    }

    #[test]
    fn unmatched_paths_and_case_skip_it() {
        let (engine, files) = vite();
        let headers = http::HeaderMap::new();
        for path in ["/items/abc", "/OLD", "/rewritten.html", "/vj-rewrite"] {
            assert!(matches!(engine.step(&request(path, None, &headers), &files), Step::Decided(_)), "{path}");
        }
        // One trailing slash still matches, as path-to-regexp's default allows (seen on Vercel).
        assert!(matches!(engine.step(&request("/old/", None, &headers), &files), Step::Middleware { .. }));
        // The middleware's own function isn't a path anyone can ask for.
        let Step::Decided(d) = engine.step(&request("/middleware", None, &headers), &files) else { panic!() };
        assert_eq!(d.status, Some(404));
        // `route` treats it as going on.
        assert_eq!(engine.route(&request("/", None, &headers), &files).outcome, Outcome::Static { file: "/index.html".into() });
    }

    #[test]
    fn inserting_it_later_matches_merging_it() {
        // A preset's leading `continue` route, a check route and another, around the project's.
        let preset = json!([
            { "src": "^/assets/(.*)$", "headers": { "cache-control": "immutable" }, "continue": true },
            { "src": "^/a$", "dest": "/b", "check": true },
            { "src": "^/c$", "status": 404 },
            { "handle": "filesystem" },
            { "src": "^/d$", "dest": "/e" }
        ]);
        let config = json!({ "redirects": [{ "source": "/x", "destination": "/y" }], "headers": [{ "source": "/(.*)", "headers": [{ "key": "k", "value": "v" }] }] });
        let route = super::route("middleware", Some(&["/".to_owned()])).unwrap();
        let config = config.as_object().unwrap();
        let merged = Program::with_middleware(config, &Function::default(), Some(("Vite", &preset)), std::slice::from_ref(&route)).unwrap().unwrap();
        let mut inserted = Program::from_vercel(config, &Function::default(), Some(("Vite", &preset))).unwrap().unwrap();
        inserted.insert_middleware(route.clone());
        assert_eq!(inserted.routes, merged.routes);
        // Into an empty program (a static site with no routes of its own).
        let mut empty = Program::default();
        empty.insert_middleware(route);
        assert_eq!(empty.routes.len(), 1);
    }

    #[test]
    fn a_build_outputs_middleware_route_imports() {
        // test-apps/boa/edge-middleware's config.json.
        let routes = json!([{ "src": "/(.*)", "middlewarePath": "_middleware", "continue": true }]);
        let program = Program::from_build_output(&serde_json::Map::new(), "boa", &routes, vec![], Default::default(), Default::default()).unwrap();
        let engine = Engine::new(&program).unwrap();
        let headers = http::HeaderMap::new();
        let files = Fs(["/index.html"].into_iter().collect());
        let Step::Middleware { function, paused, .. } = engine.step(&request("/", None, &headers), &files) else { panic!() };
        assert_eq!(function, "/_middleware");
        let Step::Decided(d) = engine.resume(&request("/", None, &headers), &files, paused, Resume::Next) else { panic!() };
        assert_eq!(d.outcome, Outcome::Static { file: "/index.html".into() });
    }
}
