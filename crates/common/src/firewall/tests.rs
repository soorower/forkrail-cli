use std::net::IpAddr;
use std::time::Duration;

use serde_json::json;

use super::*;

struct Req {
    method: &'static str,
    host: &'static str,
    path: &'static str,
    target: Option<&'static str>,
    query: Option<&'static str>,
    headers: http::HeaderMap,
    ip: &'static str,
    protocol: &'static str,
    scheme: &'static str,
    environment: &'static str,
    country: Option<&'static str>,
    continent: Option<&'static str>,
    country_region: Option<&'static str>,
    city: Option<&'static str>,
    asn: Option<u32>,
    api: Option<(&'static str, &'static str)>,
    passed: bool,
    ja4: Option<&'static str>,
}

impl Default for Req {
    fn default() -> Self {
        Self {
            method: "GET",
            host: "app.example.com",
            path: "/",
            target: None,
            query: None,
            headers: http::HeaderMap::new(),
            ip: "203.0.113.7",
            protocol: "HTTP/2.0",
            scheme: "https",
            environment: "production",
            country: Some("US"),
            continent: Some("NA"),
            country_region: Some("California"),
            city: Some("San Francisco"),
            asn: Some(13335),
            api: None,
            passed: false,
            ja4: Some("t13d1516h2_8daaf6152771_e5627efa2ab1"),
        }
    }
}

impl Req {
    fn header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.append(name, http::HeaderValue::from_static(value));
        self
    }

    fn judge(&self, fw: &Firewall) -> Verdict {
        self.judge_in(fw, &NoLimits)
    }

    fn judge_in(&self, fw: &Firewall, limiter: &dyn Limiter) -> Verdict {
        let ip: IpAddr = self.ip.parse().unwrap();
        fw.check(&Request {
            method: self.method,
            host: self.host,
            raw_path: self.path,
            target_path: self.target,
            query: self.query,
            headers: &self.headers,
            ip,
            protocol: self.protocol,
            scheme: self.scheme,
            environment: self.environment,
            region: "us-east-1",
            continent: self.continent,
            country: self.country,
            country_region: self.country_region,
            city: self.city,
            asn: self.asn,
            ja4: self.ja4,
            rate_limit_api: self.api.map(|(id, key)| ApiCheck { id, key }),
            challenge_passed: self.passed,
        }, limiter)
    }
}

fn config(value: serde_json::Value) -> Config {
    let mut c: Config = serde_json::from_value(value).unwrap();
    c.assign_ids();
    c
}

/// One deny rule with one condition.
fn deny_if(condition: serde_json::Value) -> Firewall {
    Firewall::new(&config(json!({
        "rules": [{"name": "r", "active": true, "conditionGroup": [{"conditions": [condition]}], "action": {"mitigate": {"action": "deny"}}}]
    })))
    .unwrap()
}

fn denied(fw: &Firewall, req: &Req) -> bool {
    matches!(req.judge(fw).decision, Decision::Deny { .. })
}

#[test]
fn every_condition_type_reads_its_part_of_the_request() {
    let cases: Vec<(serde_json::Value, Req, Req)> = vec![
        (json!({"type": "host", "op": "eq", "value": "APP.example.com"}), Req::default(), Req { host: "other.example.com", ..Req::default() }),
        (json!({"type": "path", "op": "eq", "value": "/api/users"}), Req { path: "/api//users/", ..Req::default() }, Req { path: "/api/user", ..Req::default() }),
        (json!({"type": "path", "op": "eq", "value": "/a b"}), Req { path: "/a%20b", ..Req::default() }, Req::default()),
        (json!({"type": "raw_path", "op": "eq", "value": "/api/"}), Req { path: "/api/", ..Req::default() }, Req { path: "/api", ..Req::default() }),
        (json!({"type": "target_path", "op": "eq", "value": "/index.html"}), Req { target: Some("/index.html"), ..Req::default() }, Req { target: Some("/x"), ..Req::default() }),
        (json!({"type": "method", "op": "eq", "value": "post"}), Req { method: "POST", ..Req::default() }, Req::default()),
        (json!({"type": "user_agent", "op": "sub", "value": "curl"}), Req::default().header("user-agent", "Curl/8.4"), Req::default().header("user-agent", "Mozilla")),
        (json!({"type": "header", "key": "X-Api-Key", "op": "eq", "value": "abc"}), Req::default().header("x-api-key", "ABC"), Req::default().header("x-api-key", "abd")),
        (json!({"type": "query", "key": "Debug", "op": "eq", "value": "1"}), Req { query: Some("a=2&debug=1"), ..Req::default() }, Req { query: Some("debug=0"), ..Req::default() }),
        (json!({"type": "cookie", "key": "session", "op": "pre", "value": "adm"}), Req::default().header("cookie", "a=1; Session=Admin42"), Req::default().header("cookie", "session=user")),
        (json!({"type": "ip_address", "op": "eq", "value": "203.0.113.0/24"}), Req::default(), Req { ip: "198.51.100.1", ..Req::default() }),
        (json!({"type": "ip_address", "op": "eq", "value": "2001:db8::/32"}), Req { ip: "2001:db8::1", ..Req::default() }, Req { ip: "2001:db9::1", ..Req::default() }),
        (json!({"type": "ip_address", "op": "eq", "value": "203.0.113.7"}), Req { ip: "::ffff:203.0.113.7", ..Req::default() }, Req { ip: "203.0.113.8", ..Req::default() }),
        (json!({"type": "region", "op": "eq", "value": "us-east-1"}), Req::default(), Req::default()),
        (json!({"type": "protocol", "op": "eq", "value": "http/1.1"}), Req { protocol: "HTTP/1.1", ..Req::default() }, Req::default()),
        (json!({"type": "scheme", "op": "eq", "value": "http"}), Req { scheme: "http", ..Req::default() }, Req::default()),
        (json!({"type": "environment", "op": "eq", "value": "Preview"}), Req { environment: "preview", ..Req::default() }, Req::default()),
        (json!({"type": "geo_continent", "op": "eq", "value": "eu"}), Req { continent: Some("EU"), ..Req::default() }, Req::default()),
        (json!({"type": "geo_country", "op": "inc", "value": ["RU", "kp"]}), Req { country: Some("KP"), ..Req::default() }, Req::default()),
        (json!({"type": "geo_country_region", "op": "eq", "value": "california"}), Req::default(), Req { country_region: None, ..Req::default() }),
        (json!({"type": "geo_city", "op": "eq", "value": "san francisco"}), Req::default(), Req { city: Some("Oakland"), ..Req::default() }),
        (json!({"type": "geo_as_number", "op": "eq", "value": "13335"}), Req::default(), Req { asn: Some(16509), ..Req::default() }),
        (json!({"type": "ja4_digest", "op": "eq", "value": "T13D1516H2_8daaf6152771_e5627efa2ab1"}), Req::default(), Req { ja4: None, ..Req::default() }),
        (json!({"type": "ja4_digest", "op": "pre", "value": "t13d"}), Req::default(), Req { ja4: Some("t12i020100_aaaaaaaaaaaa_bbbbbbbbbbbb"), ..Req::default() }),
        (json!({"type": "geo_as_number", "op": "inc", "value": ["16509", "14618"]}), Req { asn: Some(14618), ..Req::default() }, Req::default()),
    ];
    for (condition, yes, no) in cases {
        let fw = deny_if(condition.clone());
        assert!(denied(&fw, &yes), "{condition} should match");
        // Region is always the router's, so its "no" case is a different value instead.
        if condition["type"] != "region" {
            assert!(!denied(&fw, &no), "{condition} shouldn't match");
        }
    }
    let other_region = deny_if(json!({"type": "region", "op": "eq", "value": "eu-central-1"}));
    assert!(!denied(&other_region, &Req::default()));
}

#[test]
fn every_operator_and_negation() {
    let ua = |v: &'static str| Req::default().header("user-agent", v);
    let cases: Vec<(serde_json::Value, &'static str, bool)> = vec![
        (json!({"type": "user_agent", "op": "eq", "value": "bot"}), "BOT", true),
        (json!({"type": "user_agent", "op": "neq", "value": "bot"}), "BOT", false),
        (json!({"type": "user_agent", "op": "neq", "value": "bot"}), "human", true),
        (json!({"type": "user_agent", "op": "eq", "value": "bot", "neg": true}), "human", true),
        (json!({"type": "user_agent", "op": "inc", "value": ["a", "b"]}), "B", true),
        (json!({"type": "user_agent", "op": "ninc", "value": ["a", "b"]}), "c", true),
        (json!({"type": "user_agent", "op": "ninc", "value": ["a", "b"]}), "a", false),
        (json!({"type": "user_agent", "op": "pre", "value": "python-"}), "python-requests/2", true),
        (json!({"type": "user_agent", "op": "pre", "value": "python-", "neg": true}), "python-requests/2", false),
        (json!({"type": "user_agent", "op": "suf", "value": "/2"}), "python-requests/2", true),
        (json!({"type": "user_agent", "op": "sub", "value": "REQUESTS"}), "python-requests/2", true),
        (json!({"type": "user_agent", "op": "re", "value": "^python-\\w+/\\d$"}), "Python-Requests/2", true),
        (json!({"type": "user_agent", "op": "re", "value": "^(?!mozilla).*"}), "curl", true),
        (json!({"type": "user_agent", "op": "re", "value": "^(?!mozilla).*"}), "Mozilla/5.0", false),
        (json!({"type": "user_agent", "op": "ex"}), "x", true),
        (json!({"type": "user_agent", "op": "nex"}), "x", false),
        (json!({"type": "user_agent", "op": "gt", "value": 5}), "6", true),
        (json!({"type": "user_agent", "op": "lte", "value": "5"}), "6", false),
    ];
    for (condition, agent, want) in cases {
        assert_eq!(denied(&deny_if(condition.clone()), &ua(agent)), want, "{condition} on {agent:?}");
    }
    // A missing header: `ex` is false, and its negation true.
    assert!(!denied(&deny_if(json!({"type": "header", "key": "x-token", "op": "ex"})), &Req::default()));
    assert!(denied(&deny_if(json!({"type": "header", "key": "x-token", "op": "nex"})), &Req::default()));
    assert!(denied(&deny_if(json!({"type": "header", "key": "x-token", "op": "ex", "neg": true})), &Req::default()));
    // A missing value fails a positive test, so its negation matches.
    assert!(denied(&deny_if(json!({"type": "header", "key": "x-token", "op": "neq", "value": "secret"})), &Req::default()));
    // Numbers on the AS number.
    assert!(denied(&deny_if(json!({"type": "geo_as_number", "op": "gte", "value": 13335})), &Req::default()));
    assert!(!denied(&deny_if(json!({"type": "geo_as_number", "op": "lt", "value": 13335})), &Req::default()));
    // A header sent twice matches on either value.
    assert!(denied(&deny_if(json!({"type": "header", "key": "x-a", "op": "eq", "value": "2"})), &Req::default().header("x-a", "1").header("x-a", "2")));
    // Query keys and cookie names exist with no value too.
    assert!(denied(&deny_if(json!({"type": "query", "key": "flag", "op": "ex"})), &Req { query: Some("flag"), ..Req::default() }));
    assert!(denied(&deny_if(json!({"type": "query", "key": "q", "op": "eq", "value": "a b"})), &Req { query: Some("q=a+b"), ..Req::default() }));
}

#[test]
fn groups_are_ored_and_conditions_anded() {
    let fw = Firewall::new(&config(json!({"rules": [{
        "name": "admin from outside",
        "conditionGroup": [
            {"conditions": [{"type": "path", "op": "pre", "value": "/admin"}, {"type": "geo_country", "op": "neq", "value": "US"}]},
            {"conditions": [{"type": "method", "op": "eq", "value": "TRACE"}]}
        ],
        "action": {"mitigate": {"action": "deny"}}
    }]})))
    .unwrap();
    assert!(!denied(&fw, &Req { path: "/admin", ..Req::default() }));
    assert!(denied(&fw, &Req { path: "/admin/x", country: Some("DE"), ..Req::default() }));
    assert!(!denied(&fw, &Req { path: "/home", country: Some("DE"), ..Req::default() }));
    assert!(denied(&fw, &Req { method: "TRACE", ..Req::default() }));
}

fn rule(name: &str, condition: serde_json::Value, mitigate: serde_json::Value) -> serde_json::Value {
    json!({"id": name, "name": name, "conditionGroup": [{"conditions": [condition]}], "action": {"mitigate": mitigate}})
}

#[test]
fn actions_in_order() {
    let any = json!({"type": "path", "op": "pre", "value": "/"});
    let api = json!({"type": "path", "op": "pre", "value": "/api"});
    let fw = Firewall::new(&config(json!({"rules": [
        rule("log1", any.clone(), json!({"action": "log"})),
        rule("log2", api.clone(), json!({"action": "log"})),
        rule("pass", json!({"type": "header", "key": "x-trusted", "op": "ex"}), json!({"action": "bypass"})),
        rule("away", json!({"type": "path", "op": "eq", "value": "/old"}), json!({"action": "redirect", "redirect": {"location": "/new", "permanent": false}})),
        rule("gone", json!({"type": "path", "op": "eq", "value": "/gone"}), json!({"action": "redirect", "redirect": {"location": "https://example.org/", "permanent": true}})),
        rule("block", api.clone(), json!({"action": "deny", "actionDuration": "5m"})),
    ]})))
    .unwrap();
    // The last log rule matched is reported when nothing acts.
    assert_eq!(Req { path: "/home", ..Req::default() }.judge(&fw), Verdict { decision: Decision::Log, rule: Some("log1".into()) });
    // A deny after log rules is what's reported.
    assert_eq!(
        Req { path: "/api/x", ..Req::default() }.judge(&fw),
        Verdict { decision: Decision::Deny { persist: Some(Duration::from_secs(300)) }, rule: Some("block".into()) }
    );
    // A bypass skips the rest.
    assert_eq!(Req { path: "/api/x", ..Req::default() }.header("x-trusted", "1").judge(&fw), Verdict { decision: Decision::Bypass, rule: Some("pass".into()) });
    assert_eq!(
        Req { path: "/old", ..Req::default() }.judge(&fw),
        Verdict { decision: Decision::Redirect { location: "/new".into(), status: 307 }, rule: Some("away".into()) }
    );
    assert_eq!(Req { path: "/gone", ..Req::default() }.judge(&fw).decision, Decision::Redirect { location: "https://example.org/".into(), status: 301 });
}

#[test]
fn inactive_rules_and_a_disabled_firewall_do_nothing_but_ip_blocks_stay() {
    let mut c = config(json!({
        "rules": [rule("off", json!({"type": "path", "op": "pre", "value": "/"}), json!({"action": "deny"}))],
        "ips": [{"hostname": "*", "ip": "198.51.100.0/24", "action": "deny"}]
    }));
    c.rules[0].active = false;
    let fw = Firewall::new(&c).unwrap();
    assert!(!denied(&fw, &Req::default()));
    c.rules[0].active = true;
    c.firewall_enabled = false;
    let fw = Firewall::new(&c).unwrap();
    assert!(!denied(&fw, &Req::default()));
    assert!(denied(&fw, &Req { ip: "198.51.100.9", ..Req::default() }));
}

#[test]
fn ip_blocks_by_host_scope_and_list_order() {
    let fw = Firewall::new(&config(json!({
        "rules": [rule("all", json!({"type": "path", "op": "pre", "value": "/"}), json!({"action": "deny"}))],
        "ips": [
            {"id": "office", "hostname": "*", "ip": "192.0.2.10", "action": "bypass"},
            {"id": "range", "hostname": "*", "ip": "192.0.2.0/24", "action": "deny"},
            {"id": "docs", "hostname": "docs.example.com", "ip": "198.51.100.1", "action": "deny"},
            {"id": "subs", "hostname": "*.example.com", "ip": "198.51.100.2", "action": "deny"},
            {"id": "watch", "hostname": "*", "ip": "2001:db8::/48", "action": "log"}
        ]
    })))
    .unwrap();
    // The first matching entry decides: the bypass listed before the range.
    assert_eq!(Req { ip: "192.0.2.10", ..Req::default() }.judge(&fw), Verdict { decision: Decision::Bypass, rule: Some("office".into()) });
    assert_eq!(Req { ip: "192.0.2.11", ..Req::default() }.judge(&fw), Verdict { decision: Decision::Deny { persist: None }, rule: Some("range".into()) });
    // One host only.
    assert!(denied(&fw, &Req { ip: "198.51.100.1", host: "docs.example.com", ..Req::default() }));
    assert_eq!(Req { ip: "198.51.100.1", host: "app.example.com", ..Req::default() }.judge(&fw).rule.as_deref(), Some("all"));
    // `*.example.com` covers subdomains, not the apex.
    assert_eq!(Req { ip: "198.51.100.2", host: "a.b.example.com", ..Req::default() }.judge(&fw).rule.as_deref(), Some("subs"));
    assert_eq!(Req { ip: "198.51.100.2", host: "example.com", ..Req::default() }.judge(&fw).rule.as_deref(), Some("all"));
    // A log entry goes on to the rules.
    assert_eq!(Req { ip: "2001:db8::5", ..Req::default() }.judge(&fw).rule.as_deref(), Some("all"));
    // What the address alone decides, before any request (the router's handshake start): the
    // first entry again, for the host; custom rules aren't the address's.
    let stops = |ip: &str, host: &str| fw.stops_address(ip.parse().unwrap(), host);
    assert!(!stops("192.0.2.10", "app.example.com"));
    assert!(stops("192.0.2.11", "app.example.com"));
    assert!(stops("198.51.100.1", "docs.example.com") && !stops("198.51.100.1", "app.example.com"));
    assert!(!stops("2001:db8::5", "app.example.com"));
    assert!(!stops("203.0.113.1", "app.example.com"));
}

#[test]
fn named_lists() {
    let fw = Firewall::new(&config(json!({
        "lists": {"scanners": ["198.51.100.0/24", "2001:db8::1"]},
        "rules": [rule("scan", json!({"type": "ip_address", "op": "list", "value": "scanners"}), json!({"action": "deny"}))]
    })))
    .unwrap();
    assert!(denied(&fw, &Req { ip: "198.51.100.200", ..Req::default() }));
    assert!(denied(&fw, &Req { ip: "2001:db8::1", ..Req::default() }));
    assert!(!denied(&fw, &Req { ip: "2001:db8::2", ..Req::default() }));
}

#[test]
fn refused_configurations() {
    let bad = |c: serde_json::Value| -> String {
        let mut config: Config = serde_json::from_value(c).unwrap();
        config.assign_ids();
        Firewall::new(&config).err().unwrap_or_else(|| panic!("accepted"))
    };
    let cond = |c: serde_json::Value| json!({"rules": [{"name": "r", "conditionGroup": [{"conditions": [c]}], "action": {"mitigate": {"action": "deny"}}}]});
    assert!(bad(cond(json!({"type": "bot_name", "op": "eq", "value": "x"}))).contains("isn't supported"));
    assert!(bad(cond(json!({"type": "nope", "op": "eq", "value": "x"}))).contains("unknown condition type"));
    assert!(bad(cond(json!({"type": "path", "op": "like", "value": "x"}))).contains("unknown operator"));
    assert!(bad(cond(json!({"type": "header", "op": "eq", "value": "x"}))).contains("needs a key"));
    assert!(bad(cond(json!({"type": "path", "op": "eq"}))).contains("needs a value"));
    assert!(bad(cond(json!({"type": "path", "op": "eq", "value": ["a"]}))).contains("not a list"));
    assert!(bad(cond(json!({"type": "path", "op": "re", "value": "("}))).contains("pattern"));
    assert!(bad(cond(json!({"type": "ip_address", "op": "eq", "value": "300.1.1.1"}))).contains("isn't an IP"));
    assert!(bad(cond(json!({"type": "ip_address", "op": "eq", "value": "10.0.0.0/33"}))).contains("prefix length"));
    assert!(bad(cond(json!({"type": "ip_address", "op": "list", "value": "missing"}))).contains("no list"));
    assert!(bad(cond(json!({"type": "path", "op": "list", "value": "x"}))).contains("for ip_address"));
    assert!(bad(cond(json!({"type": "path", "op": "gt", "value": "abc"}))).contains("needs a number"));
    let many: Vec<String> = (0..=MAX_VALUES).map(|i| i.to_string()).collect();
    assert!(bad(cond(json!({"type": "path", "op": "inc", "value": many}))).contains("1-75 values"));
    let with = |m: serde_json::Value| json!({"rules": [{"name": "r", "conditionGroup": [{"conditions": [{"type": "path", "op": "eq", "value": "/"}]}], "action": {"mitigate": m}}]});
    let limit = |r: serde_json::Value| with(json!({"action": "rate_limit", "rateLimit": r}));
    assert!(bad(with(json!({"action": "rate_limit"}))).contains("needs rateLimit"));
    assert!(bad(limit(json!({"algo": "fixed_window", "window": 5, "limit": 10, "keys": ["ip"]}))).contains("10-3600 seconds"));
    assert!(bad(limit(json!({"algo": "fixed_window", "window": 7200, "limit": 10, "keys": ["ip"]}))).contains("10-3600 seconds"));
    assert!(bad(limit(json!({"algo": "fixed_window", "window": 60, "limit": 0, "keys": ["ip"]}))).contains("1-10000000 requests"));
    assert!(bad(limit(json!({"algo": "fixed_window", "window": 60, "limit": 10, "keys": []}))).contains("rate-limit keys"));
    assert!(bad(limit(json!({"algo": "fixed_window", "window": 60, "limit": 10, "keys": ["mac"]}))).contains("use ip, ja4, header:<name>"));
    assert!(bad(limit(json!({"algo": "fixed_window", "window": 60, "limit": 10, "keys": ["header:"]}))).contains("name what to count"));
    assert!(bad(limit(json!({"algo": "fixed_window", "window": 60, "limit": 10, "keys": ["ip", "IP"]}))).contains("twice"));
    assert!(bad(with(json!({"action": "rate_limit", "actionDuration": "5m", "rateLimit": {"algo": "fixed_window", "window": 60, "limit": 10, "keys": ["ip"], "action": "log"}}))).contains("only logs"));
    assert!(serde_json::from_value::<Config>(limit(json!({"algo": "sliding", "window": 60, "limit": 10, "keys": ["ip"]}))).is_err());
    let many: Vec<serde_json::Value> = (0..=MAX_RATE_LIMITS)
        .map(|i| json!({"name": format!("r{i}"), "conditionGroup": [{"conditions": [{"type": "path", "op": "eq", "value": "/"}]}], "action": {"mitigate": {"action": "rate_limit", "rateLimit": {"algo": "fixed_window", "window": 60, "limit": 10, "keys": ["ip"]}}}}))
        .collect();
    assert!(bad(json!({"rules": many})).contains("at most 100 rate-limit rules"));
    assert!(bad(with(json!({"action": "redirect"}))).contains("redirect.location"));
    assert!(bad(with(json!({"action": "redirect", "redirect": {"location": "javascript:x", "permanent": false}}))).contains("must be a path"));
    assert!(bad(with(json!({"action": "deny", "actionDuration": "2h"}))).contains("1 minute to 1 hour"));
    assert!(bad(with(json!({"action": "deny", "actionDuration": "30s"}))).contains("1 minute to 1 hour"));
    assert!(bad(with(json!({"action": "log", "actionDuration": "5m"}))).contains("goes with deny, challenge or rate_limit"));
    assert!(bad(json!({"rules": [{"name": "r", "conditionGroup": [], "action": {"mitigate": {"action": "deny"}}}]})).contains("condition groups"));
    assert!(bad(json!({"rules": [{"name": "", "conditionGroup": [{"conditions": [{"type": "path", "op": "eq", "value": "/"}]}], "action": {"mitigate": {"action": "deny"}}}]})).contains("a name"));
    assert!(bad(json!({"ips": [{"hostname": "bad host", "ip": "1.2.3.4", "action": "deny"}]})).contains("isn't a domain"));
    assert!(bad(json!({"lists": {"bad name": []}})).contains("list name"));
    // Two entries with one id.
    let dup = json!({"rules": [
        {"id": "x", "name": "a", "conditionGroup": [{"conditions": [{"type": "path", "op": "eq", "value": "/"}]}], "action": {"mitigate": {"action": "deny"}}},
        {"id": "x", "name": "b", "conditionGroup": [{"conditions": [{"type": "path", "op": "eq", "value": "/"}]}], "action": {"mitigate": {"action": "deny"}}}
    ]});
    assert!(bad(dup).contains("used twice"));
}

#[test]
fn a_vercel_export_reads_as_it_is() {
    // The shape Vercel's API returns, with fields we keep but don't use.
    let text = r#"{
        "firewallEnabled": true,
        "managedRules": {"owasp": {"active": false, "action": "log"}},
        "rules": [{
            "id": "rule_block_env_files",
            "name": "Block .env probes",
            "description": "scanners",
            "active": true,
            "conditionGroup": [{"conditions": [{"type": "path", "op": "suf", "neg": false, "value": ".env"}]}],
            "action": {"mitigate": {"action": "deny", "rateLimit": null, "redirect": null, "actionDuration": "1h", "bypassSystem": null}}
        }],
        "ips": [{"id": "ip_1", "hostname": "*", "ip": "192.0.2.1", "notes": "", "action": "deny"}]
    }"#;
    let c: Config = serde_json::from_str(text).unwrap();
    let fw = Firewall::new(&c).unwrap();
    assert!(denied(&fw, &Req { path: "/app/.ENV", ..Req::default() }));
    let again: Config = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
    assert_eq!(again, c);
}

#[test]
fn target_path_is_known_to_be_needed() {
    assert!(deny_if(json!({"type": "target_path", "op": "eq", "value": "/x"})).uses_target_path());
    assert!(!deny_if(json!({"type": "path", "op": "eq", "value": "/x"})).uses_target_path());
}

#[test]
fn durations() {
    assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
    assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
    assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    assert!(parse_duration("5").is_err());
    assert!(parse_duration("61m").is_err());
}

#[test]
fn ip_sets_find_every_range_holding_an_address() {
    let mut set = IpSet::default();
    set.insert(Cidr::parse("10.0.0.0/8").unwrap(), 1);
    set.insert(Cidr::parse("10.1.0.0/16").unwrap(), 2);
    set.insert(Cidr::parse("10.1.2.3").unwrap(), 3);
    set.insert(Cidr::parse("0.0.0.0/0").unwrap(), 4);
    set.insert(Cidr::parse("::/0").unwrap(), 5);
    let mut found: Vec<i32> = set.find("10.1.2.3".parse().unwrap()).copied().collect();
    found.sort();
    assert_eq!(found, vec![1, 2, 3, 4]);
    assert_eq!(set.find("::1".parse().unwrap()).copied().collect::<Vec<_>>(), vec![5]);
    assert_eq!(Cidr::parse("10.1.2.3/8").unwrap(), Cidr { addr: "10.0.0.0".parse().unwrap(), len: 8 });
}

#[test]
fn forty_rules_judge_quickly() {
    // The plan's budget: 40 rules within 20 µs a request (the bench measures the router itself).
    let mut rules = Vec::new();
    for i in 0..40 {
        rules.push(json!({
            "name": format!("r{i}"),
            "conditionGroup": [
                {"conditions": [{"type": "path", "op": "pre", "value": format!("/never-{i}")}, {"type": "geo_country", "op": "inc", "value": ["RU", "CN", "KP"]}]},
                {"conditions": [{"type": "user_agent", "op": "re", "value": format!("^scanner-{i}/")}]}
            ],
            "action": {"mitigate": {"action": "deny"}}
        }));
    }
    let fw = Firewall::new(&config(json!({"rules": rules}))).unwrap();
    let req = Req { path: "/api/listings/123", ..Req::default() }.header("user-agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15");
    let n = 20_000;
    let start = std::time::Instant::now();
    for _ in 0..n {
        assert_eq!(req.judge(&fw).decision, Decision::Allow);
    }
    let each = start.elapsed() / n;
    eprintln!("40 rules: {each:?} a request");
    if !cfg!(debug_assertions) {
        assert!(each < Duration::from_micros(20), "{each:?}");
    }
}

// ---- Rate limits ----

/// Buckets in a map, on a clock the test moves.
struct Counts {
    now: std::cell::Cell<u64>,
    buckets: std::cell::RefCell<HashMap<(String, Vec<u8>), Bucket>>,
}

impl Counts {
    fn new() -> Self {
        // A whole minute, so a 10 s window starts here.
        Self { now: std::cell::Cell::new(1_800_000_000 * 1_000_000_000), buckets: Default::default() }
    }

    fn wait(&self, d: Duration) {
        self.now.set(self.now.get() + d.as_nanos() as u64);
    }
}

impl Limiter for Counts {
    fn hit(&self, rule: &str, spec: RateSpec, client: &[u8]) -> Hit {
        let mut buckets = self.buckets.borrow_mut();
        let key = (rule.to_owned(), client.to_vec());
        let fresh = !buckets.contains_key(&key);
        buckets.entry(key).or_default().hit(fresh, spec, self.now.get())
    }
}

fn limited(path: &str, rate: serde_json::Value, persist: Option<&str>) -> serde_json::Value {
    let mut m = json!({"action": "rate_limit", "rateLimit": rate});
    if let Some(d) = persist {
        m["actionDuration"] = json!(d);
    }
    json!({"id": "limit", "name": "limit", "conditionGroup": [{"conditions": [{"type": "path", "op": "pre", "value": path}]}], "action": {"mitigate": m}})
}

fn decisions(fw: &Firewall, req: &Req, counts: &Counts, n: usize) -> Vec<&'static str> {
    (0..n).map(|_| req.judge_in(fw, counts).decision.name()).collect()
}

#[test]
fn a_fixed_window_allows_the_limit_then_answers_429_until_the_next_window() {
    let fw = Firewall::new(&config(json!({"rules": [limited("/api", json!({"algo": "fixed_window", "window": 10, "limit": 3, "keys": ["ip"]}), None)]}))).unwrap();
    let counts = Counts::new();
    let req = Req { path: "/api/x", ..Req::default() };
    assert_eq!(decisions(&fw, &req, &counts, 5), vec!["", "", "", "rate_limit", "rate_limit"]);
    let v = req.judge_in(&fw, &counts);
    assert_eq!((v.rule.as_deref(), v.decision.clone()), (Some("limit"), Decision::RateLimited { persist: None, retry_after: Duration::from_secs(10) }));
    // Other paths aren't counted; another address has its own bucket.
    assert_eq!(Req::default().judge_in(&fw, &counts).decision, Decision::Allow);
    assert_eq!(Req { path: "/api/x", ip: "203.0.113.8", ..Req::default() }.judge_in(&fw, &counts).decision, Decision::Allow);
    counts.wait(Duration::from_secs(4));
    assert!(matches!(req.judge_in(&fw, &counts).decision, Decision::RateLimited { retry_after, .. } if retry_after == Duration::from_secs(6)));
    counts.wait(Duration::from_secs(6));
    assert_eq!(decisions(&fw, &req, &counts, 4), vec!["", "", "", "rate_limit"]);
}

#[test]
fn a_token_bucket_refills_steadily() {
    let fw = Firewall::new(&config(json!({"rules": [limited("/", json!({"algo": "token_bucket", "window": 10, "limit": 10, "keys": ["ip"]}), None)]}))).unwrap();
    let counts = Counts::new();
    let req = Req::default();
    let first = decisions(&fw, &req, &counts, 11);
    assert_eq!(first.iter().filter(|d| d.is_empty()).count(), 10);
    assert_eq!(first[10], "rate_limit");
    // One token a second.
    assert!(matches!(req.judge_in(&fw, &counts).decision, Decision::RateLimited { retry_after, .. } if retry_after == Duration::from_secs(1)));
    counts.wait(Duration::from_millis(1500));
    assert_eq!(decisions(&fw, &req, &counts, 2), vec!["", "rate_limit"]);
    // Refused requests took nothing: after 0.5 s more, the second token is back.
    counts.wait(Duration::from_millis(500));
    assert_eq!(decisions(&fw, &req, &counts, 1), vec![""]);
    counts.wait(Duration::from_secs(60));
    assert_eq!(decisions(&fw, &req, &counts, 11).iter().filter(|d| d.is_empty()).count(), 10);
}

#[test]
fn keys_decide_who_shares_a_bucket() {
    let fw = Firewall::new(&config(json!({"rules": [limited("/", json!({"algo": "fixed_window", "window": 60, "limit": 1, "keys": ["header:X-Api-Key"]}), None)]}))).unwrap();
    let counts = Counts::new();
    let a = Req::default().header("x-api-key", "a");
    let a_elsewhere = Req { ip: "198.51.100.1", ..Req::default() }.header("x-api-key", "a");
    let b = Req::default().header("x-api-key", "b");
    let upper_a = Req::default().header("x-api-key", "A");
    assert_eq!(a.judge_in(&fw, &counts).decision, Decision::Allow);
    // The key, not the address, is the client; values are case-sensitive.
    assert_eq!(a_elsewhere.judge_in(&fw, &counts).decision.name(), "rate_limit");
    assert_eq!(b.judge_in(&fw, &counts).decision, Decision::Allow);
    assert_eq!(upper_a.judge_in(&fw, &counts).decision, Decision::Allow);
    // Without the header: one bucket for all of them.
    assert_eq!(Req::default().judge_in(&fw, &counts).decision, Decision::Allow);
    assert_eq!(Req { ip: "198.51.100.2", ..Req::default() }.judge_in(&fw, &counts).decision.name(), "rate_limit");

    // Two keys: one bucket per pair. An IPv6 client is counted by its /64.
    let fw = Firewall::new(&config(json!({"rules": [limited("/", json!({"algo": "fixed_window", "window": 60, "limit": 1, "keys": ["ip", "user_agent"]}), None)]}))).unwrap();
    let counts = Counts::new();
    let one = Req { ip: "2001:db8:1:2::1", ..Req::default() }.header("user-agent", "x");
    let same_64 = Req { ip: "2001:db8:1:2::ffff", ..Req::default() }.header("user-agent", "x");
    let other_64 = Req { ip: "2001:db8:1:3::1", ..Req::default() }.header("user-agent", "x");
    let other_agent = Req { ip: "2001:db8:1:2::1", ..Req::default() }.header("user-agent", "y");
    assert_eq!(one.judge_in(&fw, &counts).decision, Decision::Allow);
    assert_eq!(same_64.judge_in(&fw, &counts).decision.name(), "rate_limit");
    assert_eq!(other_64.judge_in(&fw, &counts).decision, Decision::Allow);
    assert_eq!(other_agent.judge_in(&fw, &counts).decision, Decision::Allow);

    // Every other key reads its part of the request.
    for (key, a, b) in [
        ("asn", Req::default(), Req { asn: Some(16509), ..Req::default() }),
        ("ja4", Req::default(), Req { ja4: Some("t12i020100_aaaaaaaaaaaa_bbbbbbbbbbbb"), ..Req::default() }),
        ("path", Req { path: "/a", ..Req::default() }, Req { path: "/b", ..Req::default() }),
        ("host", Req::default(), Req { host: "other.example.com", ..Req::default() }),
        ("country", Req::default(), Req { country: Some("DE"), ..Req::default() }),
        ("cookie:session", Req::default().header("cookie", "session=1"), Req::default().header("cookie", "session=2")),
        ("query:user", Req { query: Some("user=1"), ..Req::default() }, Req { query: Some("user=2"), ..Req::default() }),
    ] {
        let fw = Firewall::new(&config(json!({"rules": [limited("/", json!({"algo": "fixed_window", "window": 60, "limit": 1, "keys": [key]}), None)]}))).unwrap();
        let counts = Counts::new();
        assert_eq!(a.judge_in(&fw, &counts).decision, Decision::Allow, "{key}");
        assert_eq!(b.judge_in(&fw, &counts).decision, Decision::Allow, "{key}");
        assert_eq!(a.judge_in(&fw, &counts).decision.name(), "rate_limit", "{key}");
    }

    // A name is the app's, case and all, and its first value is the one counted: another case of
    // it, or a copy after it, is no new client.
    for (key, a, b) in [
        ("cookie:session", Req::default().header("cookie", "session=1"), Req::default().header("cookie", "SESSION=x; session=1; session=y")),
        ("query:user", Req { query: Some("user=1"), ..Req::default() }, Req { query: Some("User=x&user=1&user=y"), ..Req::default() }),
        ("header:x-api-key", Req::default().header("x-api-key", "1"), Req::default().header("x-api-key", "1").header("x-api-key", "y")),
    ] {
        let fw = Firewall::new(&config(json!({"rules": [limited("/", json!({"algo": "fixed_window", "window": 60, "limit": 1, "keys": [key]}), None)]}))).unwrap();
        let counts = Counts::new();
        assert_eq!(a.judge_in(&fw, &counts).decision, Decision::Allow, "{key}");
        assert_eq!(b.judge_in(&fw, &counts).decision.name(), "rate_limit", "{key}");
    }
}

#[test]
fn over_the_limit_actions_and_the_rules_after_a_rate_limit() {
    let deny_admin = json!({"id": "admin", "name": "admin", "conditionGroup": [{"conditions": [{"type": "path", "op": "pre", "value": "/admin"}]}], "action": {"mitigate": {"action": "deny"}}});
    let rate = |action: &str| json!({"algo": "fixed_window", "window": 60, "limit": 1, "keys": ["ip"], "action": action});
    let counts = Counts::new();
    // Under the limit, the rules after it still run.
    let fw = Firewall::new(&config(json!({"rules": [limited("/", rate("rate_limit"), None), deny_admin.clone()]}))).unwrap();
    let v = Req { path: "/admin", ..Req::default() }.judge_in(&fw, &counts);
    assert_eq!((v.decision.name(), v.rule.as_deref()), ("deny", Some("admin")));
    // Over it with deny: 403, held 5 minutes.
    let fw = Firewall::new(&config(json!({"rules": [limited("/", rate("deny"), Some("5m"))]}))).unwrap();
    let counts = Counts::new();
    Req::default().judge_in(&fw, &counts);
    assert_eq!(Req::default().judge_in(&fw, &counts).decision, Decision::Deny { persist: Some(Duration::from_secs(300)) });
    // With 429 and a hold, retry-after is the hold.
    let fw = Firewall::new(&config(json!({"rules": [limited("/", rate("rate_limit"), Some("10m"))]}))).unwrap();
    let counts = Counts::new();
    Req::default().judge_in(&fw, &counts);
    let d = Req::default().judge_in(&fw, &counts).decision;
    assert_eq!(d.persist(), Some(Duration::from_secs(600)));
    assert!(matches!(d, Decision::RateLimited { retry_after, .. } if retry_after == Duration::from_secs(600)));
    // Over it with log: recorded, and the rules after it run.
    let fw = Firewall::new(&config(json!({"rules": [limited("/", rate("log"), None), deny_admin]}))).unwrap();
    let counts = Counts::new();
    let req = Req { path: "/x", ..Req::default() };
    assert_eq!(req.judge_in(&fw, &counts).decision, Decision::Allow);
    let v = req.judge_in(&fw, &counts);
    assert_eq!((v.decision.name(), v.rule.as_deref()), ("log", Some("limit")));
    assert_eq!(Req { path: "/admin", ..Req::default() }.judge_in(&fw, &counts).decision.name(), "deny");
}

#[test]
fn an_apps_rate_limit_question_runs_only_the_rules_naming_its_id() {
    let api_rule = json!({
        "id": "update", "name": "update",
        "conditionGroup": [{"conditions": [{"type": "rate_limit_api_id", "op": "eq", "value": "Update-Object"}, {"type": "header", "key": "x-rr-plan", "op": "neq", "value": "pro"}]}],
        "action": {"mitigate": {"action": "rate_limit", "rateLimit": {"algo": "fixed_window", "window": 60, "limit": 2, "keys": ["ip"]}}}
    });
    let deny_all = json!({"id": "all", "name": "all", "conditionGroup": [{"conditions": [{"type": "path", "op": "pre", "value": "/"}]}], "action": {"mitigate": {"action": "deny"}}});
    let fw = Firewall::new(&config(json!({"rules": [deny_all, api_rule], "ips": [{"hostname": "*", "ip": "203.0.113.0/24", "action": "deny"}]}))).unwrap();
    assert!(fw.knows_api("update-object"));
    assert!(!fw.knows_api("other"));
    let counts = Counts::new();
    let ask = |key: &'static str| Req { path: "/.well-known/forkrail/rate-limit-api/update-object", api: Some(("update-object", key)), ..Req::default() };
    // The IP block and the deny-everything rule aren't about the app's question; the key is the client.
    assert_eq!(decisions(&fw, &ask("user-1"), &counts, 3), vec!["", "", "rate_limit"]);
    assert_eq!(ask("user-2").judge_in(&fw, &counts).decision, Decision::Allow);
    // The rule's other conditions read the question's own headers.
    assert_eq!(decisions(&fw, &ask("user-1").header("x-rr-plan", "pro"), &counts, 1), vec![""]);
    // A visitor's request never matches a rate_limit_api_id condition.
    assert_eq!(Req { ip: "198.51.100.1", ..Req::default() }.judge_in(&fw, &counts).decision.name(), "deny");
    // Off: the id isn't known.
    let mut off = config(json!({"rules": [{"name": "u", "conditionGroup": [{"conditions": [{"type": "rate_limit_api_id", "op": "eq", "value": "x"}]}], "action": {"mitigate": {"action": "rate_limit", "rateLimit": {"algo": "fixed_window", "window": 60, "limit": 2, "keys": ["ip"]}}}}]}));
    assert!(Firewall::new(&off).unwrap().knows_api("x"));
    off.firewall_enabled = false;
    assert!(!Firewall::new(&off).unwrap().knows_api("x"));
}

#[test]
fn buckets_expire_once_they_would_start_over() {
    let fixed = RateSpec { algo: Algo::FixedWindow, window_ns: 60_000_000_000, limit: 5 };
    let mut b = Bucket::default();
    let now = 1_800_000_030 * 1_000_000_000;
    b.hit(true, fixed, now);
    assert_eq!(b.expires_s, 1_800_000_060);
    let bucket = RateSpec { algo: Algo::TokenBucket, window_ns: 10_000_000_000, limit: 10 };
    let mut b = Bucket::default();
    b.hit(true, bucket, now);
    b.hit(false, bucket, now);
    // Two tokens out, one a second back.
    assert_eq!(b.expires_s, 1_800_000_032);
    // The largest limit in the shortest window: a token every microsecond, still counted.
    let fast = RateSpec { algo: Algo::TokenBucket, window_ns: 10_000_000_000, limit: 10_000_000 };
    let mut b = Bucket::default();
    assert!(!b.hit(true, fast, now).over);
    assert_eq!(std::mem::size_of::<Bucket>(), 16);
}

#[test]
fn rate_limits_count_quickly() {
    // Five rate limits on every request, each counted in a map as the router does (without its lock).
    let mut rules = Vec::new();
    for i in 0..5 {
        rules.push(json!({"name": format!("l{i}"), "conditionGroup": [{"conditions": [{"type": "path", "op": "pre", "value": "/"}]}],
            "action": {"mitigate": {"action": "rate_limit", "rateLimit": {"algo": if i % 2 == 0 { "fixed_window" } else { "token_bucket" }, "window": 60, "limit": 10_000_000, "keys": ["ip", "header:x-api-key"]}}}}));
    }
    let fw = Firewall::new(&config(json!({"rules": rules}))).unwrap();
    let counts = Counts::new();
    let req = Req { path: "/api/listings/123", ..Req::default() }.header("x-api-key", "k_1234567890");
    let n = 20_000;
    let start = std::time::Instant::now();
    for _ in 0..n {
        assert_eq!(req.judge_in(&fw, &counts).decision, Decision::Allow);
    }
    let each = start.elapsed() / n;
    eprintln!("5 rate limits: {each:?} a request");
    if !cfg!(debug_assertions) {
        assert!(each < Duration::from_micros(20), "{each:?}");
    }
}

#[test]
fn a_challenge_answers_until_solved_then_the_rules_after_it_run() {
    let fw = Firewall::new(&config(json!({
        "rules": [
            {"id": "ch", "name": "ch", "conditionGroup": [{"conditions": [{"type": "path", "op": "pre", "value": "/"}]}], "action": {"mitigate": {"action": "challenge", "actionDuration": "10m"}}},
            {"id": "admin", "name": "admin", "conditionGroup": [{"conditions": [{"type": "path", "op": "pre", "value": "/admin"}]}], "action": {"mitigate": {"action": "deny"}}}
        ],
        "ips": [{"id": "suspect", "hostname": "*", "ip": "198.51.100.0/24", "action": "challenge"}]
    })))
    .unwrap();
    let v = Req::default().judge(&fw);
    assert_eq!((v.decision.clone(), v.rule.as_deref()), (Decision::Challenge { persist: Some(Duration::from_secs(600)) }, Some("ch")));
    assert_eq!(Req { passed: true, ..Req::default() }.judge(&fw).decision, Decision::Allow);
    assert_eq!(Req { passed: true, path: "/admin", ..Req::default() }.judge(&fw).decision.name(), "deny");
    // An IP block's challenge comes first, without a hold.
    let v = Req { ip: "198.51.100.9", ..Req::default() }.judge(&fw);
    assert_eq!((v.decision, v.rule.as_deref()), (Decision::Challenge { persist: None }, Some("suspect")));
    // A rate limit's: over the limit, unless solved.
    let fw = Firewall::new(&config(json!({"rules": [limited("/", json!({"algo": "fixed_window", "window": 60, "limit": 1, "keys": ["ip"], "action": "challenge"}), None)]}))).unwrap();
    let counts = Counts::new();
    assert_eq!(Req::default().judge_in(&fw, &counts).decision, Decision::Allow);
    assert_eq!(Req::default().judge_in(&fw, &counts).decision.name(), "challenge");
    assert_eq!(Req { passed: true, ..Req::default() }.judge_in(&fw, &counts).decision, Decision::Allow);
}
