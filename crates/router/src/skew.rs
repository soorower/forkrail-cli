//! Skew protection (Vercel's, `/docs/skew-protection`, read 2026-11-16): a request that names one of
//! its project's deployments is served by that deployment rather than the one its host points at,
//! so a page keeps getting its own version's chunks, data and actions after a newer deployment
//! goes live. Frameworks name the deployment that served the page: Next.js and Astro with `?dpl=`
//! or `x-deployment-id` on the requests they make, SvelteKit, Nuxt, Qwik and React Router with a
//! `__vdpl` cookie their routes set on page loads.
//!
//! The rules, in order (`decide`), for a project with skew protection on:
//! - the id must be one of the project's deployments still in the routing table (a deleted one,
//!   another project's or a made-up one is ignored: the current deployment serves);
//! - a page load (`sec-fetch-dest: document`) is never pinned, whatever names the deployment, so a
//!   refresh gets the newest version;
//! - another site's request is pinned only if the project allows that site;
//! - a production name (`<project>.<base>`, a production domain) is pinned only to production
//!   deployments: standard protection leaves those names open and guards previews, and a preview
//!   must not be reachable through them (forkrail's own rule; Vercel says nothing of it);
//! - elsewhere (a deployment's own address, a branch's), only to a deployment the visitor could
//!   reach at its own address: deployment protection's sign-ins, share links and exceptions each
//!   open one host, and a pin mustn't carry them to another deployment (`Gates::pin_reaches`;
//!   checked by the registry, which has the gates);
//! - nothing created at or before the project's threshold;
//! - nothing older than the maximum age (12 hours by default), from the deployment's creation; 60
//!   days for Googlebot and Bingbot, when they come from the addresses Google and Bing publish.
//!
//! As on Vercel, a `dpl` naming one of the project's deployments is taken out of the query before
//! routing and the app see it; one naming nothing stays (checked on Vercel 2026-11-16).

use forkrail_common::routes::{Deployment, SKEW_CRAWLER_MAX_AGE_S, SkewProtection, normalize_host, wildcard_above};
use forkrail_common::rules::convert::query_unescape;
use hyper::HeaderMap;
use hyper::header::{COOKIE, ORIGIN, REFERER, USER_AGENT};

/// Vercel's names for a deployment id in a request: a query parameter, a header, a cookie.
pub const QUERY: &str = "dpl";
pub const HEADER: &str = "x-deployment-id";
pub const COOKIE_NAME: &str = "__vdpl";

/// Where a request named its deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Query,
    Header,
    Cookie,
}

/// What skew protection made of a request that named a deployment, for the access log's `skew`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Served by the deployment it named, not the one its host points at.
    Pinned,
    /// It named the deployment its host points at: nothing to do.
    Current,
    /// A page load, which the current deployment always serves.
    Document,
    /// From another site the project doesn't allow.
    CrossSite,
    /// A preview, named on a production name.
    Environment,
    /// Another deployment, which the host's protection doesn't open.
    Protected,
    /// At or before the project's threshold.
    Threshold,
    /// Older than the maximum age, or of unknown age (an entry from before ages were recorded).
    Expired,
    /// None of the project's deployments has that id.
    Unknown,
}

impl Outcome {
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Pinned => "pinned",
            Outcome::Current => "current",
            Outcome::Document => "document",
            Outcome::CrossSite => "cross_site",
            Outcome::Environment => "environment",
            Outcome::Protected => "protected",
            Outcome::Threshold => "threshold",
            Outcome::Expired => "expired",
            Outcome::Unknown => "unknown",
        }
    }
}

/// The deployment a request names, and where: its query's `dpl`, else `x-deployment-id`, else the
/// `__vdpl` cookie. The explicit ways come first, since one cookie serves every tab of a browser
/// (the newest page load's). A query with `dpl` twice names nothing by it: Vercel left such a query
/// as it was (2026-11-16), so it didn't take either.
pub fn requested(query: Option<&str>, headers: &HeaderMap) -> Option<(String, Source)> {
    if let Some(id) = query.and_then(query_id) {
        return Some((id, Source::Query));
    }
    if let Some(id) = headers.get(HEADER).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty()) {
        return Some((id.to_owned(), Source::Header));
    }
    cookie(headers).map(|id| (id, Source::Cookie))
}

/// The value of the query's only `dpl`, decoded.
fn query_id(query: &str) -> Option<String> {
    let mut values = query.split('&').filter_map(|pair| pair.strip_prefix(QUERY)?.strip_prefix('='));
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    Some(query_unescape(first)).filter(|v| !v.is_empty())
}

/// The `__vdpl` cookie's value, when the request has one. Two that differ name nothing: another
/// app under the same domain can set one for the whole domain beside the app's own, and a browser
/// sends both, so neither can be told for the app's.
fn cookie(headers: &HeaderMap) -> Option<String> {
    let mut values = headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().strip_prefix(COOKIE_NAME)?.strip_prefix('='))
        .map(|v| v.trim().trim_matches('"'))
        .filter(|v| !v.is_empty());
    let first = values.next()?;
    values.all(|v| v == first).then(|| first.to_owned())
}

/// `query` without its `dpl`, every other parameter as it was; None when nothing is left.
pub fn strip(query: &str) -> Option<String> {
    let kept: Vec<&str> = query.split('&').filter(|pair| !pair.strip_prefix(QUERY).is_some_and(|rest| rest.starts_with('='))).collect();
    let kept = kept.join("&");
    Some(kept).filter(|q| !q.is_empty())
}

/// What the rules read of a request.
pub struct Ask<'a> {
    /// Normalised.
    pub host: &'a str,
    pub headers: &'a HeaderMap,
    /// Unix seconds.
    pub now: u64,
    /// Whether `host` is one of the project's production names (`protection::production_name`).
    pub production_host: bool,
    /// Whether the client is a search engine's crawler, by the addresses its operator publishes.
    pub verified_crawler: bool,
}

/// Whether a request naming deployment `id` is served by it, where its host points at `current`.
/// `candidate` is the deployment of that id when it's one of the project's.
pub fn decide(setting: &SkewProtection, ask: &Ask, current: &str, id: &str, candidate: Option<&Deployment>) -> Outcome {
    let Some(candidate) = candidate else { return Outcome::Unknown };
    if id == current {
        return Outcome::Current;
    }
    if header(ask.headers, "sec-fetch-dest") == Some("document") {
        return Outcome::Document;
    }
    if let Some(site) = other_site(ask.headers, ask.host)
        && !allowed(&setting.allowed_domains, site.as_deref())
    {
        return Outcome::CrossSite;
    }
    if ask.production_host && candidate.target.as_deref() != Some("production") {
        return Outcome::Environment;
    }
    if let Some(threshold) = &setting.threshold
        && (id == threshold.deployment || candidate.created.is_none_or(|c| c <= threshold.created))
    {
        return Outcome::Threshold;
    }
    let max_age = match ask.verified_crawler && crawler(ask.headers) {
        true => setting.max_age().max(SKEW_CRAWLER_MAX_AGE_S),
        false => setting.max_age(),
    };
    match candidate.created {
        Some(created) if ask.now.saturating_sub(created) <= max_age => Outcome::Pinned,
        _ => Outcome::Expired,
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim)
}

/// The site a request came from when that isn't `host` itself: `Some(Some(name))`, or
/// `Some(None)` for one that can't be named (an opaque origin, or a cross-site request without a
/// referrer). None for the same site, and for clients that aren't browsers. `Origin` says it when
/// the browser sends one (fetches, CORS); a plain `<script src>` or `<img>` from elsewhere has
/// only `Sec-Fetch-Site` and the referrer.
fn other_site(headers: &HeaderMap, host: &str) -> Option<Option<String>> {
    if let Some(origin) = header(headers, ORIGIN.as_str()) {
        if origin == "null" {
            return Some(None);
        }
        let name = url_host(origin)?;
        return (name != host).then_some(Some(name));
    }
    match header(headers, "sec-fetch-site") {
        Some("cross-site" | "same-site") => Some(header(headers, REFERER.as_str()).and_then(url_host).filter(|r| r != host)),
        _ => None,
    }
}

/// The host of `https://name:port/path`, normalised.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    Some(normalize_host(authority)).filter(|h| !h.is_empty())
}

/// Whether the project's allowed domains take a request from `site` (None: a site without a name,
/// which only `*` takes).
fn allowed(domains: &[String], site: Option<&str>) -> bool {
    domains.iter().any(|d| match site {
        _ if d == "*" => true,
        Some(site) => d == site || wildcard_above(site).as_deref() == Some(d.as_str()),
        None => false,
    })
}

/// Googlebot or Bingbot, by the user agent (the address is checked apart).
fn crawler(headers: &HeaderMap) -> bool {
    header(headers, USER_AGENT.as_str()).is_some_and(|ua| {
        let ua = ua.to_ascii_lowercase();
        ua.contains("googlebot") || ua.contains("bingbot")
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use forkrail_common::routes::{SKEW_DEFAULT_MAX_AGE_S, SkewThreshold};
    use hyper::header::HeaderValue;

    use super::*;

    const NOW: u64 = 1_794_800_000;

    fn deployment(target: &str, age_s: Option<u64>) -> Deployment {
        Deployment {
            project: "shop".into(),
            function: "forkrail-app-shop".into(),
            qualifier: "1".into(),
            region: None,
            replicas: BTreeMap::new(),
            max_inflight: None,
            cpu_target_pct: None,
            max_workers: None,
            idle_ms: None,
            headroom_pct: None,
            max_duration_s: 300,
            memory_mb: 640,
            target: Some(target.into()),
            runtime: None,
            entry: None,
            env: BTreeMap::new(),
            static_files: None,
            egress: Vec::new(),
            rules: None,
            schedules: Vec::new(),
            images: None,
            created: age_s.map(|a| NOW - a),
            skew_id: None,
            next: None,
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn on() -> SkewProtection {
        SkewProtection { enabled: true, ..Default::default() }
    }

    /// The outcome for a request with `pairs` to `host`, naming `old`, an hour-old production
    /// deployment, while the host points at `new`.
    fn outcome(setting: &SkewProtection, pairs: &[(&'static str, &str)], production_host: bool) -> Outcome {
        let h = headers(pairs);
        let ask = Ask { host: "shop.forkrail.com", headers: &h, now: NOW, production_host, verified_crawler: false };
        decide(setting, &ask, "new", "old", Some(&deployment("production", Some(3_600))))
    }

    #[test]
    fn finds_the_id_by_query_then_header_then_cookie() {
        let none = HeaderMap::new();
        assert_eq!(requested(Some("a=1&dpl=a1b2c3d4"), &none), Some(("a1b2c3d4".into(), Source::Query)));
        assert_eq!(requested(Some("dpl=a1b2%2Dc3"), &none), Some(("a1b2-c3".into(), Source::Query)), "decoded");
        let all = headers(&[("x-deployment-id", " h1 "), ("cookie", "a=b; __vdpl=c1; x=y")]);
        assert_eq!(requested(Some("dpl=q1"), &all), Some(("q1".into(), Source::Query)));
        assert_eq!(requested(Some("a=1"), &all), Some(("h1".into(), Source::Header)));
        let cookies = headers(&[("cookie", "a=b"), ("cookie", "__vdpl=\"c1\"")]);
        assert_eq!(requested(None, &cookies), Some(("c1".into(), Source::Cookie)), "a second Cookie header, quoted value");
        // Twice, empty, bare or another name's: not an id.
        for query in ["dpl=a&dpl=b", "dpl=", "dpl", "DPL=a", "xdpl=a", "dplx=a"] {
            assert_eq!(requested(Some(query), &none), None, "{query}");
        }
        assert_eq!(requested(None, &headers(&[("cookie", "__vdplx=a; x__vdpl=b")])), None);
        // Two that differ (another app under the domain can set one for all of it): neither.
        assert_eq!(requested(None, &headers(&[("cookie", "__vdpl=mine; __vdpl=theirs")])), None);
        assert_eq!(requested(None, &headers(&[("cookie", "__vdpl=c1"), ("cookie", "__vdpl=c2")])), None);
        assert_eq!(requested(None, &headers(&[("cookie", "__vdpl=c1; __vdpl=c1")])), Some(("c1".into(), Source::Cookie)));
        assert_eq!(requested(None, &headers(&[("x-deployment-id", "  ")])), None);
    }

    #[test]
    fn strips_dpl_and_keeps_the_rest() {
        assert_eq!(strip("dpl=a1&a=1").as_deref(), Some("a=1"));
        assert_eq!(strip("a=1&dpl=a1&b=%20x").as_deref(), Some("a=1&b=%20x"));
        assert_eq!(strip("dpl=a1"), None);
        assert_eq!(strip("dplx=1&a=1").as_deref(), Some("dplx=1&a=1"));
    }

    #[test]
    fn pins_a_young_deployment_of_the_same_project() {
        assert_eq!(outcome(&on(), &[], true), Outcome::Pinned);
        assert_eq!(outcome(&on(), &[("sec-fetch-dest", "script"), ("sec-fetch-site", "same-origin")], true), Outcome::Pinned);
        // The same origin, named by Origin (a fetch, a form post).
        assert_eq!(outcome(&on(), &[("origin", "https://shop.forkrail.com")], true), Outcome::Pinned);
    }

    #[test]
    fn never_pins_unknown_ids_or_the_current_one() {
        let h = HeaderMap::new();
        let ask = Ask { host: "shop.forkrail.com", headers: &h, now: NOW, production_host: true, verified_crawler: false };
        assert_eq!(decide(&on(), &ask, "new", "gone", None), Outcome::Unknown);
        assert_eq!(decide(&on(), &ask, "new", "new", Some(&deployment("production", Some(10)))), Outcome::Current);
    }

    #[test]
    fn never_pins_a_page_load() {
        assert_eq!(outcome(&on(), &[("sec-fetch-dest", "document")], true), Outcome::Document);
    }

    #[test]
    fn other_sites_only_when_allowed() {
        let evil = [("origin", "https://evil.example")];
        assert_eq!(outcome(&on(), &evil, true), Outcome::CrossSite);
        // No Origin (a <script src> from elsewhere): Sec-Fetch-Site and the referrer.
        let script = [("sec-fetch-site", "cross-site"), ("referer", "https://app.example.com/page")];
        assert_eq!(outcome(&on(), &script, true), Outcome::CrossSite);
        assert_eq!(outcome(&on(), &[("sec-fetch-site", "same-site")], true), Outcome::CrossSite, "a sibling subdomain is another origin");
        assert_eq!(outcome(&on(), &[("origin", "null")], true), Outcome::CrossSite);
        let allow = |d: &[&str]| SkewProtection { allowed_domains: d.iter().map(|s| s.to_string()).collect(), ..on() };
        assert_eq!(outcome(&allow(&["app.example.com"]), &script, true), Outcome::Pinned);
        assert_eq!(outcome(&allow(&["*.example.com"]), &script, true), Outcome::Pinned);
        assert_eq!(outcome(&allow(&["*.example.com"]), &[("origin", "https://a.app.example.com:8443")], true), Outcome::CrossSite, "one label deep");
        assert_eq!(outcome(&allow(&["*.example.com"]), &[("origin", "https://example.com")], true), Outcome::CrossSite);
        assert_eq!(outcome(&allow(&["*"]), &evil, true), Outcome::Pinned);
        assert_eq!(outcome(&allow(&["*"]), &[("origin", "null")], true), Outcome::Pinned, "only * takes a site without a name");
        assert_eq!(outcome(&allow(&["app.example.com"]), &[("sec-fetch-site", "cross-site")], true), Outcome::CrossSite);
        // A port on our own origin is still our site.
        assert_eq!(outcome(&on(), &[("origin", "https://SHOP.forkrail.com:443")], true), Outcome::Pinned);
    }

    #[test]
    fn production_names_never_serve_previews() {
        let h = HeaderMap::new();
        let ask = |production_host| Ask { host: "shop.forkrail.com", headers: &h, now: NOW, production_host, verified_crawler: false };
        let preview = deployment("preview", Some(60));
        assert_eq!(decide(&on(), &ask(true), "new", "old", Some(&preview)), Outcome::Environment);
        // A preview's own host or its branch's may, as far as these rules go (the registry then
        // asks protection: `Gates::pin_reaches`).
        assert_eq!(decide(&on(), &ask(false), "new", "old", Some(&preview)), Outcome::Pinned);
        let mut untargeted = preview.clone();
        untargeted.target = None;
        assert_eq!(decide(&on(), &ask(true), "new", "old", Some(&untargeted)), Outcome::Environment);
    }

    #[test]
    fn threshold_and_maximum_age() {
        let h = HeaderMap::new();
        let ask = Ask { host: "shop.forkrail.com", headers: &h, now: NOW, production_host: true, verified_crawler: false };
        let at = |age| deployment("production", Some(age));
        // 12 hours by default, from creation.
        assert_eq!(decide(&on(), &ask, "new", "old", Some(&at(SKEW_DEFAULT_MAX_AGE_S))), Outcome::Pinned);
        assert_eq!(decide(&on(), &ask, "new", "old", Some(&at(SKEW_DEFAULT_MAX_AGE_S + 1))), Outcome::Expired);
        assert_eq!(decide(&on(), &ask, "new", "old", Some(&deployment("production", None))), Outcome::Expired, "no age, no pin");
        let week = SkewProtection { max_age_s: Some(7 * 86_400), ..on() };
        assert_eq!(decide(&week, &ask, "new", "old", Some(&at(6 * 86_400))), Outcome::Pinned);
        // The threshold deployment and anything created before it.
        let threshold = SkewProtection { threshold: Some(SkewThreshold { deployment: "fix".into(), created: NOW - 600 }), ..on() };
        assert_eq!(decide(&threshold, &ask, "new", "old", Some(&at(601))), Outcome::Threshold);
        assert_eq!(decide(&threshold, &ask, "new", "old", Some(&at(600))), Outcome::Threshold);
        assert_eq!(decide(&threshold, &ask, "new", "fix", Some(&at(100))), Outcome::Threshold);
        assert_eq!(decide(&threshold, &ask, "new", "old", Some(&at(599))), Outcome::Pinned);
    }

    #[test]
    fn verified_crawlers_reach_back_sixty_days() {
        let googlebot = headers(&[("user-agent", "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)")]);
        let ask = |verified_crawler| Ask { host: "shop.forkrail.com", headers: &googlebot, now: NOW, production_host: true, verified_crawler };
        let month = deployment("production", Some(30 * 86_400));
        assert_eq!(decide(&on(), &ask(true), "new", "old", Some(&month)), Outcome::Pinned);
        // Claiming to be Googlebot from anywhere else gets the project's maximum age.
        assert_eq!(decide(&on(), &ask(false), "new", "old", Some(&month)), Outcome::Expired);
        assert_eq!(decide(&on(), &ask(true), "new", "old", Some(&deployment("production", Some(61 * 86_400)))), Outcome::Expired);
        // A verified address with another user agent (a fetcher acting for a person): the project's.
        let other = headers(&[("user-agent", "Mozilla/5.0")]);
        let ask = Ask { host: "shop.forkrail.com", headers: &other, now: NOW, production_host: true, verified_crawler: true };
        assert_eq!(decide(&on(), &ask, "new", "old", Some(&month)), Outcome::Expired);
    }

    #[test]
    fn hosts_of_urls() {
        assert_eq!(url_host("https://App.Example.com:8443/x?y").as_deref(), Some("app.example.com"));
        assert_eq!(url_host("http://user:pw@a.example.com").as_deref(), Some("a.example.com"));
        assert_eq!(url_host("https://[::1]:3000").as_deref(), Some("[::1]"));
        assert_eq!(url_host("https://"), None);
    }
}
