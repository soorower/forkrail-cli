//! A custom domain's certificate, as the issuing router last saw it:
//! `state/certs/<domain>/status.json`, beside the certificate. The router writes it after every
//! look; the CLI, the control function and the dashboard read it to say how the domain stands.

use serde::{Deserialize, Serialize};

/// The file's name in a certificate's directory.
pub const STATUS_FILE: &str = "status.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertStatus {
    pub state: CertState,
    /// How the issuer proves the domain is ours to Let's Encrypt, as of the last look: the one it
    /// used or will use. None when no way works yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<CertMethod>,
    /// When it last looked (RFC 3339), and when it looks next unless asked sooner.
    pub checked_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_check_at: Option<String>,
    /// How often it looks, in seconds. The file is written again only when what it says changes,
    /// when someone asks for a check, or every 10 minutes (an hour once issued), so `checked_at`
    /// can be older than the last look.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_s: Option<u64>,
    /// The certificate in use, if any: when it stops being valid (RFC 3339), and when it was issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<String>,
    /// Why the last attempt failed, or why none could be made, in words for the person who owns
    /// the domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the domain's own nameservers said at the last look.
    #[serde(default)]
    pub dns: DnsFindings,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertState {
    /// A certificate is in use and isn't due for renewal.
    Issued,
    /// None yet, and there's a way to get one: it's being issued, or retried soon.
    #[default]
    Pending,
    /// None, and no way to get one: DNS doesn't point here, the challenge isn't delegated, and no
    /// Cloudflare zone the install's token edits holds it.
    Invalid,
    /// One is in use but due, and renewing it failed. It keeps serving until it expires.
    RenewalFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertMethod {
    /// DNS-01, the TXT record in the domain's own Cloudflare zone, which the token edits.
    Cloudflare,
    /// DNS-01, the domain's `_acme-challenge` CNAME pointing into the install's zone, where the
    /// TXT record is written (`routes::acme_delegation`). Works before the domain points here, and
    /// for wildcards.
    Delegated,
    /// HTTP-01: the domain points here already, and every router answers the challenge on port 80.
    Http,
}

impl CertMethod {
    pub fn name(self) -> &'static str {
        match self {
            CertMethod::Cloudflare => "cloudflare",
            CertMethod::Delegated => "delegated",
            CertMethod::Http => "http",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsFindings {
    /// Its addresses (A and AAAA) are all the install's, or it's a CNAME to a name under the base
    /// domain.
    #[serde(default)]
    pub points_here: bool,
    /// What it resolves to: the CNAME target it names, if any, then the addresses at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    /// `_acme-challenge.<domain>` is a CNAME to the install's delegation name.
    #[serde(default)]
    pub delegated: bool,
    /// A Cloudflare zone the token edits holds it.
    #[serde(default)]
    pub cloudflare: bool,
    /// Its addresses are Cloudflare's proxy's: the record must be set to DNS only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub proxied: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes() {
        let status = CertStatus {
            state: CertState::Issued,
            method: Some(CertMethod::Http),
            checked_at: "2026-11-13T10:00:00Z".into(),
            not_after: Some("2027-02-11T09:00:00Z".into()),
            dns: DnsFindings { points_here: true, addresses: vec!["203.0.113.10".into()], ..Default::default() },
            ..Default::default()
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains(r#""state":"issued""#) && json.contains(r#""method":"http""#), "{json}");
        assert_eq!(serde_json::from_str::<CertStatus>(&json).unwrap(), status);
        let bare: CertStatus = serde_json::from_str(r#"{"state":"renewal_failed","checked_at":"x"}"#).unwrap();
        assert_eq!((bare.state, bare.dns.points_here), (CertState::RenewalFailed, false));
    }
}
