//! Deployment protection (C3): whether a request may reach a protected deployment.
//!
//! The router checks; the dashboard grants. A browser without access gets a random nonce in an
//! HttpOnly cookie for that host and is sent to the dashboard's `/protection` page with the
//! nonce's SHA-256 (`nh`), which (for a team member or the password) signs a token naming `nh` and
//! sends the browser back to `https://<host>/_forkrail/auth?token=<JWT>&path=<path>`. The router
//! checks the token and that the browser holds the nonce, keeps the token as an HttpOnly cookie
//! for that host only, and redirects to the path. The token works only beside its nonce, so one
//! read from a URL, a history or a log opens nothing elsewhere. Tokens are EdDSA JWTs
//! signed by the dashboard; routers hold only the public keys, so a router can't grant access.
//!
//! Two layers, each with its own scope of hosts:
//! - sign-in (`scope`): a team member's or the password's token, or a share link's;
//! - trusted IPs (`trusted_ips`): only listed addresses, signed in or not; others get 404.
//!
//! Past both: `OPTIONS` to allowlisted paths; automation's bypass secret
//! (`x-forkrail-protection-bypass`, as a header or a query parameter;
//! `x-forkrail-set-bypass-cookie` keeps it as a cookie, by a redirect); and a share link, a token in
//! the URL's `_forkrail_share` parameter, kept as the access cookie by a redirect without it. The
//! tester's port is never protected.
//!
//! The same names with `vercel` for `forkrail` are accepted too, never shown: an app moved from
//! Vercel keeps working with what its CI already sends, as `x-vercel-ip-*` and `VERCEL=1` do.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use forkrail_common::protection::{Cidr, Protection, ProtectionScope, TOKEN_MAX_S, TrustedScope};
use forkrail_common::routes::{Routes, one_label_under};
use forkrail_common::rules::convert::percent_decode;
use hyper::header::{COOKIE, HeaderValue};
use hyper::{HeaderMap, Method};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where the dashboard sends a browser back with its token.
pub const AUTH_PATH: &str = "/_forkrail/auth";
/// The access token, kept for one host.
pub const TOKEN_COOKIE: &str = "_forkrail_jwt";
/// A bypass secret kept as a cookie at automation's request.
pub const BYPASS_COOKIE: &str = "_forkrail_bypass";
/// The secrets that tie a sign-in to the browser that began it: a few at once (`.`-separated),
/// for sign-ins begun in several tabs. Only their SHA-256 (`nh`) leaves the browser, in the
/// dashboard's address and then the token, so a token in a leaked URL or log is no use without
/// this cookie.
pub const NONCE_COOKIE: &str = "_forkrail_nonce";
const MAX_NONCES: usize = 4;
const BYPASS_NAMES: [&str; 2] = ["x-forkrail-protection-bypass", "x-vercel-protection-bypass"];
const SET_BYPASS_COOKIE_NAMES: [&str; 2] = ["x-forkrail-set-bypass-cookie", "x-vercel-set-bypass-cookie"];
/// A share link's token, in the deployment's own URL.
const SHARE_NAMES: [&str; 2] = ["_forkrail_share", "_vercel_share"];
/// A token from a clock a little ahead of ours is still good.
const CLOCK_SKEW_S: u64 = 60;
/// Longer tokens are refused unread.
const MAX_TOKEN_LEN: usize = 4096;

/// The install's verifying keys and where browsers go for access.
struct Keys {
    keys: Vec<UnparsedPublicKey<Vec<u8>>>,
    login: Option<String>,
}

/// One project's protection, ready to check requests against.
pub struct Gate {
    project: String,
    scope: ProtectionScope,
    members: bool,
    password_version: Option<u32>,
    trusted: Option<(TrustedScope, Vec<Cidr>)>,
    options_paths: Vec<String>,
    exceptions: HashSet<String>,
    bypass: Vec<[u8; 32]>,
    revoked_before: u64,
    revoked_shares: HashSet<String>,
    keys: Arc<Keys>,
}

/// A project's gate as it applies to one host: which layers cover it.
#[derive(Clone)]
pub struct Guard {
    pub gate: Arc<Gate>,
    /// Sign-in is needed.
    pub auth: bool,
    /// Only trusted addresses may in.
    pub ip: bool,
}

/// Every project's gate, built with each new table.
#[derive(Default)]
pub struct Gates {
    gates: HashMap<String, Arc<Gate>>,
    base: Option<String>,
}

impl Gates {
    /// The gates of `routes`: each project's own settings, else the install's default scope.
    pub fn build(routes: &Routes, base: Option<&str>) -> Gates {
        let base = base.map(str::to_owned);
        // Without the install's own settings there's no default, and no key a sign-in or share link
        // could be checked with: a project's own settings still hold, and only the ways in that need
        // no key (bypass secrets, trusted addresses) open it. Never a host left open.
        let config = routes.protection.as_ref();
        let keys = Arc::new(Keys {
            keys: config
                .into_iter()
                .flat_map(|c| c.keys.iter())
                .filter_map(|k| URL_SAFE_NO_PAD.decode(k).ok())
                .map(|k| UnparsedPublicKey::new(&ED25519, k))
                .collect(),
            login: config.and_then(|c| c.login.clone()),
        });
        let default = Protection { scope: config.map_or(ProtectionScope::Off, |c| c.default), ..Protection::default() };
        let mut gates = HashMap::new();
        for d in routes.deployments.values() {
            if gates.contains_key(&d.project) {
                continue;
            }
            let own = routes.projects.get(&d.project).and_then(|p| p.protection.as_ref());
            let settings = own.unwrap_or(&default);
            if settings.scope != ProtectionScope::Off || settings.trusted_ips.is_some() {
                gates.insert(d.project.clone(), Arc::new(Gate::new(&d.project, settings, keys.clone())));
            }
        }
        Gates { gates, base }
    }

    /// What a request to `host` (normalised) of `project` must pass, if anything.
    pub fn for_host(&self, routes: &Routes, host: &str, project: &str) -> Option<Guard> {
        let gate = self.gates.get(project)?;
        if gate.exceptions.contains(host) {
            return None;
        }
        let production = self.production_name(routes, host, project);
        let auth = match gate.scope {
            ProtectionScope::Off => false,
            ProtectionScope::All => true,
            ProtectionScope::Standard => !production,
        };
        let ip = gate.trusted.as_ref().is_some_and(|(scope, _)| match scope {
            TrustedScope::All => true,
            TrustedScope::Standard => !production,
            TrustedScope::Production => production,
        });
        (auth || ip).then(|| Guard { gate: gate.clone(), auth, ip })
    }

    fn production_name(&self, routes: &Routes, host: &str, project: &str) -> bool {
        production_name(routes, self.base.as_deref(), host, project)
    }

    /// Whether a visitor let in at `host` (not a production name) may be served `project`'s
    /// deployment `id` there (skew protection's pin). Only when what let them in holds as much for
    /// that deployment's own address: a sign-in or share link opens one host, and an exception
    /// makes one host public, so neither reaches another deployment; trusted addresses may, when
    /// they guard that deployment's address alike.
    pub fn pin_reaches(&self, routes: &Routes, host: &str, project: &str, id: &str) -> bool {
        let Some(gate) = self.gates.get(project) else { return true };
        let Some(base) = &self.base else { return false };
        let there = self.for_host(routes, &format!("{project}-{id}.{base}"), project);
        if gate.exceptions.contains(host) {
            return there.is_none();
        }
        let here = self.for_host(routes, host, project);
        if here.as_ref().is_some_and(|g| g.auth) {
            return false;
        }
        there.is_none_or(|t| !t.auth && (!t.ip || here.is_some_and(|h| h.ip)))
    }
}

/// Whether `host` is one of `project`'s production names: `<project>.<base>`, or a custom domain
/// that follows production. What standard protection leaves open.
pub fn production_name(routes: &Routes, base: Option<&str>, host: &str, project: &str) -> bool {
    let own = match base {
        Some(base) => one_label_under(host, base) && host.split('.').next() == Some(project),
        None => host.split('.').next() == Some(project),
    };
    own || routes.domain(host).is_some_and(|(_, d)| d.project == project && d.production())
}

/// What a request gets.
#[derive(Debug, PartialEq, Eq)]
pub enum Access {
    Granted,
    /// Set this cookie and go to `location` (the same URL, without the parameter that asked).
    Redirect { cookie: HeaderValue, location: String },
    /// Sign-in needed.
    Denied,
    /// Not from a trusted address: 404, as if there were no such deployment.
    NotFound,
}

/// The parts of a request the gate looks at.
pub struct Ask<'a> {
    pub host: &'a str,
    pub method: &'a Method,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub headers: &'a HeaderMap,
    pub client: IpAddr,
    /// Unix seconds.
    pub now: u64,
}

#[derive(Debug, Deserialize)]
struct Header {
    alg: String,
}

/// An access token's claims.
#[derive(Debug, Deserialize)]
struct Claims {
    /// The one host it opens.
    aud: String,
    project: String,
    /// How access was given: `member`, `password` or `share`.
    via: String,
    /// The password's version, for `password`.
    #[serde(default)]
    pv: Option<u32>,
    /// The share link's id, for `share`.
    #[serde(default)]
    share: Option<String>,
    /// SHA-256 (hex) of the nonce the sign-in began with: the token works only beside the
    /// `NONCE_COOKIE` holding it. Share links and `forkrail curl`'s tokens have none.
    #[serde(default)]
    nh: Option<String>,
    iat: u64,
    exp: u64,
}

impl Claims {
    fn shared(&self) -> bool {
        self.via == "share"
    }

    /// Whether it may be used with these request headers: unbound, or bound to a nonce they hold.
    fn bound_to(&self, headers: &HeaderMap) -> bool {
        match &self.nh {
            None => true,
            Some(nh) => nonces(headers).iter().any(|n| forkrail_common::hex(&Sha256::digest(n.as_bytes())) == *nh),
        }
    }
}

/// The nonces the browser holds.
fn nonces(headers: &HeaderMap) -> Vec<String> {
    let mut held: Vec<String> = Vec::new();
    for v in cookies(headers, NONCE_COOKIE) {
        for n in v.split('.').filter(|n| n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit())) {
            if !held.iter().any(|h| h == n) {
                held.push(n.to_owned());
            }
        }
    }
    held
}

impl Guard {
    /// Whether `ask` may in, and how.
    pub fn check(&self, ask: &Ask) -> Access {
        let gate = &self.gate;
        if *ask.method == Method::OPTIONS && plain_path(ask.path) && gate.options_paths.iter().any(|p| ask.path.starts_with(p.as_str())) {
            return Access::Granted;
        }
        // A share link in the URL: kept as the access cookie, and the URL loses it.
        if let Some((name, token)) = SHARE_NAMES.iter().find_map(|n| query_param(ask.query, n).map(|t| (*n, t)))
            && let Ok(claims) = gate.verify(&token, ask.host, ask.now)
            && claims.shared()
        {
            let max_age = claims.exp.saturating_sub(ask.now).min(TOKEN_MAX_S);
            if let Ok(cookie) = HeaderValue::from_str(&format!("{TOKEN_COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax")) {
                return Access::Redirect { cookie, location: same_url_without(ask, &[name]) };
            }
        }
        if !gate.bypass.is_empty() {
            let presented = BYPASS_NAMES
                .iter()
                .find_map(|n| ask.headers.get(*n).and_then(|v| v.to_str().ok()).map(str::to_owned))
                .or_else(|| BYPASS_NAMES.iter().find_map(|n| query_param(ask.query, n)));
            let kept = cookies(ask.headers, BYPASS_COOKIE);
            if let Some(secret) = &presented
                && gate.bypasses(secret)
            {
                let wanted = SET_BYPASS_COOKIE_NAMES
                    .iter()
                    .find_map(|n| ask.headers.get(*n).and_then(|v| v.to_str().ok()).map(str::to_owned))
                    .or_else(|| SET_BYPASS_COOKIE_NAMES.iter().find_map(|n| query_param(ask.query, n)));
                // Asked to keep it, and not kept yet: a redirect sets it. Once the
                // cookie holds it, requests that keep sending both headers simply pass.
                if !kept.iter().any(|k| k == secret)
                    && let Some(cookie) = wanted.and_then(|w| bypass_cookie(secret, &w))
                {
                    let mut drop: Vec<&str> = BYPASS_NAMES.to_vec();
                    drop.extend(SET_BYPASS_COOKIE_NAMES);
                    return Access::Redirect { cookie, location: same_url_without(ask, &drop) };
                }
                return Access::Granted;
            }
            if kept.iter().any(|s| gate.bypasses(s)) {
                return Access::Granted;
            }
        }
        let tokens: Vec<Claims> = cookies(ask.headers, TOKEN_COOKIE).iter().filter_map(|t| gate.verify(t, ask.host, ask.now).ok()).filter(|c| c.bound_to(ask.headers)).collect();
        let token = tokens.iter().find(|c| c.shared()).or(tokens.first());
        // A share link also passes the trusted-IP layer; a sign-in doesn't.
        if self.ip && !token.is_some_and(|c| c.shared()) && !gate.trusted.as_ref().is_some_and(|(_, c)| c.iter().any(|c| c.contains(ask.client))) {
            return Access::NotFound;
        }
        if self.auth && token.is_none() {
            return Access::Denied;
        }
        Access::Granted
    }

    /// A sign-in begun here: a new nonce beside the ones the browser holds (the cookie to set),
    /// and its SHA-256 for the dashboard's address.
    pub fn begin(&self, headers: &HeaderMap) -> (HeaderValue, String) {
        let nonce = forkrail_common::random_hex(16);
        let nh = forkrail_common::hex(&Sha256::digest(nonce.as_bytes()));
        let mut held = vec![nonce];
        held.extend(nonces(headers).into_iter().take(MAX_NONCES - 1));
        // As long as a sign-in's token: the token needs it for every request.
        let cookie = format!("{NONCE_COOKIE}={}; Path=/; Max-Age={TOKEN_MAX_S}; HttpOnly; Secure; SameSite=Lax", held.join("."));
        (HeaderValue::from_str(&cookie).expect("hex is a valid header"), nh)
    }

    /// The dashboard's page for access to `host` and `path` (with its query), if it has one, for
    /// the sign-in `nh` names (`begin`).
    pub fn login_url(&self, host: &str, path_and_query: &str, nh: &str) -> Option<String> {
        let login = self.gate.keys.login.as_deref()?;
        Some(format!("{login}/protection?host={}&path={}&nh={nh}", crate::geo::percent_encode(host), crate::geo::percent_encode(path_and_query)))
    }

    /// `/_forkrail/auth?token=…&path=…`: the cookie to set for a good token, and where to go. The
    /// token must be bound to a nonce this browser holds: one in another browser is refused.
    pub fn auth(&self, host: &str, query: Option<&str>, headers: &HeaderMap, now: u64) -> Result<(HeaderValue, String), &'static str> {
        let token = query_param(query, "token").ok_or("no token")?;
        let claims = self.gate.verify(&token, host, now)?;
        if claims.nh.is_none() || !claims.bound_to(headers) {
            return Err("not from this browser");
        }
        let path = query_param(query, "path").filter(|p| safe_path(p)).unwrap_or_else(|| "/".to_owned());
        let max_age = claims.exp.saturating_sub(now).min(TOKEN_MAX_S);
        let cookie = format!("{TOKEN_COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax");
        let cookie = HeaderValue::from_str(&cookie).map_err(|_| "bad token")?;
        Ok((cookie, path))
    }
}

impl Gate {
    fn new(project: &str, p: &Protection, keys: Arc<Keys>) -> Gate {
        Gate {
            project: project.to_owned(),
            scope: p.scope,
            members: p.members,
            password_version: p.password_version,
            trusted: p.trusted_ips.as_ref().map(|t| (t.scope, t.addresses.iter().filter_map(|c| Cidr::parse(c)).collect())),
            options_paths: p.options_paths.clone(),
            exceptions: p.exceptions.iter().cloned().collect(),
            bypass: p.bypass.iter().filter_map(|h| unhex32(h)).collect(),
            revoked_before: p.revoked_before.unwrap_or(0),
            revoked_shares: p.revoked_shares.iter().cloned().collect(),
            keys,
        }
    }

    fn bypasses(&self, secret: &str) -> bool {
        let hash: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        self.bypass.contains(&hash)
    }

    /// The claims of `token` if it's good for `host` now.
    fn verify(&self, token: &str, host: &str, now: u64) -> Result<Claims, &'static str> {
        if token.len() > MAX_TOKEN_LEN {
            return Err("token too long");
        }
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(signature), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err("not a JWT");
        };
        let header: Header = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).map_err(|_| "bad header")?).map_err(|_| "bad header")?;
        if header.alg != "EdDSA" {
            return Err("not EdDSA");
        }
        let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| "bad signature")?;
        let signed = &token[..token.len() - token.rsplit('.').next().map_or(0, str::len) - 1];
        if !self.keys.keys.iter().any(|k| k.verify(signed.as_bytes(), &signature).is_ok()) {
            return Err("bad signature");
        }
        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).map_err(|_| "bad claims")?).map_err(|_| "bad claims")?;
        if claims.aud != host || claims.project != self.project {
            return Err("for another host");
        }
        // A sign-in lasts 12 h; a share link until it's taken back or expires.
        let too_long = !claims.shared() && claims.exp.saturating_sub(claims.iat) > TOKEN_MAX_S + CLOCK_SKEW_S;
        if claims.exp <= now || claims.iat > now + CLOCK_SKEW_S || too_long {
            return Err("expired");
        }
        if claims.iat < self.revoked_before {
            return Err("revoked");
        }
        let current = match claims.via.as_str() {
            "member" => self.members,
            "password" => claims.pv.is_some() && claims.pv == self.password_version,
            "share" => claims.share.as_ref().is_some_and(|s| !self.revoked_shares.contains(s)),
            _ => false,
        };
        if !current {
            return Err("revoked");
        }
        Ok(claims)
    }
}

/// The request's path and query, without the parameters `drop` names.
fn same_url_without(ask: &Ask, drop: &[&str]) -> String {
    let kept: Vec<&str> = ask
        .query
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty() && !drop.contains(&pair.split_once('=').map_or(*pair, |(n, _)| n)))
        .collect();
    // One leading slash: `//evil.example/` as a Location is another host.
    let path = format!("/{}", ask.path.trim_start_matches('/'));
    if kept.is_empty() { path } else { format!("{path}?{}", kept.join("&")) }
}

/// A path with no `.` or `..` segment (written plainly or as `%2e`) and no empty one: what a
/// prefix of it says is where it goes. An app resolving `/api/../admin` would serve `/admin`.
fn plain_path(path: &str) -> bool {
    !path.contains("//")
        && path.split('/').all(|segment| {
            let segment = segment.to_ascii_lowercase().replace("%2e", ".");
            segment != "." && segment != ".."
        })
}

/// The bypass cookie automation asked for: `true` for a same-site one, `samesitenone` for one
/// sent from other sites' frames too.
fn bypass_cookie(secret: &str, wanted: &str) -> Option<HeaderValue> {
    let same_site = match wanted {
        "true" => "Lax",
        "samesitenone" => "None",
        _ => return None,
    };
    if !secret.bytes().all(|b| b.is_ascii_graphic() && b != b';' && b != b',') {
        return None;
    }
    HeaderValue::from_str(&format!("{BYPASS_COOKIE}={secret}; Path=/; HttpOnly; Secure; SameSite={same_site}")).ok()
}

/// A path to redirect to on this host: one slash, no scheme or other host.
fn safe_path(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//") && !path.starts_with("/\\") && !path.bytes().any(|b| b.is_ascii_control())
}

/// A query parameter's first value, decoded.
fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (n, v) = pair.split_once('=').unwrap_or((pair, ""));
        (n == name).then(|| percent_decode(&v.replace('+', " ")))
    })
}

/// A cookie's value from the request's `Cookie` headers.
/// Every value of the cookie `name`. Any app on the install's domain can set one for the whole
/// domain, which the browser may send before ours: each is tried, so a decoy locks nobody out.
fn cookies(headers: &HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| {
            let (n, v) = c.trim().split_once('=')?;
            (n == name).then(|| v.to_owned())
        })
        .collect()
}

/// Take the router's own cookies out of the request before the app sees it.
pub fn strip_cookies(headers: &mut HeaderMap) {
    let values: Vec<HeaderValue> = headers.get_all(COOKIE).iter().cloned().collect();
    let ours = |n: &str| n == TOKEN_COOKIE || n == BYPASS_COOKIE || n == NONCE_COOKIE;
    if !values.iter().any(|v| v.to_str().is_ok_and(|s| s.contains(TOKEN_COOKIE) || s.contains(BYPASS_COOKIE) || s.contains(NONCE_COOKIE))) {
        return;
    }
    headers.remove(COOKIE);
    for value in values {
        let Ok(text) = value.to_str() else {
            headers.append(COOKIE, value);
            continue;
        };
        let kept: Vec<&str> = text
            .split(';')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .filter(|c| c.split_once('=').is_none_or(|(n, _)| !ours(n)))
            .collect();
        if !kept.is_empty()
            && let Ok(v) = HeaderValue::from_str(&kept.join("; "))
        {
            headers.append(COOKIE, v);
        }
    }
}

fn unhex32(hex: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    if hex.len() != 64 {
        return None;
    }
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forkrail_common::protection::{ProtectionConfig, TrustedIps};
    use forkrail_common::routes::{CustomDomain, Deployment, ProjectRouting};
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    const NOW: u64 = 1_800_000_000;
    const HOST: &str = "shop-a1.forkrail.com";

    fn keypair() -> Ed25519KeyPair {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    fn sign(key: &Ed25519KeyPair, claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signed = format!("{header}.{payload}");
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(key.sign(signed.as_bytes())))
    }

    fn member(host: &str) -> serde_json::Value {
        serde_json::json!({"aud": host, "project": "shop", "via": "member", "iat": NOW - 10, "exp": NOW + 3600})
    }

    fn share(host: &str, id: &str) -> serde_json::Value {
        serde_json::json!({"aud": host, "project": "shop", "via": "share", "share": id, "iat": NOW - 86_400 * 30, "exp": NOW + 86_400 * 365})
    }

    fn deployment(project: &str) -> Deployment {
        serde_json::from_value(serde_json::json!({"project": project, "function": "f", "qualifier": "1"})).unwrap()
    }

    fn table(key: &Ed25519KeyPair, protection: Option<Protection>, default: ProtectionScope) -> Routes {
        let mut routes = Routes::default();
        routes.deployments.insert("a1".into(), deployment("shop"));
        routes.deployments.insert("b2".into(), deployment("blog"));
        routes.hosts.insert("shop.forkrail.com".into(), "a1".into());
        routes.hosts.insert(HOST.into(), "a1".into());
        routes.hosts.insert("shop.example.com".into(), "a1".into());
        routes.hosts.insert("blog-b2.forkrail.com".into(), "b2".into());
        routes.domains.insert("shop.example.com".into(), CustomDomain::new("shop"));
        routes.domains.insert("staging.example.com".into(), CustomDomain { branch: Some("staging".into()), ..CustomDomain::new("shop") });
        routes.protection = Some(ProtectionConfig {
            keys: vec![URL_SAFE_NO_PAD.encode(key.public_key().as_ref())],
            login: Some("https://dash.forkrail.com".into()),
            default,
        });
        if let Some(p) = protection {
            routes.projects.insert("shop".into(), ProjectRouting { protection: Some(p), ..Default::default() });
        }
        routes.validate().unwrap();
        routes
    }

    fn with(key: &Ed25519KeyPair, p: Protection) -> Routes {
        table(key, Some(p), ProtectionScope::Off)
    }

    fn guard(routes: &Routes, host: &str) -> Option<Guard> {
        let project = routes.lookup(host).map(|(_, d)| d.project.clone()).unwrap_or_else(|| "shop".into());
        Gates::build(routes, Some("forkrail.com")).for_host(routes, host, &project)
    }

    fn ask<'a>(headers: &'a HeaderMap, query: Option<&'a str>) -> Ask<'a> {
        Ask { host: HOST, method: &Method::GET, path: "/p", query, headers, client: "198.51.100.1".parse().unwrap(), now: NOW }
    }

    fn with_cookie(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_str(value).unwrap());
        h
    }

    fn trusted(scope: TrustedScope) -> Option<TrustedIps> {
        Some(TrustedIps { scope, addresses: vec!["10.0.0.0/8".into()] })
    }

    #[test]
    fn scopes() {
        let key = keypair();
        let routes = with(&key, Protection::default());
        assert!(guard(&routes, "shop.forkrail.com").is_none());
        assert!(guard(&routes, "shop.example.com").is_none());
        assert!(guard(&routes, HOST).is_some_and(|g| g.auth && !g.ip));
        assert!(guard(&routes, "staging.example.com").is_some(), "a branch's domain is a preview");
        assert!(guard(&routes, "blog-b2.forkrail.com").is_none(), "no settings, default off");
        let all = with(&key, Protection { scope: ProtectionScope::All, ..Protection::default() });
        assert!(guard(&all, "shop.forkrail.com").is_some() && guard(&all, "shop.example.com").is_some());
        assert!(guard(&with(&key, Protection { scope: ProtectionScope::Off, ..Protection::default() }), HOST).is_none());
        let excepted = with(&key, Protection { exceptions: vec![HOST.into()], ..Protection::default() });
        assert!(guard(&excepted, HOST).is_none());
        // The install's default covers projects without settings.
        let default = table(&key, None, ProtectionScope::Standard);
        assert!(guard(&default, "blog-b2.forkrail.com").is_some() && guard(&default, "shop.forkrail.com").is_none());
        let mut none = default.clone();
        none.protection = None;
        assert!(guard(&none, "blog-b2.forkrail.com").is_none());
        // A project's own settings hold without the install's: no key, so nobody signs in.
        let mut own_only = with(&key, Protection { scope: ProtectionScope::All, ..Protection::default() });
        own_only.protection = None;
        let g = guard(&own_only, HOST).expect("still guarded");
        assert!(g.auth && g.gate.keys.keys.is_empty());
        // Trusted IPs have a scope of their own, production alone included.
        let prod_ips = with(&key, Protection { scope: ProtectionScope::Off, trusted_ips: trusted(TrustedScope::Production), ..Protection::default() });
        assert!(guard(&prod_ips, "shop.forkrail.com").is_some_and(|g| g.ip && !g.auth));
        assert!(guard(&prod_ips, HOST).is_none());
        let both = with(&key, Protection { trusted_ips: trusted(TrustedScope::All), ..Protection::default() });
        assert!(guard(&both, HOST).is_some_and(|g| g.ip && g.auth));
        assert!(guard(&both, "shop.forkrail.com").is_some_and(|g| g.ip && !g.auth));
    }

    #[test]
    fn a_pin_reaches_only_what_the_hosts_access_opens() {
        let key = keypair();
        let reaches = |routes: &Routes, host: &str, id: &str| Gates::build(routes, Some("forkrail.com")).pin_reaches(routes, host, "shop", id);
        let branch = "shop-git-main.forkrail.com";
        // Nothing guards the project: anywhere.
        assert!(reaches(&table(&key, None, ProtectionScope::Off), branch, "c3"));
        // Standard protection: a preview's host opens with a sign-in or a share link, for that host alone.
        let standard = table(&key, None, ProtectionScope::Standard);
        assert!(!reaches(&standard, branch, "c3"));
        assert!(!reaches(&standard, HOST, "c3"));
        // An exception makes one host public, not another deployment, unless its own address is public too.
        let one = with(&key, Protection { scope: ProtectionScope::Standard, exceptions: vec![branch.into()], ..Protection::default() });
        assert!(!reaches(&one, branch, "c3"));
        let both = with(&key, Protection { scope: ProtectionScope::Standard, exceptions: vec![branch.into(), "shop-c3.forkrail.com".into()], ..Protection::default() });
        assert!(reaches(&both, branch, "c3"));
        // Trusted addresses alone guard every preview's address alike.
        let addresses = with(&key, Protection { scope: ProtectionScope::Off, trusted_ips: trusted(TrustedScope::Standard), ..Protection::default() });
        assert!(reaches(&addresses, branch, "c3"));
    }

    #[test]
    fn tokens() {
        let key = keypair();
        let g = guard(&with(&key, Protection { password_version: Some(2), ..Protection::default() }), HOST).unwrap();
        let gate = &g.gate;
        let good = sign(&key, member(HOST));
        assert!(gate.verify(&good, HOST, NOW).is_ok());
        assert_eq!(g.check(&ask(&with_cookie(&format!("a=1; {TOKEN_COOKIE}={good}")), None)), Access::Granted);
        assert_eq!(g.check(&ask(&HeaderMap::new(), None)), Access::Denied);
        assert_eq!(gate.verify(&good, "shop-b9.forkrail.com", NOW).unwrap_err(), "for another host");
        assert_eq!(gate.verify(&sign(&keypair(), member(HOST)), HOST, NOW).unwrap_err(), "bad signature");
        assert_eq!(gate.verify(&good, HOST, NOW + 3600).unwrap_err(), "expired");
        let mut c = member(HOST);
        c["iat"] = (NOW + 600).into();
        c["exp"] = (NOW + 1200).into();
        assert_eq!(gate.verify(&sign(&key, c), HOST, NOW).unwrap_err(), "expired");
        let mut c = member(HOST);
        c["exp"] = (NOW + 2 * TOKEN_MAX_S).into();
        assert_eq!(gate.verify(&sign(&key, c), HOST, NOW).unwrap_err(), "expired", "a sign-in lasts at most 12 h");
        let mut c = member(HOST);
        c["project"] = "blog".into();
        assert!(gate.verify(&sign(&key, c), HOST, NOW).is_err());
        let mut it = good.split('.');
        let (h, p, s) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let mut c = member(HOST);
        c["exp"] = (NOW + 99_999).into();
        assert_eq!(gate.verify(&format!("{h}.{}.{s}", URL_SAFE_NO_PAD.encode(c.to_string())), HOST, NOW).unwrap_err(), "bad signature");
        assert_eq!(gate.verify(&format!("{}.{p}.", URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#)), HOST, NOW).unwrap_err(), "not EdDSA");
        let password = |pv: u32| sign(&key, serde_json::json!({"aud": HOST, "project": "shop", "via": "password", "pv": pv, "iat": NOW, "exp": NOW + 60}));
        assert!(gate.verify(&password(2), HOST, NOW).is_ok());
        assert_eq!(gate.verify(&password(1), HOST, NOW).unwrap_err(), "revoked");
        // A share link lives long, until it's taken back; everyone can be signed out.
        let link = sign(&key, share(HOST, "s1"));
        assert!(gate.verify(&link, HOST, NOW).is_ok());
        let revoked = with(&key, Protection { revoked_shares: vec!["s1".into()], revoked_before: Some(NOW - 5), ..Protection::default() });
        let g2 = guard(&revoked, HOST).unwrap();
        assert_eq!(g2.gate.verify(&link, HOST, NOW).unwrap_err(), "revoked");
        assert_eq!(g2.gate.verify(&good, HOST, NOW).unwrap_err(), "revoked", "issued before revoked_before");
        let g3 = guard(&with(&key, Protection { members: false, ..Protection::default() }), HOST).unwrap();
        assert!(g3.gate.verify(&good, HOST, NOW).is_err());
    }

    #[test]
    fn auth_sets_a_cookie_and_goes_back() {
        let key = keypair();
        let g = guard(&with(&key, Protection::default()), HOST).unwrap();
        let (nonce_cookie, nh) = g.begin(&HeaderMap::new());
        let jar = with_cookie(nonce_cookie.to_str().unwrap().split(';').next().unwrap());
        let mut claims = member(HOST);
        claims["nh"] = nh.clone().into();
        let token = sign(&key, claims);
        let (cookie, path) = g.auth(HOST, Some(&format!("token={token}&path=%2Fdocs%3Fq%3D1")), &jar, NOW).unwrap();
        assert_eq!(path, "/docs?q=1");
        let cookie = cookie.to_str().unwrap();
        assert!(cookie.starts_with(&format!("{TOKEN_COOKIE}={token}; Path=/; Max-Age=3600; HttpOnly; Secure")), "{cookie}");
        assert!(!cookie.contains("Domain"), "host-only");
        for bad in ["%2F%2Fevil.com", "https%3A%2F%2Fevil.com", "%2F%5Cevil.com", "evil"] {
            assert_eq!(g.auth(HOST, Some(&format!("token={token}&path={bad}")), &jar, NOW).unwrap().1, "/", "{bad}");
        }
        assert!(g.auth("shop-b9.forkrail.com", Some(&format!("token={token}")), &jar, NOW).is_err());
        assert!(g.auth(HOST, None, &jar, NOW).is_err());
        assert_eq!(
            g.login_url(HOST, "/a b?x=1", &nh).unwrap(),
            format!("https://dash.forkrail.com/protection?host=shop-a1.forkrail.com&path=%2Fa%20b%3Fx%3D1&nh={nh}")
        );
    }

    #[test]
    fn sign_ins_are_tied_to_the_browser_that_began_them() {
        let key = keypair();
        let g = guard(&with(&key, Protection::default()), HOST).unwrap();
        let (set, nh) = g.begin(&HeaderMap::new());
        let set = set.to_str().unwrap();
        assert!(set.starts_with(&format!("{NONCE_COOKIE}=")) && set.contains("HttpOnly; Secure; SameSite=Lax"), "{set}");
        let nonce = set.split(';').next().unwrap().split_once('=').unwrap().1.to_owned();
        assert_eq!(nonce.len(), 32);
        // Only the hash leaves the browser.
        assert_eq!(nh, forkrail_common::hex(&Sha256::digest(nonce.as_bytes())));
        assert!(!nh.contains(&nonce));
        let mut claims = member(HOST);
        claims["nh"] = nh.clone().into();
        let token = sign(&key, claims);
        let here = with_cookie(&format!("{NONCE_COOKIE}={nonce}"));
        let elsewhere = with_cookie(&format!("{NONCE_COOKIE}={}", "0".repeat(32)));
        let auth = |jar: &HeaderMap| g.auth(HOST, Some(&format!("token={token}")), jar, NOW).map(|_| ());
        // The way back works only in the browser holding the nonce.
        assert!(auth(&here).is_ok());
        assert_eq!(auth(&HeaderMap::new()).unwrap_err(), "not from this browser");
        assert_eq!(auth(&elsewhere).unwrap_err(), "not from this browser");
        // An unbound token (a share link's, curl's) isn't taken at the way back.
        let unbound = sign(&key, member(HOST));
        assert_eq!(g.auth(HOST, Some(&format!("token={unbound}")), &here, NOW).unwrap_err(), "not from this browser");
        // The bound token as a cookie: only beside its nonce.
        let jar = |nonce_cookie: &str| with_cookie(&format!("{TOKEN_COOKIE}={token}; {nonce_cookie}"));
        assert_eq!(g.check(&ask(&jar(&format!("{NONCE_COOKIE}={nonce}")), None)), Access::Granted);
        assert_eq!(g.check(&ask(&jar("a=1"), None)), Access::Denied);
        assert_eq!(g.check(&ask(&jar(&format!("{NONCE_COOKIE}={}", "f".repeat(32))), None)), Access::Denied);
        // Sign-ins begun in several tabs each keep their nonce, up to four.
        let mut held = here.clone();
        for _ in 0..5 {
            let (set, _) = g.begin(&held);
            held = with_cookie(set.to_str().unwrap().split(';').next().unwrap());
        }
        assert_eq!(nonces(&held).len(), MAX_NONCES);
        let (set, _) = g.begin(&here);
        assert!(set.to_str().unwrap().contains(&format!(".{nonce};")), "an earlier sign-in's nonce is kept");
    }

    #[test]
    fn share_links_in_the_url() {
        let key = keypair();
        let g = guard(&with(&key, Protection { trusted_ips: trusted(TrustedScope::All), ..Protection::default() }), HOST).unwrap();
        let empty = HeaderMap::new();
        let link = sign(&key, share(HOST, "s1"));
        let Access::Redirect { cookie, location } = g.check(&ask(&empty, Some(&format!("a=1&_vercel_share={link}&b=2")))) else { panic!("no redirect") };
        assert_eq!(location, "/p?a=1&b=2");
        assert!(cookie.to_str().unwrap().starts_with(&format!("{TOKEN_COOKIE}={link}; Path=/; Max-Age={TOKEN_MAX_S}; HttpOnly")));
        // Its cookie passes both layers, from any address.
        assert_eq!(g.check(&ask(&with_cookie(&format!("{TOKEN_COOKIE}={link}")), None)), Access::Granted);
        // A member's sign-in in the URL isn't a share link.
        let signed_in = sign(&key, member(HOST));
        assert_eq!(g.check(&ask(&empty, Some(&format!("_forkrail_share={signed_in}")))), Access::NotFound);
    }

    #[test]
    fn trusted_ips_are_a_layer_on_top() {
        let key = keypair();
        let g = guard(&with(&key, Protection { trusted_ips: trusted(TrustedScope::All), ..Protection::default() }), HOST).unwrap();
        let signed_in = with_cookie(&format!("{TOKEN_COOKIE}={}", sign(&key, member(HOST))));
        let inside: IpAddr = "10.1.2.3".parse().unwrap();
        // Outside the list: 404 even signed in.
        assert_eq!(g.check(&ask(&signed_in, None)), Access::NotFound);
        // Inside: still needs the sign-in.
        assert_eq!(g.check(&Ask { client: inside, ..ask(&HeaderMap::new(), None) }), Access::Denied);
        assert_eq!(g.check(&Ask { client: inside, ..ask(&signed_in, None) }), Access::Granted);
        // Trusted IPs alone, on production.
        let prod = with(&key, Protection { scope: ProtectionScope::Off, trusted_ips: trusted(TrustedScope::Production), ..Protection::default() });
        let g = guard(&prod, "shop.forkrail.com").unwrap();
        let at_prod = |client: &str| g.check(&Ask { host: "shop.forkrail.com", client: client.parse().unwrap(), ..ask(&HeaderMap::new(), None) });
        assert_eq!((at_prod("10.0.0.1"), at_prod("8.8.8.8")), (Access::Granted, Access::NotFound));
    }

    #[test]
    fn bypass_and_options() {
        let key = keypair();
        let secret = "s3cret-for-ci";
        let p = Protection {
            trusted_ips: trusted(TrustedScope::All),
            options_paths: vec!["/api/".into()],
            bypass: vec![forkrail_common::hex(&Sha256::digest(secret.as_bytes()))],
            ..Protection::default()
        };
        let g = guard(&with(&key, p), HOST).unwrap();
        let empty = HeaderMap::new();
        let options = Ask { method: &Method::OPTIONS, path: "/api/x", ..ask(&empty, None) };
        assert_eq!(g.check(&options), Access::Granted);
        assert_eq!(g.check(&Ask { path: "/other", ..options }), Access::NotFound);
        // A prefix of a path that resolves elsewhere is no prefix.
        for elsewhere in ["/api/../admin", "/api/%2E%2e/admin", "/api/./x", "/api//x"] {
            assert_eq!(g.check(&Ask { path: elsewhere, ..options }), Access::NotFound, "{elsewhere}");
        }
        // A share link's redirect stays on this host, however the path starts.
        let shared = Ask { path: "//evil.example/x", query: Some("_vercel_share=t&a=1"), ..ask(&empty, None) };
        assert_eq!(same_url_without(&shared, &["_vercel_share"]), "/evil.example/x?a=1");
        assert_eq!(g.check(&Ask { path: "/api/x", ..ask(&empty, None) }), Access::NotFound);
        // The secret passes both layers, as a header, a query parameter or its cookie.
        let mut h = HeaderMap::new();
        h.insert("x-forkrail-protection-bypass", HeaderValue::from_static("s3cret-for-ci"));
        assert_eq!(g.check(&ask(&h, None)), Access::Granted);
        // Vercel's spelling too, for CI moved from it.
        let mut v = HeaderMap::new();
        v.insert("x-vercel-protection-bypass", HeaderValue::from_static("s3cret-for-ci"));
        assert_eq!(g.check(&ask(&v, None)), Access::Granted);
        h.insert("x-forkrail-protection-bypass", HeaderValue::from_static("wrong"));
        assert_eq!(g.check(&ask(&h, None)), Access::NotFound);
        assert_eq!(g.check(&ask(&empty, Some("x-forkrail-protection-bypass=s3cret-for-ci"))), Access::Granted);
        assert_eq!(g.check(&ask(&with_cookie(&format!("{BYPASS_COOKIE}={secret}")), None)), Access::Granted);
        // Keeping it as a cookie is a redirect, to the URL without those parameters.
        let Access::Redirect { cookie, location } =
            g.check(&ask(&empty, Some("q=1&x-forkrail-protection-bypass=s3cret-for-ci&x-forkrail-set-bypass-cookie=samesitenone")))
        else {
            panic!("no redirect")
        };
        assert_eq!(location, "/p?q=1");
        assert_eq!(cookie.to_str().unwrap(), "_forkrail_bypass=s3cret-for-ci; Path=/; HttpOnly; Secure; SameSite=None");
        // Headers sent on every request: once the cookie holds the secret, no more redirects.
        let mut h = with_cookie(&format!("{BYPASS_COOKIE}={secret}"));
        h.insert("x-forkrail-protection-bypass", HeaderValue::from_static("s3cret-for-ci"));
        h.insert("x-forkrail-set-bypass-cookie", HeaderValue::from_static("true"));
        assert_eq!(g.check(&ask(&h, None)), Access::Granted);
        h.remove(COOKIE);
        assert!(matches!(g.check(&ask(&h, None)), Access::Redirect { .. }));
    }

    #[test]
    fn the_apps_never_see_our_cookies() {
        let mut h = HeaderMap::new();
        h.append(COOKIE, HeaderValue::from_static("a=1; _forkrail_jwt=x.y.z; b=2"));
        h.append(COOKIE, HeaderValue::from_static("_forkrail_bypass=s"));
        h.append(COOKIE, HeaderValue::from_static("_forkrail_nonce=abc; c=3"));
        strip_cookies(&mut h);
        assert_eq!(h.get_all(COOKIE).iter().map(|v| v.to_str().unwrap()).collect::<Vec<_>>(), ["a=1; b=2", "c=3"]);
        let mut h = with_cookie("a=1");
        strip_cookies(&mut h);
        assert_eq!(h[COOKIE], "a=1");
    }
}
