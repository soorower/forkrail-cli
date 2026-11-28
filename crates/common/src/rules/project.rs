//! A project's own routing rules, as Vercel's project-level routing rules have them: edited in the
//! dashboard, CLI or API, published without a redeploy, and run on every request to any of the
//! project's deployments, in front of the deployment's own routes.
//!
//! Each rule matches a path (exactly, by a path pattern, or by a regex), optionally a host,
//! header, cookie or query, and then does one primary thing (rewrite, redirect, or answer with a
//! status) and any number of changes to headers and the query. A rule with no primary action only
//! changes things and lets the request go on, so every matching rule's changes apply; the first
//! rule with a primary action ends the project's rules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{Condition, Engine, Header, MAX_PATTERN, MAX_RULES, Program, Route, SourceRoute, Transform, convert};

/// How a rule's `src` is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SrcSyntax {
    /// The path exactly (`/blog`).
    #[serde(rename = "equals")]
    Equals,
    /// A path pattern with `:param` and wildcards (`/blog/:slug`, `/api/:path*`).
    #[serde(rename = "path-to-regexp")]
    PathToRegexp,
    /// A regular expression (`^/posts/[0-9]+$`).
    #[serde(rename = "regex")]
    Regex,
}

impl SrcSyntax {
    /// The syntax a pattern written without one is taken to use: a regex if it has a regex's
    /// marks, a path pattern if it has `:name` or `*`, else the path exactly.
    pub fn infer(src: &str) -> SrcSyntax {
        if src.starts_with('^') || src.ends_with('$') || src.contains(['(', '[', '|', '\\', '+', '?']) {
            SrcSyntax::Regex
        } else if src.contains('*') || src.split('/').any(|seg| seg.starts_with(':')) {
            SrcSyntax::PathToRegexp
        } else {
            SrcSyntax::Equals
        }
    }
}

/// One rule, as the API takes and gives it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRule {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_syntax: Option<SrcSyntax>,
    pub route: RuleRoute,
}

fn enabled() -> bool {
    true
}

/// What a rule matches and does, in `vercel.json`'s `routes` terms.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleRoute {
    pub src: String,
    /// Where it goes: a path in the deployment or a URL elsewhere. With a 3xx `status`, a redirect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Response headers to set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_sensitive: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub has: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transforms: Vec<Transform>,
}

/// What a rule does, for lists and filters (Vercel's `routeType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleType {
    Redirect,
    Rewrite,
    SetStatus,
    Transform,
}

const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];
const MAX_NAME: usize = 256;
const MAX_DESCRIPTION: usize = 1024;

impl ProjectRule {
    pub fn kind(&self) -> RuleType {
        match (&self.route.dest, self.route.status) {
            (Some(_), Some(s)) if REDIRECTS.contains(&s) => RuleType::Redirect,
            (Some(_), _) => RuleType::Rewrite,
            (None, Some(_)) => RuleType::SetStatus,
            (None, None) => RuleType::Transform,
        }
    }

    pub fn syntax(&self) -> SrcSyntax {
        self.src_syntax.unwrap_or_else(|| SrcSyntax::infer(&self.route.src))
    }

    /// The rule as one of Vercel's routes, checked: a mistake is refused here, when it's saved,
    /// not found by a visitor.
    pub fn compile(&self) -> Result<SourceRoute, String> {
        let what = format!("rule {:?}", self.name);
        if self.name.trim().is_empty() || self.name.chars().count() > MAX_NAME {
            return Err(format!("a rule needs a name of 1 to {MAX_NAME} characters"));
        }
        if self.description.as_ref().is_some_and(|d| d.chars().count() > MAX_DESCRIPTION) {
            return Err(format!("{what}: the description is longer than {MAX_DESCRIPTION} characters"));
        }
        let r = &self.route;
        if r.src.is_empty() || r.src.chars().count() > MAX_PATTERN {
            return Err(format!("{what}: the path pattern must be 1 to {MAX_PATTERN} characters"));
        }
        if r.dest.as_ref().is_some_and(|d| d.is_empty() || d.chars().count() > MAX_PATTERN) {
            return Err(format!("{what}: the destination must be 1 to {MAX_PATTERN} characters"));
        }
        // Conditions and transforms are checked as in `vercel.json`.
        let mut raw = Map::new();
        for (key, value) in [("has", serde_json::to_value(&r.has)), ("missing", serde_json::to_value(&r.missing)), ("transforms", serde_json::to_value(&r.transforms))] {
            let value = value.map_err(|e| e.to_string())?;
            if value.as_array().is_some_and(|a| !a.is_empty()) {
                raw.insert(key.to_owned(), value);
            }
        }
        let has = super::conditions(&raw, "has", &what)?;
        let missing = super::conditions(&raw, "missing", &what)?;
        let transforms = super::transforms(&raw, &what, None)?;
        for (key, value) in &r.headers {
            if key.is_empty() || !key.bytes().all(|b| b.is_ascii_graphic()) || value.chars().any(char::is_control) {
                return Err(format!("{what}: header {key:?} isn't a header"));
            }
        }
        let kind = self.kind();
        if let Some(status) = r.status {
            if !(100..=999).contains(&status) {
                return Err(format!("{what}: status {status} must be from 100 to 999"));
            }
            if r.dest.is_some() && (300..400).contains(&status) && !REDIRECTS.contains(&status) {
                return Err(format!("{what}: a redirect's status is 301, 302, 303, 307 or 308"));
            }
        }
        let (src, dest) = match self.syntax() {
            SrcSyntax::Equals => {
                if !r.src.starts_with('/') {
                    return Err(format!("{what}: an exact path starts with `/`"));
                }
                (format!("^{}$", regex::escape(&r.src)), r.dest.clone())
            }
            SrcSyntax::PathToRegexp => {
                let (src, segments) = convert::source_to_regex(&r.src).map_err(|e| format!("{what}: {:?} isn't a valid path pattern: {e}", r.src))?;
                let dest = match &r.dest {
                    Some(d) => Some(convert::replace_segments(&segments, &convert::has_segments(&has), d, kind == RuleType::Redirect).map_err(|e| format!("{what}: {e}"))?),
                    None => None,
                };
                (src, dest)
            }
            SrcSyntax::Regex => (convert::normalize_src(&r.src), r.dest.clone()),
        };
        let mut headers: Vec<Header> = r.headers.iter().map(|(k, v)| Header { key: k.clone(), value: v.clone() }).collect();
        let route = match kind {
            RuleType::Redirect => {
                headers.push(Header { key: "Location".into(), value: dest.unwrap_or_default() });
                SourceRoute { src, headers, status: r.status, ..Default::default() }
            }
            RuleType::Rewrite => {
                let external = dest.as_deref().is_some_and(|d| d.starts_with("http://") || d.starts_with("https://"));
                // A path in the deployment goes on through the deployment's own routes when it isn't
                // one of its files.
                SourceRoute { src, dest, headers, status: r.status, check: !external, ..Default::default() }
            }
            RuleType::SetStatus => SourceRoute { src, headers, status: r.status, ..Default::default() },
            RuleType::Transform => SourceRoute { src, headers, cont: true, ..Default::default() },
        };
        let route = SourceRoute {
            // As the rule says, else not: never the deployment's (whose own routes may match
            // case-sensitively), or a rule kept `/admin` would let `/ADMIN` by.
            case_sensitive: Some(r.case_sensitive.unwrap_or(false)),
            has,
            missing,
            transforms,
            from: Some(format!("project rule {:?}", self.name)),
            ..route
        };
        Engine::new(&Program { routes: vec![Route::Source(route.clone())], ..Program::default() }).map_err(|e| format!("{what}: {e}"))?;
        Ok(route)
    }
}

/// The program a project's rules make: the enabled ones, in order. Ids must be unique.
pub fn compile(rules: &[ProjectRule]) -> Result<Program, String> {
    if rules.len() > MAX_RULES {
        return Err(format!("{} rules; the limit is {MAX_RULES} a project", rules.len()));
    }
    let mut seen = std::collections::HashSet::new();
    let mut routes = Vec::new();
    for rule in rules {
        if !seen.insert(rule.id.as_str()) {
            return Err(format!("rule id {} is there twice", rule.id));
        }
        let route = rule.compile()?;
        if rule.enabled {
            routes.push(Route::Source(route));
        }
    }
    Ok(Program { routes, ..Program::default() })
}

/// Rules from JSON (`[ProjectRule]`), for the API; `id`s it lacks are made up.
pub fn from_json(value: &Value) -> Result<Vec<ProjectRule>, String> {
    let Some(list) = value.as_array() else { return Err("rules must be a list".into()) };
    list.iter()
        .enumerate()
        .map(|(i, v)| {
            let mut v = v.clone();
            if let Some(obj) = v.as_object_mut()
                && !obj.contains_key("id")
            {
                obj.insert("id".into(), Value::String(new_id()));
            }
            serde_json::from_value(v).map_err(|e| format!("rule {i}: {e}"))
        })
        .collect()
}

/// A new rule's id.
pub fn new_id() -> String {
    format!("rt_{}", crate::random_hex(8))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::rules::{Files, NoFiles, Outcome, Request};

    fn rules(v: Value) -> Vec<ProjectRule> {
        from_json(&v).unwrap()
    }

    fn route(program: &Program, files: &dyn Files, path: &str, headers: &[(&'static str, &str)]) -> crate::rules::Decision {
        let mut h = http::HeaderMap::new();
        for (k, v) in headers {
            h.append(*k, v.parse().unwrap());
        }
        let (path, query) = path.split_once('?').map_or((path, None), |(p, q)| (p, Some(q)));
        Engine::new(program).unwrap().route(&Request { method: "GET", path, query, host: "a.com", headers: &h }, files)
    }

    #[test]
    fn rules_route_in_front_of_the_deployment() {
        let list = rules(json!([
            {"name": "CORS", "route": {"src": "^/api/.*$", "headers": {"access-control-allow-origin": "*"}}},
            {"name": "Old blog", "srcSyntax": "equals", "route": {"src": "/blog", "dest": "/articles", "status": 301}},
            {"name": "API proxy", "srcSyntax": "path-to-regexp", "route": {"src": "/api/:path*", "dest": "https://api.example.com/:path*"}},
            {"name": "Block admin", "route": {"src": "^/admin/.*$", "status": 403}},
            {"name": "Off", "enabled": false, "route": {"src": "/x", "dest": "/y", "status": 308}},
            {"name": "Needs login", "srcSyntax": "path-to-regexp", "route": {"src": "/protected/:path*", "dest": "/login", "status": 307, "missing": [{"type": "cookie", "key": "session"}]}},
            {"name": "Theme", "route": {"src": "/home", "transforms": [{"type": "request.query", "op": "set", "target": {"key": "theme"}, "args": "dark"}]}},
        ]));
        let mut program = compile(&rules(serde_json::to_value(&list).unwrap())).unwrap();
        assert_eq!(program.routes.len(), 6);
        // The deployment's own routes follow; here, a function that takes everything.
        program.fallback = true;
        let d = route(&program, &NoFiles, "/blog?a=1", &[]);
        assert_eq!((d.outcome, d.status), (Outcome::Redirect { location: "/articles?a=1".into() }, Some(301)));
        let d = route(&program, &NoFiles, "/api/users/1", &[]);
        assert_eq!(d.outcome, Outcome::External { url: "https://api.example.com/users/1".into() });
        assert_eq!(d.headers, vec![("access-control-allow-origin".into(), "*".into())]);
        assert_eq!(route(&program, &NoFiles, "/admin/x", &[]).status, Some(403));
        assert_eq!(route(&program, &NoFiles, "/x", &[]).outcome, Outcome::Function { query: None });
        assert_eq!(route(&program, &NoFiles, "/protected/a", &[]).outcome, Outcome::Redirect { location: "/login".into() });
        assert_eq!(route(&program, &NoFiles, "/protected/a", &[("cookie", "session=1")]).outcome, Outcome::Function { query: None });
        // A query transform is in the function's query already.
        let home = route(&program, &NoFiles, "/home", &[]);
        assert_eq!((home.outcome, home.transforms.len()), (Outcome::Function { query: Some("theme=dark".into()) }, 0));
        assert_eq!(list[1].kind(), RuleType::Redirect);
        assert_eq!(list[0].kind(), RuleType::Transform);
    }

    #[test]
    fn mistakes_are_refused() {
        let bad = |v: Value, expect: &str| {
            let err = compile(&rules(json!([v]))).unwrap_err();
            assert!(err.contains(expect), "{err}");
        };
        bad(json!({"name": "", "route": {"src": "/a"}}), "name");
        bad(json!({"name": "a", "srcSyntax": "equals", "route": {"src": "a"}}), "starts with");
        bad(json!({"name": "a", "route": {"src": "/a", "dest": "/b", "status": 305}}), "301, 302");
        bad(json!({"name": "a", "srcSyntax": "regex", "route": {"src": "/a(?<x"}}), "isn't one forkrail can run");
        bad(json!({"name": "a", "route": {"src": "/a", "has": [{"type": "query"}]}}), "needs `key`");
        assert!(compile(&rules(json!([{"id": "x", "name": "a", "route": {"src": "/a"}}, {"id": "x", "name": "b", "route": {"src": "/b"}}]))).is_err());
        assert_eq!(SrcSyntax::infer("/blog"), SrcSyntax::Equals);
        assert_eq!(SrcSyntax::infer("/blog/:slug"), SrcSyntax::PathToRegexp);
        assert_eq!(SrcSyntax::infer("^/posts/[0-9]+$"), SrcSyntax::Regex);
    }
}
