//! Deployment protection's settings in `state/routes.json` (C3): who may open a project's
//! deployments. The router checks them on every request; the dashboard grants access by signing a
//! short-lived token the router can verify but never make (it holds only the public keys).
//!
//! - `routes.protection` (`ProtectionConfig`): the install's verifying keys, the dashboard's address
//!   browsers are sent to for access, and the scope of projects that set none.
//! - `routes.projects.<name>.protection` (`Protection`): one project's own settings.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// Which of a project's hostnames need access.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtectionScope {
    /// Every generated host (`<project>-<id>`, `<project>-git-<branch>`, branch domains); the
    /// project's production names (`<project>.<base>` and its production custom domains) stay open.
    #[default]
    Standard,
    /// Every hostname.
    All,
    Off,
}

impl ProtectionScope {
    pub fn name(self) -> &'static str {
        match self {
            ProtectionScope::Standard => "standard",
            ProtectionScope::All => "all",
            ProtectionScope::Off => "off",
        }
    }
}

/// The install's side: shared by every project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectionConfig {
    /// Ed25519 public keys that sign access tokens, each the raw 32 bytes in base64url without
    /// padding (a JWK's `x`). Two during a rotation; a token signed by any of them is good.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Where a browser without access is sent: the dashboard's origin
    /// (`https://forkrail-web.forkrail.com`), whose `/protection` page grants it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// The scope of projects without settings of their own. Off when unset, as before C3.
    #[serde(default = "off", skip_serializing_if = "is_off")]
    pub default: ProtectionScope,
}

impl Default for ProtectionConfig {
    fn default() -> Self {
        ProtectionConfig { keys: Vec::new(), login: None, default: ProtectionScope::Off }
    }
}

fn off() -> ProtectionScope {
    ProtectionScope::Off
}

fn is_off(scope: &ProtectionScope) -> bool {
    *scope == ProtectionScope::Off
}

/// A project's own protection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protection {
    #[serde(default)]
    pub scope: ProtectionScope,
    /// Any member of the project's team may sign in (forkrail Authentication).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub members: bool,
    /// The password's version while one is set: changing it makes a new version, which ends every
    /// access given by the old one. The password itself is only the dashboard's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_version: Option<u32>,
    /// Only these addresses may open the hosts its scope covers, signed in or not (a
    /// Trusted IPs): a layer on top of the others, which a bypass secret or a share link skips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_ips: Option<TrustedIps>,
    /// Path prefixes whose `OPTIONS` requests pass, for CORS preflights.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options_paths: Vec<String>,
    /// Hostnames made public whatever the scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<String>,
    /// Hex SHA-256 of each automation bypass secret.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bypass: Vec<String>,
    /// Tokens issued before this (Unix seconds) are refused: everyone signs in again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_before: Option<u64>,
    /// Share links taken back: tokens they gave are refused. The dashboard may drop an id once the
    /// last token it could have given has expired (`TOKEN_MAX_S`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revoked_shares: Vec<String>,
}

impl Default for Protection {
    fn default() -> Self {
        Protection {
            scope: ProtectionScope::Standard,
            members: true,
            password_version: None,
            trusted_ips: None,
            options_paths: Vec::new(),
            exceptions: Vec::new(),
            bypass: Vec::new(),
            revoked_before: None,
            revoked_shares: Vec::new(),
        }
    }
}

/// Which hosts the trusted-IP layer covers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustedScope {
    /// Every host but the production names, as `ProtectionScope::Standard`.
    #[default]
    Standard,
    All,
    /// Only the production names.
    Production,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedIps {
    #[serde(default)]
    pub scope: TrustedScope,
    /// CIDRs, or single addresses.
    pub addresses: Vec<String>,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// The longest a sign-in's access token may live, in seconds: 12 h. A share link's
/// lives until the link is taken back or its own expiry.
pub const TOKEN_MAX_S: u64 = 12 * 3600;

impl ProtectionConfig {
    pub fn validate(&self) -> Result<(), String> {
        for key in &self.keys {
            if !valid_key(key) {
                return Err(format!("protection key {key:?} isn't 32 bytes of base64url"));
            }
        }
        if let Some(login) = &self.login
            && !valid_origin(login)
        {
            return Err(format!("protection login {login:?} isn't an https:// origin (http:// for localhost)"));
        }
        Ok(())
    }
}

impl Protection {
    pub fn validate(&self) -> Result<(), String> {
        for cidr in self.trusted_ips.iter().flat_map(|t| &t.addresses) {
            Cidr::parse(cidr).ok_or_else(|| format!("trusted IP {cidr:?} isn't an address or CIDR"))?;
        }
        if self.trusted_ips.as_ref().is_some_and(|t| t.addresses.is_empty()) {
            return Err("trusted IPs need at least one address".to_owned());
        }
        if let Some(p) = self.options_paths.iter().find(|p| !p.starts_with('/') || p.len() > 1024) {
            return Err(format!("OPTIONS path {p:?} must start with /"));
        }
        if let Some(h) = self.exceptions.iter().find(|h| crate::routes::normalize_host(h) != **h || h.is_empty()) {
            return Err(format!("exception {h:?} isn't a normalised hostname"));
        }
        if let Some(b) = self.bypass.iter().find(|b| b.len() != 64 || !b.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))) {
            return Err(format!("bypass {b:?} isn't a hex SHA-256"));
        }
        Ok(())
    }
}

/// 32 bytes in base64url without padding: 43 characters, the last carrying 4 bits.
fn valid_key(key: &str) -> bool {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    key.len() == 43
        && key.bytes().all(|b| ALPHABET.contains(&b))
        && ALPHABET.iter().position(|&c| c == key.as_bytes()[42]).is_some_and(|i| i % 4 == 0)
}

/// `https://host[:port]`, nothing after; `http://` only for this machine (a dashboard in development).
fn valid_origin(url: &str) -> bool {
    let host_port = |rest: &str| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
    match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => host_port(rest),
        (_, Some(rest)) => host_port(rest) && ["localhost", "127.0.0.1"].contains(&rest.split(':').next().unwrap_or_default()),
        _ => false,
    }
}

/// An address range: `10.0.0.0/8`, `2001:db8::/32`, or one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    bits: u8,
}

impl Cidr {
    pub fn parse(text: &str) -> Option<Cidr> {
        let (addr, bits) = match text.split_once('/') {
            Some((a, b)) => (a.parse::<IpAddr>().ok()?, Some(b.parse::<u8>().ok()?)),
            None => (text.parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        (bits <= max).then_some(Cidr { addr, bits })
    }

    /// Whether `ip` is in the range. An IPv4 address mapped into IPv6 counts as IPv4.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => prefix_eq(&net.octets(), &ip.octets(), self.bits),
            (IpAddr::V6(net), IpAddr::V6(ip)) => prefix_eq(&net.octets(), &ip.octets(), self.bits),
            _ => false,
        }
    }
}

fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
    let full = usize::from(bits / 8);
    let rest = bits % 8;
    a[..full] == b[..full] && (rest == 0 || (a[full] ^ b[full]) >> (8 - rest) == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidrs() {
        let net = Cidr::parse("10.1.0.0/16").unwrap();
        assert!(net.contains("10.1.200.3".parse().unwrap()));
        assert!(!net.contains("10.2.0.1".parse().unwrap()));
        assert!(net.contains("::ffff:10.1.0.9".parse().unwrap()));
        let one = Cidr::parse("203.0.113.7").unwrap();
        assert!(one.contains("203.0.113.7".parse().unwrap()) && !one.contains("203.0.113.8".parse().unwrap()));
        let v6 = Cidr::parse("2001:db8::/33").unwrap();
        assert!(v6.contains("2001:db8:7fff::1".parse().unwrap()) && !v6.contains("2001:db8:8000::1".parse().unwrap()));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
        assert!(!Cidr::parse("0.0.0.0/0").unwrap().contains("::1".parse().unwrap()));
        for bad in ["10.0.0.0/33", "::/129", "10.0.0/8", "x", "10.0.0.0/"] {
            assert!(Cidr::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn settings_read_and_write_what_they_have() {
        let p: Protection = serde_json::from_str("{}").unwrap();
        assert_eq!(p, Protection::default());
        assert_eq!(serde_json::to_string(&p).unwrap(), r#"{"scope":"standard"}"#);
        let p: Protection = serde_json::from_str(r#"{"scope":"all","members":false,"password_version":3}"#).unwrap();
        assert_eq!((p.scope, p.members, p.password_version), (ProtectionScope::All, false, Some(3)));
        assert_eq!(serde_json::to_string(&ProtectionConfig::default()).unwrap(), "{}");
        assert_eq!(serde_json::from_str::<ProtectionConfig>("{}").unwrap().default, ProtectionScope::Off);
    }

    #[test]
    fn validation() {
        let key = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
        let ok = ProtectionConfig { keys: vec![key.into()], login: Some("https://forkrail-web.forkrail.com".into()), default: ProtectionScope::Standard };
        assert!(ok.validate().is_ok());
        for bad in [&key[1..], "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURp", "11qYAYKxCrfVS+7TyWQHOg7hcvPapiMlrwIaaPcHURo"] {
            assert!(ProtectionConfig { keys: vec![bad.into()], ..ok.clone() }.validate().is_err(), "{bad}");
        }
        assert!(ProtectionConfig { login: Some("http://localhost:3040".into()), ..ok.clone() }.validate().is_ok());
        for bad in ["http://x.com", "https://x.com/protection", "https://", "http://localhost.evil.com"] {
            assert!(ProtectionConfig { login: Some(bad.into()), ..ok.clone() }.validate().is_err(), "{bad}");
        }
        let p = Protection {
            trusted_ips: Some(TrustedIps { scope: TrustedScope::All, addresses: vec!["10.0.0.0/8".into(), "2001:db8::1".into()] }),
            options_paths: vec!["/api".into()],
            exceptions: vec!["app-git-main.forkrail.com".into()],
            bypass: vec!["ab".repeat(32)],
            ..Protection::default()
        };
        assert!(p.validate().is_ok());
        assert!(Protection { trusted_ips: Some(TrustedIps { addresses: vec!["10.0.0.0/40".into()], ..Default::default() }), ..p.clone() }.validate().is_err());
        assert!(Protection { trusted_ips: Some(TrustedIps::default()), ..p.clone() }.validate().is_err());
        assert!(Protection { options_paths: vec!["api".into()], ..p.clone() }.validate().is_err());
        assert!(Protection { exceptions: vec!["App.forkrail.com".into()], ..p.clone() }.validate().is_err());
        assert!(Protection { bypass: vec!["AB".repeat(32)], ..p.clone() }.validate().is_err());
    }
}
