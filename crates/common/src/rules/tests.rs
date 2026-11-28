use std::collections::HashSet;

use serde_json::{Value, json};

use super::*;

/// What Vercel's routing-utils 6.6.1 (`getTransformedRoutes`) makes of a set of `vercel.json`
/// files: generated with Vercel's own library, so the conversion here is checked against it.
const GOLDEN: &str = include_str!("testdata/routing-utils.json");

/// A route in Vercel's shape: headers as an object, without forkrail's `from`.
fn vercel_shape(route: &Route) -> Value {
    let mut v = serde_json::to_value(route).unwrap();
    if let Some(obj) = v.as_object_mut() {
        obj.remove("from");
        if let Some(Value::Array(headers)) = obj.remove("headers") {
            let map: serde_json::Map<String, Value> =
                headers.into_iter().map(|h| (h["key"].as_str().unwrap().to_owned(), h["value"].clone())).collect();
            obj.insert("headers".into(), Value::Object(map));
        }
    }
    v
}

fn program(config: Value) -> Program {
    Program::from_vercel(config.as_object().unwrap(), &Function::default(), None).unwrap().expect("routing keys make a program")
}

#[test]
fn conversion_matches_vercels() {
    let golden: Vec<Value> = serde_json::from_str(GOLDEN).unwrap();
    for case in golden {
        let config = case["config"].clone();
        let got: Vec<Value> = program(config.clone()).routes.iter().map(vercel_shape).collect();
        let want = case["routes"].as_array().unwrap();
        assert_eq!(got.len(), want.len(), "{config}\n got {got:#?}");
        for (g, w) in got.iter().zip(want) {
            assert_eq!(g, w, "{config}");
        }
    }
}

struct Fs(HashSet<&'static str>);

impl Files for Fs {
    fn has(&self, path: &str) -> bool {
        self.0.contains(path)
    }
}

fn fs(paths: &[&'static str]) -> Fs {
    Fs(paths.iter().copied().collect())
}

struct Req {
    method: &'static str,
    path: String,
    query: Option<String>,
    host: &'static str,
    headers: http::HeaderMap,
}

fn get(url: &str) -> Req {
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_owned(), Some(q.to_owned())),
        None => (url.to_owned(), None),
    };
    Req { method: "GET", path, query, host: "shop.example.com", headers: http::HeaderMap::new() }
}

impl Req {
    fn header(mut self, k: &'static str, v: &str) -> Self {
        self.headers.append(k, http::HeaderValue::from_str(v).unwrap());
        self
    }
    fn host(mut self, h: &'static str) -> Self {
        self.host = h;
        self
    }
    fn method(mut self, m: &'static str) -> Self {
        self.method = m;
        self
    }
    fn route(&self, engine: &Engine, files: &dyn Files) -> Decision {
        engine.route(&Request { method: self.method, path: &self.path, query: self.query.as_deref(), host: self.host, headers: &self.headers }, files)
    }
}

fn engine(config: Value) -> Engine {
    Engine::new(&program(config)).unwrap()
}

fn redirect(location: &str, status: u16) -> (Outcome, Option<u16>) {
    (Outcome::Redirect { location: location.into() }, Some(status))
}

fn file(path: &str) -> (Outcome, Option<u16>) {
    (Outcome::Static { file: path.into() }, None)
}

fn outcome(d: &Decision) -> (Outcome, Option<u16>) {
    (d.outcome.clone(), d.status)
}

const SITE: &[&str] = &["/index.html", "/about.html", "/docs/index.html", "/assets/app.js", "/style.css", "/404.html", "/410.html", "/blog.html"];

#[test]
fn folders_and_clean_urls() {
    let plain = Engine::plain(false);
    let site = fs(SITE);
    assert_eq!(outcome(&get("/").route(&plain, &site)), file("/index.html"));
    assert_eq!(outcome(&get("/docs").route(&plain, &site)), file("/docs/index.html"));
    assert_eq!(outcome(&get("/docs/").route(&plain, &site)), file("/docs/index.html"));
    assert_eq!(outcome(&get("/about").route(&plain, &site)), (Outcome::Static { file: "/404.html".into() }, Some(404)));
    assert_eq!(outcome(&get("/nope").route(&plain, &fs(&["/index.html"]))), (Outcome::NotFound, Some(404)));

    let clean = engine(json!({"cleanUrls": true}));
    assert_eq!(outcome(&get("/about").route(&clean, &site)), file("/about.html"));
    assert_eq!(outcome(&get("/about.html?x=1").route(&clean, &site)), redirect("/about?x=1", 308));
    assert_eq!(outcome(&get("/index.html").route(&clean, &site)), redirect("/", 308));
    assert_eq!(outcome(&get("/docs/index").route(&clean, &site)), redirect("/docs", 308));
    assert_eq!(outcome(&get("/docs").route(&clean, &site)), file("/docs/index.html"));

    let both = engine(json!({"cleanUrls": true, "trailingSlash": true}));
    assert_eq!(outcome(&get("/about").route(&both, &site)), redirect("/about/", 308));
    assert_eq!(outcome(&get("/about/").route(&both, &site)), file("/about.html"));
    assert_eq!(outcome(&get("/about.html").route(&both, &site)), redirect("/about/", 308));
    assert_eq!(outcome(&get("/style.css").route(&both, &site)), file("/style.css"));
    assert_eq!(outcome(&get("/style.css/").route(&both, &site)), redirect("/style.css", 308));
    // The ACME challenge and .well-known are left alone.
    assert_eq!(outcome(&get("/.well-known/security.txt").route(&both, &site)).1, Some(404));

    let no_slash = engine(json!({"trailingSlash": false}));
    assert_eq!(outcome(&get("/docs/").route(&no_slash, &site)), redirect("/docs", 308));
    assert_eq!(outcome(&get("/").route(&no_slash, &site)), file("/index.html"));
}

/// The path a file is named after (`content-disposition`), as Vercel names them (its copies,
/// 2026-11-16): the path the filesystem was asked for once the routes rewrote it.
#[test]
fn files_are_named_by_the_path_looked_up() {
    let site = fs(SITE);
    let looked_up = |e: &Engine, url: &str| get(url).route(e, &site).looked_up;
    let plain = Engine::plain(false);
    assert_eq!(looked_up(&plain, "/docs").as_deref(), Some("/docs"));
    assert_eq!(looked_up(&plain, "/docs/").as_deref(), Some("/docs/"));
    assert_eq!(looked_up(&plain, "/style.css").as_deref(), Some("/style.css"));
    // A missing page: the 404 page by its name, which a `cleanUrls` build makes `404`.
    assert_eq!(looked_up(&plain, "/nope").as_deref(), Some("/404.html"));
    let clean = engine(json!({"cleanUrls": true, "rewrites": [{"source": "/static", "destination": "/docs/index.html"}, {"source": "/alias/:p", "destination": "/:p"}]}));
    assert_eq!(looked_up(&clean, "/nope").as_deref(), Some("/404"));
    assert_eq!(looked_up(&clean, "/about").as_deref(), Some("/about"));
    assert_eq!(looked_up(&clean, "/alias/about").as_deref(), Some("/about"));
    let rewritten = engine(json!({"rewrites": [{"source": "/static", "destination": "/docs/index.html"}]}));
    assert_eq!(looked_up(&rewritten, "/static").as_deref(), Some("/docs/index.html"));
    // Nothing for what isn't a file.
    assert_eq!(looked_up(&clean, "/about.html"), None);
}

#[test]
fn redirects() {
    let e = engine(json!({"redirects": [
        {"source": "/old", "destination": "/new"},
        {"source": "/blog/:slug", "destination": "/news/:slug", "permanent": false},
        {"source": "/docs/:path*", "destination": "https://docs.example.com/:path*", "statusCode": 301},
        {"source": "/team", "destination": "/about?tab=team#people"},
        {"source": "/((?!uk/).*)", "destination": "/uk/$1", "has": [{"type": "query", "key": "geo", "value": "uk"}]},
        {"source": "/:path*", "has": [{"type": "host", "value": "(?<sub>.*)\\.example\\.org"}], "destination": "https://example.org/:sub/:path*"},
        {"source": "/legacy", "destination": "/modern", "has": [{"type": "header", "key": "X-Flag", "value": "on|yes"}], "missing": [{"type": "cookie", "key": "beta"}]},
        {"source": "/search", "has": [{"type": "query", "key": "q"}], "destination": "/find?term=:q"}
    ]}));
    let site = fs(SITE);
    assert_eq!(outcome(&get("/old").route(&e, &site)), redirect("/new", 308));
    assert_eq!(outcome(&get("/old?a=1&b=2").route(&e, &site)), redirect("/new?a=1&b=2", 308));
    // Sources are case-sensitive.
    assert_eq!(outcome(&get("/OLD").route(&e, &site)).1, Some(404));
    assert_eq!(outcome(&get("/blog/hello").route(&e, &site)), redirect("/news/hello", 307));
    assert_eq!(outcome(&get("/docs/a/b").route(&e, &site)), redirect("https://docs.example.com/a/b", 301));
    assert_eq!(outcome(&get("/docs").route(&e, &site)), redirect("https://docs.example.com/", 301));
    assert_eq!(outcome(&get("/team?x=1").route(&e, &site)), redirect("/about?x=1&tab=team#people", 308));
    assert_eq!(outcome(&get("/team?tab=other").route(&e, &site)), redirect("/about?tab=team#people", 308));
    // A lookahead, run by fancy-regex.
    assert_eq!(outcome(&get("/shop?geo=uk").route(&e, &site)), redirect("/uk/shop?geo=uk", 308));
    assert_eq!(outcome(&get("/uk/shop?geo=uk").route(&e, &site)).1, Some(404));
    // A host's named group.
    assert_eq!(outcome(&get("/a/b").host("blog.example.org").route(&e, &site)), redirect("https://example.org/blog/a/b", 308));
    // Header values are regexes over the whole value; a missing cookie must be missing.
    assert_eq!(outcome(&get("/legacy").header("x-flag", "yes").route(&e, &site)), redirect("/modern", 308));
    assert_eq!(outcome(&get("/legacy").header("x-flag", "yes!").route(&e, &site)).1, Some(404));
    assert_eq!(outcome(&get("/legacy").header("x-flag", "on").header("cookie", "a=1; beta=1").route(&e, &site)).1, Some(404));
    // A query key with no value is a parameter.
    assert_eq!(outcome(&get("/search?q=shoes").route(&e, &site)), redirect("/find?q=shoes&term=shoes", 308));
}

#[test]
fn rewrites_check_the_filesystem_first() {
    let mut program = program(json!({"rewrites": [
        {"source": "/blog/:slug", "destination": "/blog.html"},
        {"source": "/api/:path*", "destination": "https://api.example.com/:path*"},
        {"source": "/p/:id", "destination": "/api/main.py?id=:id"},
        {"source": "/gone", "destination": "/410.html", "statusCode": 410},
        {"source": "/missing", "destination": "/not-there.html"},
        {"source": "/(.*)", "destination": "/index.html"}
    ]}));
    program.function = function_paths("api/main.py");
    let e = Engine::new(&program).unwrap();
    let site = fs(SITE);
    // Files are served before any rewrite.
    assert_eq!(outcome(&get("/assets/app.js").route(&e, &site)), file("/assets/app.js"));
    assert_eq!(outcome(&get("/about.html").route(&e, &site)), file("/about.html"));
    assert_eq!(outcome(&get("/blog/x").route(&e, &site)), file("/blog.html"));
    assert_eq!(outcome(&get("/api/v1/items?limit=2").route(&e, &site)), (Outcome::External { url: "https://api.example.com/v1/items?limit=2".into() }, None));
    // The function gets the destination's parameters set over the client's, as on Vercel.
    assert_eq!(outcome(&get("/p/42").route(&e, &site)), (Outcome::Function { query: Some("id=42".into()) }, None));
    assert_eq!(outcome(&get("/p/42?id=7&a=b+c").route(&e, &site)), (Outcome::Function { query: Some("id=42&a=b%20c".into()) }, None));
    assert_eq!(outcome(&get("/gone").route(&e, &site)), (Outcome::Static { file: "/410.html".into() }, Some(410)));
    // A rewrite to nothing goes on to the next: the single-page app's catch-all.
    assert_eq!(outcome(&get("/missing").route(&e, &site)), file("/index.html"));
    assert_eq!(outcome(&get("/any/deep/path").route(&e, &site)), file("/index.html"));
}

#[test]
fn headers_apply_to_what_serves() {
    let e = engine(json!({
        "headers": [
            {"source": "/(.*)", "headers": [{"key": "X-Frame-Options", "value": "DENY"}]},
            {"source": "/assets/:file", "headers": [{"key": "Cache-Control", "value": "public, max-age=31536000, immutable"}, {"key": "x-file", "value": ":file"}]},
            {"source": "/:p*", "has": [{"type": "query", "key": "v"}], "headers": [{"key": "x-v", "value": ":v"}]}
        ],
        "redirects": [{"source": "/old", "destination": "/"}]
    }));
    let site = fs(SITE);
    let d = get("/assets/app.js?v=3").route(&e, &site);
    assert_eq!(d.outcome, Outcome::Static { file: "/assets/app.js".into() });
    assert_eq!(
        d.headers,
        vec![
            ("x-frame-options".into(), "DENY".into()),
            ("cache-control".into(), "public, max-age=31536000, immutable".into()),
            ("x-file".into(), "app.js".into()),
            ("x-v".into(), "3".into())
        ]
    );
    // Redirects come before headers, as Vercel orders them.
    let d = get("/old").route(&e, &site);
    assert_eq!(outcome(&d), redirect("/", 308));
    assert!(d.headers.is_empty());
    // A 404 still gets them.
    assert_eq!(get("/nope").route(&e, &site).headers[0].0, "x-frame-options");
}

#[test]
fn legacy_routes() {
    let mut program = program(json!({"routes": [
        {"src": "/old", "status": 301, "headers": {"Location": "/new"}},
        {"src": "/admin(.*)", "status": 403},
        {"src": "/write", "methods": ["POST"], "dest": "/api/main.py"},
        {"src": "/(?<slug>[^/]+)/edit", "dest": "/api/main.py?slug=$slug", "headers": {"X-Edit": "$slug"}},
        {"handle": "filesystem"},
        {"src": "/api/(.*)", "dest": "/api/main.py"},
        {"handle": "error"},
        {"src": "/(.*)", "dest": "/404.html", "status": 404}
    ]}));
    program.function = function_paths("api/main.py");
    assert!(!program.case_sensitive, "routes alone match case-insensitively");
    let e = Engine::new(&program).unwrap();
    let site = fs(SITE);
    assert_eq!(outcome(&get("/OLD").route(&e, &site)), redirect("/new", 301));
    assert_eq!(outcome(&get("/admin/users").route(&e, &site)), (Outcome::Error, Some(403)));
    assert_eq!(outcome(&get("/write").method("POST").route(&e, &site)), (Outcome::Function { query: None }, None));
    assert_eq!(outcome(&get("/write").route(&e, &site)).1, Some(404));
    let d = get("/hello/edit").route(&e, &site);
    assert_eq!(outcome(&d), (Outcome::Function { query: Some("slug=hello".into()) }, None));
    assert_eq!(d.headers, vec![("x-edit".into(), "hello".into())]);
    assert_eq!(outcome(&get("/style.css").route(&e, &site)), file("/style.css"));
    assert_eq!(outcome(&get("/api/anything").route(&e, &site)), (Outcome::Function { query: None }, None));
    // The error phase's page, with its status.
    assert_eq!(outcome(&get("/nothing").route(&e, &site)), (Outcome::Static { file: "/404.html".into() }, Some(404)));
    // An encoded `/` reaches no file past a rule on the folder.
    let guarded = engine(json!({"routes": [{"src": "/private/(.*)", "status": 403}]}));
    let files = fs(&["/private/x.html", "/404.html"]);
    assert_eq!(outcome(&get("/private/x.html").route(&guarded, &files)), (Outcome::Static { file: "/private/x.html".into() }, Some(403)));
    assert_eq!(outcome(&get("/private%2Fx.html").route(&guarded, &files)), (Outcome::Static { file: "/404.html".into() }, Some(404)));
    // Only a challenge's own path skips the routes: one token after the prefix.
    let blocking = engine(json!({"routes": [{"src": "/.well-known/(.*)", "status": 403}]}));
    let files = fs(&["/.well-known/acme-challenge/abc_DEF-1", "/.well-known/acme-challenge/a/b"]);
    assert_eq!(outcome(&get("/.well-known/acme-challenge/abc_DEF-1").route(&blocking, &files)), file("/.well-known/acme-challenge/abc_DEF-1"));
    assert_eq!(outcome(&get("/.well-known/acme-challenge/a/b").route(&blocking, &files)).1, Some(403));
}

#[test]
fn apis_from_vercel_route_as_before() {
    let api = Function { entry: Some("api/main.py".into()), catch_all: false, ..Default::default() };
    // The catch-all most of the APIs have makes no program: every path goes to the function.
    for config in [
        json!({"builds": [{"src": "api/main.py"}], "routes": [{"src": "/(.*)", "dest": "api/main.py"}]}),
        json!({"rewrites": [{"source": "/(.*)", "destination": "/api/main.py"}]}),
        json!({"functions": {"api/main.py": {"memory": 512}}}),
    ] {
        assert_eq!(Program::from_vercel(config.as_object().unwrap(), &api, None).unwrap(), None, "{config}");
    }
    // Anything more makes one, still ending at the function.
    let p = Program::from_vercel(json!({"redirects": [{"source": "/v1/:p*", "destination": "/v2/:p*"}]}).as_object().unwrap(), &api, None).unwrap().unwrap();
    assert!(p.fallback);
    let e = Engine::new(&p).unwrap();
    assert_eq!(outcome(&get("/v1/x").route(&e, &fs(&[]))), redirect("/v2/x", 308));
    assert_eq!(outcome(&get("/search").route(&e, &fs(&[]))), (Outcome::Function { query: None }, None));
    // With rewrites, only what they send to the function reaches it, as on Vercel.
    let p = Program::from_vercel(json!({"rewrites": [{"source": "/api/(.*)", "destination": "/api/main.py"}]}).as_object().unwrap(), &api, None).unwrap().unwrap();
    let e = Engine::new(&p).unwrap();
    assert_eq!(outcome(&get("/api/items").route(&e, &fs(&[]))), (Outcome::Function { query: None }, None));
    assert_eq!(outcome(&get("/elsewhere").route(&e, &fs(&[]))), (Outcome::NotFound, Some(404)));
    // A framework's server takes whatever nothing else does.
    let nitro = Function { entry: None, catch_all: true, ..Default::default() };
    let p = Program::from_vercel(json!({"rewrites": [{"source": "/old", "destination": "/new"}]}).as_object().unwrap(), &nitro, None).unwrap().unwrap();
    let e = Engine::new(&p).unwrap();
    assert_eq!(outcome(&get("/old").route(&e, &fs(&[]))), (Outcome::Function { query: None }, None));
}

#[test]
fn per_file_functions_route_as_vercels_zero_config() {
    let files = ["api/index.go", "api/hello.go", "api/users/[id].go", "api/users/index.go"].map(String::from).to_vec();
    let function = Function { entry: None, catch_all: false, files: files.clone() };
    // No vercel.json still makes a program: only the functions' paths reach the function.
    let p = Program::from_vercel(&serde_json::Map::new(), &function, None).unwrap().unwrap();
    assert!(!p.fallback);
    let e = Engine::new(&p).unwrap();
    let none = fs(&[]);
    let reached = |path: &str| {
        let d = get(path).route(&e, &none);
        (d.outcome, d.function)
    };
    assert_eq!(reached("/api/hello"), (Outcome::Function { query: None }, Some("/api/hello".into())));
    assert_eq!(reached("/api/hello.go"), (Outcome::Function { query: None }, Some("/api/hello.go".into())));
    assert_eq!(reached("/api"), (Outcome::Function { query: None }, Some("/api".into())));
    assert_eq!(reached("/api/users"), (Outcome::Function { query: None }, Some("/api/users".into())));
    // A dynamic segment goes in the query, as Vercel's route puts it.
    assert_eq!(reached("/api/users/42"), (Outcome::Function { query: Some("id=42".into()) }, Some("/api/users/[id]".into())));
    // The rest of /api isn't found, whatever a site's catch-all would do.
    assert_eq!(get("/api/nope").route(&e, &none).outcome, Outcome::Error);
    assert_eq!(get("/api/nope").route(&e, &none).status, Some(404));
    assert_eq!(get("/elsewhere").route(&e, &none).outcome, Outcome::NotFound);
    // Beside a single-page app whose preset sends every path to index.html, /api still isn't.
    let spa = json!([{"handle": "filesystem"}, {"src": "/(.*)", "dest": "/index.html"}]);
    let p = Program::from_vercel(&serde_json::Map::new(), &function, Some(("Create React App", &spa))).unwrap().unwrap();
    let e = Engine::new(&p).unwrap();
    let site = fs(&["/index.html"]);
    assert_eq!(get("/dashboard").route(&e, &site).outcome, Outcome::Static { file: "/index.html".into() });
    assert_eq!(get("/api/nope").route(&e, &site).status, Some(404));
    assert_eq!(get("/api/users/7").route(&e, &site).function.as_deref(), Some("/api/users/[id]"));
    // A rewrite to a function says which.
    let config = json!({"rewrites": [{"source": "/(.*)", "destination": "/api/index"}]});
    let p = Program::from_vercel(config.as_object().unwrap(), &function, None).unwrap().unwrap();
    let e = Engine::new(&p).unwrap();
    let d = get("/anything/at/all").route(&e, &none);
    assert_eq!((d.outcome, d.function.as_deref()), (Outcome::Function { query: None }, Some("/api/index")));
}

#[test]
fn miss_and_hit_phases() {
    let e = engine(json!({"routes": [
        {"handle": "filesystem"},
        {"src": "/docs/(.*)", "dest": "/docs/$1.html", "check": true},
        {"handle": "miss"},
        {"src": "/guide/(.*)", "dest": "/docs/index.html", "check": true},
        {"handle": "hit"},
        {"src": "/(.*)", "headers": {"x-hit": "1"}, "continue": true}
    ]}));
    let site = fs(&["/docs/index.html", "/docs/intro.html"]);
    let d = get("/docs/intro").route(&e, &site);
    assert_eq!(outcome(&d), file("/docs/intro.html"));
    assert_eq!(d.headers, vec![("x-hit".into(), "1".into())]);
    // Miss routes run whenever the filesystem doesn't have the path, as in Vercel's router.
    assert_eq!(outcome(&get("/guide/start").route(&e, &site)), file("/docs/index.html"));
    assert_eq!(outcome(&get("/docs/other").route(&e, &site)).1, Some(404));
}

#[test]
fn mistakes_are_refused_before_deploying() {
    let refused = |config: Value, expect: &str| {
        let err = Program::from_vercel(config.as_object().unwrap(), &Function::default(), None).unwrap_err();
        assert!(err.contains(expect), "{config}: {err}");
    };
    refused(json!({"routes": [], "redirects": []}), "can't be used with `redirects`");
    refused(json!({"redirects": [{"source": "/a/:x", "destination": "/b/:y"}]}), "\":y\"");
    refused(json!({"redirects": [{"source": "/a", "destination": "/b", "permanent": true, "statusCode": 301}]}), "both");
    refused(json!({"redirects": [{"source": "/a", "destination": "/b", "statusCode": 200}]}), "301, 302");
    refused(json!({"redirects": [{"source": "/(a", "destination": "/b"}]}), "isn't a valid pattern");
    refused(json!({"redirects": [{"source": "/a", "destination": "/b", "env": ["X"]}]}), "isn't supported yet");
    refused(json!({"rewrites": [{"source": "/a", "destination": "/b", "transforms": []}]}), "non-empty array");
    refused(json!({"rewrites": [{"source": "/a", "destination": "/b", "transforms": [{"type": "request.headers", "op": "delete", "target": {"key": "x"}}]}]}), "only `request.path`");
    refused(json!({"rewrites": [{"source": "/a/:x", "destination": "/b", "transforms": [{"type": "request.path", "op": "set", "args": "/c/:y"}]}]}), "\":y\"");
    refused(json!({"routes": [{"src": "/a", "transforms": [{"type": "request.query", "op": "set", "target": {"key": "x"}}]}]}), "needs `args`");
    refused(json!({"routes": [{"src": "/a", "transforms": [{"type": "request.headers", "op": "set", "target": {"key": "x y"}, "args": "1"}]}]}), "letters, digits");
    refused(json!({"routes": [{"src": "/a", "transforms": [{"type": "request.path", "op": "append", "args": "/b"}]}]}), "only `set`");
    refused(json!({"routes": [{"src": "/a", "transforms": [{"type": "request.path", "op": "set", "args": "/b?c"}]}]}), "no query");
    refused(json!({"routes": [{"src": "/a", "transforms": [{"type": "request.headers", "op": "delete", "target": {"key": {"re": "x"}}}]}]}), "`re`");
    refused(json!({"routes": [{"src": "/a", "transforms": [{"type": "request.headers", "op": "set", "target": {"key": "x"}, "args": "$K", "env": ["K"]}]}]}), "secrets");
    refused(json!({"rewrites": [{"source": "/a"}]}), "needs `destination`");
    refused(json!({"headers": [{"source": "/a", "headers": [{"key": "x"}]}]}), "needs `value`");
    refused(json!({"redirects": [{"source": "/a", "destination": "/b", "has": [{"type": "host"}]}]}), "needs `value`");
    refused(json!({"redirects": [{"source": "/a", "destination": "/b", "has": [{"type": "query"}]}]}), "needs `key`");
    refused(json!({"cleanUrls": "yes"}), "true or false");
    refused(json!({"routes": [{"handle": "filesystem"}, {"handle": "filesystem"}]}), "at most once");
    refused(json!({"routes": [{"handle": "rewrite"}]}), "isn't supported yet");
    refused(json!({"routes": [{"src": "/a", "middlewarePath": "m", "dest": "/b"}]}), "both `middlewarePath` and `dest`");
    refused(json!({"routes": [{"handle": "filesystem"}, {"src": "/a", "middlewarePath": "m"}]}), "Routing Middleware runs before the filesystem");
    refused(json!({"routes": [{"src": "/a(?<x"}]}), "isn't one forkrail can run");
    refused(json!({"routes": [{"handle": "hit"}, {"src": "/a", "dest": "/b"}]}), "may only add headers");
    let many: Vec<Value> = (0..2049).map(|i| json!({"source": format!("/{i}"), "destination": "/"})).collect();
    refused(json!({"redirects": many}), "2048");
    refused(json!({"redirects": [{"source": format!("/{}", "a".repeat(4096)), "destination": "/"}]}), "4096");
}

#[test]
fn function_paths_are_vercels() {
    assert_eq!(function_paths("api/main.py"), vec!["/api/main.py", "/api/main"]);
    assert_eq!(function_paths("api/index.js"), vec!["/api/index.js", "/api/index", "/api"]);
    assert_eq!(function_paths("server.mjs"), vec!["/server.mjs", "/server"]);
    assert_eq!(file_paths("api/index.ts"), vec!["/api/index.ts", "/api/index", "/api", "/api/"]);
    assert_eq!(file_paths("api/users/index.py"), vec!["/api/users/index.py", "/api/users/index", "/api/users", "/api/users/"]);
    assert_eq!(file_paths("api/users.go"), vec!["/api/users.go", "/api/users"]);
}

#[test]
fn a_program_round_trips_through_json() {
    let p = program(json!({"cleanUrls": true, "redirects": [{"source": "/a", "destination": "/b", "has": [{"type": "cookie", "key": "c", "value": {"pre": "x", "gt": 2}}]}]}));
    let text = serde_json::to_string(&p).unwrap();
    assert_eq!(serde_json::from_str::<Program>(&text).unwrap(), p);
}

#[test]
fn comparisons_in_conditions() {
    let e = engine(json!({"redirects": [
        {"source": "/v", "destination": "/new", "has": [{"type": "query", "key": "n", "value": {"gte": 2, "lt": 5}}]},
        {"source": "/v", "destination": "/listed", "has": [{"type": "cookie", "key": "plan", "value": {"inc": ["pro", "team"]}}]},
        {"source": "/v", "destination": "/pre", "has": [{"type": "header", "key": "user-agent", "value": {"pre": "curl/", "re": "curl/(?<ver>\\d+)"}}]}
    ]}));
    let none = fs(&[]);
    assert_eq!(outcome(&get("/v?n=3").route(&e, &none)), redirect("/new?n=3", 308));
    assert_eq!(outcome(&get("/v?n=5").route(&e, &none)).1, Some(404));
    assert_eq!(outcome(&get("/v").header("cookie", "plan=team").route(&e, &none)), redirect("/listed", 308));
    assert_eq!(outcome(&get("/v").header("user-agent", "curl/8.5").route(&e, &none)), redirect("/pre", 308));
}

#[test]
fn the_prefilter_changes_nothing() {
    // Enough routes in each phase to be prefiltered, with what the set can't hold (a lookahead),
    // continues that change the path, case-insensitive routes, and has conditions.
    let mut routes: Vec<Value> = vec![
        json!({"src": "/Start", "dest": "/moved", "continue": true}),
        json!({"src": "/moved", "headers": {"x-moved": "1"}, "continue": true}),
        json!({"src": "/((?!skip/).*)/lookahead", "headers": {"Location": "/$1"}, "status": 308}),
        json!({"src": "/gated", "has": [{"type": "query", "key": "ok"}], "dest": "/index.html"}),
    ];
    for i in 0..60 {
        routes.push(json!({"src": format!("/r{i}/(.*)"), "headers": {"Location": format!("/n{i}/$1")}, "status": 307}));
        routes.push(json!({"src": format!("/h{i}"), "headers": {"x-h": format!("{i}")}, "continue": true}));
    }
    routes.push(json!({"handle": "filesystem"}));
    for i in 0..40 {
        routes.push(json!({"src": format!("/f{i}/(.*)"), "dest": "/about.html", "check": true}));
    }
    routes.push(json!({"src": "/(.*)", "dest": "/index.html", "check": true}));
    let e = engine(json!({"routes": routes}));
    let plain = engine(json!({"routes": routes})).without_prefilter();
    let site = fs(SITE);
    for url in ["/start", "/moved", "/x/lookahead", "/skip/x/lookahead", "/gated?ok=1", "/gated", "/r0/a", "/R59/b", "/h3", "/f12/x", "/f39/y", "/style.css", "/nothing", "/"] {
        assert_eq!(get(url).route(&e, &site), get(url).route(&plain, &site), "{url}");
    }
}

/// What routing costs a request: `cargo test --release -p forkrail-common --features rules cost -- --ignored --nocapture`.
#[test]
#[ignore]
fn cost_of_routing() {
    let time = |name: &str, engine: &Engine, url: &str| {
        let req = get(url);
        let files = fs(SITE);
        let n = 20_000;
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(req.route(engine, &files));
        }
        println!("{name:<44} {url:<24} {:>8.2} µs", start.elapsed().as_secs_f64() * 1e6 / n as f64);
    };
    let plain = Engine::plain(true);
    time("no rules", &plain, "/assets/app.js");
    let typical = engine(json!({"cleanUrls": true, "redirects": [{"source": "/old", "destination": "/new"}, {"source": "/blog/:slug", "destination": "/news/:slug"}],
        "headers": [{"source": "/(.*)", "headers": [{"key": "x-frame-options", "value": "DENY"}]}], "rewrites": [{"source": "/(.*)", "destination": "/index.html"}]}));
    time("a typical site (5 rules)", &typical, "/some/page");
    time("a typical site (5 rules)", &typical, "/assets/app.js");
    // What every function request with a query now pays, the 35 APIs' included.
    for q in ["location=New+York%2C+NY&status=for_sale&limit=42&offset=0", "id=2061534107", "url=https%3A%2F%2Fwww.example.org%2Fitems%2F123%2F&a=b"] {
        let n = 200_000;
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(normalize_query(std::hint::black_box(q)));
        }
        println!("{:<44} {:<24.24} {:>8.2} µs", "a query rebuilt", q, start.elapsed().as_secs_f64() * 1e6 / n as f64);
    }
    for n in [100, 2048] {
        let redirects: Vec<Value> = (0..n).map(|i| json!({"source": format!("/r{i}/:slug"), "destination": format!("/new{i}/:slug")})).collect();
        let e = engine(json!({"redirects": redirects}));
        time(&format!("{n} redirects, none matching"), &e, "/nomatch/x");
        time(&format!("{n} redirects, the last matching"), &e, &format!("/r{}/x", n - 1));
    }
}

/// What `tools/routing-check.py` found Vercel doing, request by request (2026-11-13).
#[test]
fn as_vercel_answers() {
    let e = engine(json!({
        "cleanUrls": true,
        "trailingSlash": false,
        "redirects": [
            {"source": "/q", "destination": "/about?from=q", "permanent": false},
            {"source": "/old/:p", "destination": "/new/:p"},
            {"source": "/hv", "has": [{"type": "header", "key": "x-t", "value": "(?<v>yes|no)"}], "destination": "/about?v=:v", "permanent": false},
            {"source": "/cv", "has": [{"type": "cookie", "key": "c", "value": "abc"}], "destination": "/about", "permanent": false},
            {"source": "/qv", "has": [{"type": "query", "key": "k", "value": "abc"}], "destination": "/about", "permanent": false},
            {"source": "/pre", "has": [{"type": "header", "key": "x-p", "value": {"pre": "ab"}}], "destination": "/about", "permanent": false},
            {"source": "/eq", "has": [{"type": "header", "key": "x-e", "value": {"eq": "abc"}}], "destination": "/about", "permanent": false}
        ],
        "rewrites": [
            {"source": "/blog/:slug", "destination": "/post.html"},
            {"source": "/clean/:slug", "destination": "/post"},
            {"source": "/gone", "destination": "/404.html", "statusCode": 410}
        ]
    }));
    let site = fs(&["/index.html", "/about.html", "/post.html", "/404.html"]);
    // The client's query first, the destination's set over it, all rebuilt the same way.
    assert_eq!(outcome(&get("/q?x=1").route(&e, &site)), redirect("/about?x=1&from=q", 307));
    assert_eq!(outcome(&get("/q?from=z&x=1").route(&e, &site)), redirect("/about?from=q&x=1", 307));
    assert_eq!(outcome(&get("/q?a=1&from=z&from=y").route(&e, &site)), redirect("/about?a=1&from=q", 307));
    assert_eq!(outcome(&get("/q?b=1&a=2&b=3").route(&e, &site)), redirect("/about?b=1&a=2&b=3&from=q", 307));
    assert_eq!(outcome(&get("/q?x&y=").route(&e, &site)), redirect("/about?x&y=&from=q", 307));
    assert_eq!(outcome(&get("/old/x?y=a+b&z=%7e&w=%2F&bad=%ZZ&bin=%FF").route(&e, &site)), redirect("/new/x?y=a%20b&z=~&w=%2F&bad=ZZ&bin=%FF", 308));
    // Repeated slashes go to the path with single ones, before any rule.
    assert_eq!(outcome(&get("/old//x?a=1").route(&e, &site)), redirect("/old/x?a=1", 308));
    assert_eq!(outcome(&get("/about//").route(&e, &site)), redirect("/about/", 308));
    // An encoded `..` is refused.
    assert_eq!(outcome(&get("/docs/%2e%2e/about").route(&e, &site)), (Outcome::Error, Some(403)));
    assert_eq!(outcome(&get("/%2e%2e/about").route(&e, &site)), (Outcome::Error, Some(400)));
    // Values are compared without regard to case; what they capture keeps it. Cookie names too,
    // query keys not.
    assert_eq!(outcome(&get("/hv").header("x-t", "YES").route(&e, &site)), redirect("/about?v=YES", 307));
    assert_eq!(outcome(&get("/cv").header("cookie", "C=ABC").route(&e, &site)), redirect("/about", 307));
    assert_eq!(outcome(&get("/qv?k=ABC").route(&e, &site)), redirect("/about?k=ABC", 307));
    assert_eq!(outcome(&get("/qv?K=abc").route(&e, &site)).1, Some(404));
    for (path, name, value) in [("/pre", "x-p", "ABc"), ("/eq", "x-e", "ABC")] {
        assert_eq!(outcome(&get(path).header(name, value).route(&e, &site)), redirect("/about", 307), "{path}");
    }
    // With cleanUrls an .html file is known by its clean name only.
    assert_eq!(outcome(&get("/blog/x").route(&e, &site)), (Outcome::Static { file: "/404.html".into() }, Some(404)));
    assert_eq!(outcome(&get("/clean/x").route(&e, &site)), file("/post.html"));
    // A rewrite with a status whose destination isn't there ends with that status.
    assert_eq!(outcome(&get("/gone").route(&e, &site)), (Outcome::Error, Some(410)));
}

#[test]
fn a_presets_routes_go_around_the_projects() {
    let angular = json!([{"handle": "filesystem"}, {"src": "/(.*)", "dest": "/index.html"}]);
    let config = json!({"rewrites": [{"source": "/blog/:slug", "destination": "/post.html"}], "headers": [{"source": "/(.*)", "headers": [{"key": "x-a", "value": "1"}]}]});
    let p = Program::from_vercel(config.as_object().unwrap(), &Function::default(), Some(("Angular", &angular))).unwrap().unwrap();
    let shapes: Vec<String> = p.routes.iter().map(|r| match r {
        Route::Handle { handle } => format!("handle {}", json_name(*handle)),
        Route::Source(s) => s.src.clone(),
    }).collect();
    assert_eq!(shapes, ["^(?:/(.*))$", "handle filesystem", "^/blog(?:/([^/]+?))$", "^/(.*)$"]);
    let e = Engine::new(&p).unwrap();
    let site = fs(&["/index.html", "/post.html", "/main.js"]);
    assert_eq!(outcome(&get("/main.js").route(&e, &site)), file("/main.js"));
    assert_eq!(outcome(&get("/blog/x").route(&e, &site)), file("/post.html"));
    assert_eq!(outcome(&get("/Deep/Link").route(&e, &site)), file("/index.html"));
    // With no vercel.json, the preset's alone.
    let p = Program::from_vercel(&serde_json::Map::new(), &Function::default(), Some(("Angular", &angular))).unwrap().unwrap();
    let e = Engine::new(&p).unwrap();
    assert_eq!(outcome(&get("/deep/link").route(&e, &site)), file("/index.html"));
}

#[test]
fn transforms_change_the_request_and_answer() {
    use super::transform::{apply_headers, apply_query, request_path};
    let e = engine(json!({"routes": [
        {"src": "/articles/(?<id>[^/]+)", "continue": true, "transforms": [
            {"type": "request.path", "op": "set", "args": "/posts/$id"},
            {"type": "request.headers", "op": "set", "target": {"key": "x-id"}, "args": "$id"},
        ]},
        {"src": "/(.*)", "continue": true, "transforms": [
            {"type": "request.headers", "op": "delete", "target": {"key": {"pre": "x-react-"}}},
            {"type": "request.query", "op": "set", "target": {"key": "theme"}, "args": "dark"},
            {"type": "request.query", "op": "append", "target": {"key": "tag"}, "args": ["a", "b"]},
            {"type": "request.query", "op": "delete", "target": {"key": "drop"}},
            {"type": "response.headers", "op": "append", "target": {"key": "vary"}, "args": "accept"},
            {"type": "response.headers", "op": "delete", "target": {"key": "x-powered-by"}},
        ]},
    ]}));
    let req = get("/articles/7?theme=light&drop=1&tag=z").header("x-react-a", "1").header("x-keep", "2");
    let d = req.route(&e, &fs(&[]));
    assert_eq!(request_path(&d.transforms), Some("/posts/7"));
    let mut headers = req.headers.clone();
    apply_headers(&d.transforms, TransformKind::RequestHeaders, &mut headers);
    assert!(headers.get("x-react-a").is_none());
    assert_eq!(headers["x-keep"], "2");
    assert_eq!(headers["x-id"], "7");
    assert_eq!(apply_query(&d.transforms, "theme=light&drop=1&tag=z").as_deref(), Some("theme=dark&tag=z&tag=a&tag=b"));
    let mut answer = http::HeaderMap::new();
    answer.insert("x-powered-by", "me".parse().unwrap());
    answer.insert("vary", "origin".parse().unwrap());
    apply_headers(&d.transforms, TransformKind::ResponseHeaders, &mut answer);
    assert!(answer.get("x-powered-by").is_none());
    assert_eq!(answer.get_all("vary").iter().collect::<Vec<_>>(), ["origin", "accept"]);

    // The headers the router sets on a request stay as it made them: no loop mark taken off, no
    // client address or schedule run made up.
    let e = engine(json!({"routes": [
        {"src": "/(.*)", "continue": true, "transforms": [
            {"type": "request.headers", "op": "delete", "target": {"key": {"pre": "x-"}}},
            {"type": "request.headers", "op": "set", "target": {"key": "x-forwarded-for"}, "args": "10.0.0.1"},
            {"type": "request.headers", "op": "set", "target": {"key": "X-Forkrail-Schedule"}, "args": "1"},
            {"type": "request.headers", "op": "append", "target": {"key": "x-vercel-ip-country"}, "args": "US"},
            {"type": "request.headers", "op": "set", "target": {"key": "forwarded"}, "args": "for=10.0.0.1"},
        ]},
    ]}));
    let req = get("/x").header("x-forkrail-loop", "1").header("x-forwarded-for", "203.0.113.9").header("x-mine", "1");
    let d = req.route(&e, &fs(&[]));
    let mut headers = req.headers.clone();
    apply_headers(&d.transforms, TransformKind::RequestHeaders, &mut headers);
    assert_eq!(headers["x-forkrail-loop"], "1");
    assert_eq!(headers.get_all("x-forwarded-for").iter().collect::<Vec<_>>(), ["203.0.113.9"]);
    assert!(headers.get("x-forkrail-schedule").is_none() && headers.get("x-vercel-ip-country").is_none() && headers.get("forwarded").is_none());
    assert!(headers.get("x-mine").is_none());

    // A query transform changes the query as it stood when its route matched: SvelteKit's first
    // route deletes a client's `__pathname`, and a later destination sets its own (Vercel's order).
    let mut p = program(json!({"routes": [
        {"src": ".*", "continue": true, "transforms": [{"type": "request.query", "op": "delete", "target": {"key": "__pathname"}}]},
        {"src": "^(/isr/([^/]+?)/?)$", "dest": "/isr/[slug]?__pathname=$1"},
    ]}));
    p.function = vec!["/isr/[slug]".to_owned()];
    let e = Engine::new(&p).unwrap();
    let d = get("/isr/one?v=1&__pathname=/evil").route(&e, &fs(&[]));
    assert_eq!(d.outcome, Outcome::Function { query: Some("v=1&__pathname=%2Fisr%2Fone".into()) });
    assert!(d.transforms.is_empty(), "applied already, so not again");
    assert_eq!(d.captures.as_deref().map(|c| c[0].clone()), Some(("1".to_owned(), "/isr/one".to_owned())));

    // In `rewrites`, a `request.path` transform's parameters are the source's.
    let r = engine(json!({"rewrites": [{"source": "/blog/:slug", "destination": "/api/main.py", "transforms": [{"type": "request.path", "op": "set", "args": "/posts/:slug"}]}]}));
    let d = get("/blog/hello").route(&r, &fs(&["/api/main.py"]));
    assert_eq!(request_path(&d.transforms), Some("/posts/hello"));
}

/// Vercel's route `mitigate` (routing-utils' schema: `{"action": "challenge" | "deny"}`): routing
/// stops at the route for the router to challenge or deny; a passed challenge goes on.
#[test]
fn mitigate_routes_stop_for_the_firewall() {
    let e = engine(json!({"routes": [
        {"src": "/admin(.*)", "mitigate": {"action": "deny"}},
        {"src": "/(.*)", "has": [{"type": "header", "key": "x-bot"}], "mitigate": {"action": "challenge"}, "headers": {"x-challenged": "1"}, "continue": true},
        {"src": "/old", "status": 308, "headers": {"Location": "/new"}}
    ]}));
    let site = fs(SITE);
    let step = |req: &Req| e.step(&Request { method: req.method, path: &req.path, query: req.query.as_deref(), host: req.host, headers: &req.headers }, &site);
    // Deny: the router answers 403; `route` says so too.
    assert!(matches!(step(&get("/admin/x")), Step::Mitigate { action: MitigateAction::Deny, .. }));
    assert_eq!(outcome(&get("/admin/x").route(&e, &site)), (Outcome::Error, Some(403)));
    // Challenge: only where its condition holds; once passed, the routes after it go on, with its
    // headers.
    let bot = get("/old").header("x-bot", "1");
    let Step::Mitigate { action: MitigateAction::Challenge, paused, .. } = step(&bot) else { panic!("no challenge") };
    let req = Request { method: bot.method, path: &bot.path, query: None, host: bot.host, headers: &bot.headers };
    let Step::Decided(d) = e.resume(&req, &site, paused, Resume::Next) else { panic!("decided") };
    assert_eq!(outcome(&d), redirect("/new", 308));
    assert!(d.headers.contains(&("x-challenged".into(), "1".into())));
    assert!(matches!(step(&get("/old")), Step::Decided(_)), "no x-bot, no challenge");
    // Its shape is checked, and it round-trips through the program's JSON.
    let program = program(json!({"routes": [{"src": "/a", "mitigate": {"action": "deny"}}]}));
    let back: Program = serde_json::from_str(&serde_json::to_string(&program).unwrap()).unwrap();
    assert_eq!(back, program);
    for bad in [json!({"action": "block"}), json!("deny"), json!({"action": "deny", "x": 1})] {
        let err = Program::from_vercel(json!({"routes": [{"src": "/a", "mitigate": bad}]}).as_object().unwrap(), &Function::default(), None).unwrap_err();
        assert!(err.contains("mitigate"), "{err}");
    }
}

#[test]
fn nitro_isr_routes_name_the_path_by_its_group() {
    // Nitro's ISR routes (its `vercel` preset, docs/data/c16/fw-vite-nitro and fw-nuxt): the path
    // in a group named `__isr_route`, referred to as `$__isr_route`, which Vercel fills in (seen:
    // the function gets `?v=1`, Nitro having taken its `__isr_route` off).
    let routes = json!([
        {"handle": "filesystem"},
        {"src": "^(?<__isr_route>\\/isr-query\\/?)$", "dest": "/isr-query-isr?__isr_route=$__isr_route"},
        {"src": "(?<__isr_route>/isr/(?:.*))", "dest": "/isr/[...]-isr?__isr_route=$__isr_route"},
        {"src": "/(.*)", "dest": "/__server"}
    ]);
    let functions = ["/isr-query-isr", "/isr/[...]-isr", "/__server"].iter().map(|s| s.to_string()).collect();
    let program = Program::from_build_output(&serde_json::Map::new(), "Nitro", &routes, functions, Default::default(), Default::default()).unwrap();
    let engine = Engine::new(&program).unwrap();
    let d = get("/isr-query?v=1&other=2").route(&engine, &fs(&[]));
    assert_eq!(d.function.as_deref(), Some("/isr-query-isr"));
    let Outcome::Function { query: Some(query) } = &d.outcome else { panic!("{:?}", d.outcome) };
    assert!(query.split('&').any(|p| p == "__isr_route=%2Fisr-query"), "{query}");
    // Nitro 2 writes its routes unanchored; they match the whole path all the same.
    let d = get("/isr/one").route(&engine, &fs(&[]));
    assert_eq!(d.function.as_deref(), Some("/isr/[...]-isr"));
    let names: Vec<&str> = d.captures.iter().flatten().map(|(k, _)| k.as_str()).collect();
    assert!(names.contains(&"__isr_route"), "{names:?}");
    assert_eq!(get("/nope/isr/one").route(&engine, &fs(&[])).function.as_deref(), Some("/__server"));
    // A reference that names no group is read as before: `$1`, letters, and nothing for none.
    assert_eq!(super::engine::resolve("/a/$1_b/$zz", &[Some("/x".into()), Some("x".into())], &[None, None], &[]), "/a/x_b/");
    assert_eq!(super::engine::resolve("$_x", &[None], &[None], &[]), "$_x");
}
