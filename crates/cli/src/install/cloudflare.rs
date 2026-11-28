//! The install's domain on Cloudflare: checking the token the person gave, finding the zone that
//! holds the domain, and the DNS records the install makes there (and removes on uninstall).
//!
//! The token is sent only to Cloudflare's API, in a header, and never printed or logged. Records
//! the installer makes carry [`COMMENT`], so uninstall removes only those.

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use hyper::Method;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::fetch::Http;

const API: &str = "https://api.cloudflare.com/client/v4";
/// On every record the installer makes.
pub const COMMENT: &str = "forkrail install";

pub struct Cloudflare {
    http: Http,
    token: String,
}

/// The zone that holds the install's domain.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
    /// `active` once the domain's nameservers are Cloudflare's; `pending` before.
    pub status: String,
    #[serde(default)]
    pub name_servers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Record {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub proxied: bool,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Deserialize)]
struct Answer {
    success: bool,
    #[serde(default)]
    errors: Vec<Value>,
    #[serde(default)]
    result: Value,
}

impl Cloudflare {
    pub fn new(token: &str) -> Result<Self> {
        Ok(Self { http: Http::new()?, token: token.trim().to_owned() })
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let auth = format!("Bearer {}", self.token);
        let headers = [("authorization", auth.as_str()), ("content-type", "application/json")];
        let body = body.map(|b| Bytes::from(b.to_string()));
        let (status, text) = self.http.exchange(method, &format!("{API}{path}"), &headers, body).await.context("Cloudflare didn't answer")?;
        let answer: Answer = serde_json::from_slice(&text).with_context(|| format!("Cloudflare answered HTTP {status} with something that isn't its API's JSON"))?;
        if !answer.success {
            let errors: Vec<String> = answer
                .errors
                .iter()
                .map(|e| format!("{} {}", e.get("code").map(Value::to_string).unwrap_or_default(), e.get("message").and_then(Value::as_str).unwrap_or("")))
                .collect();
            bail!("Cloudflare HTTP {status}: {}", errors.join("; "));
        }
        Ok(answer.result)
    }

    /// Whether the token is valid and active.
    pub async fn verify(&self) -> Result<()> {
        let result = self.call(Method::GET, "/user/tokens/verify", None).await.context("checking the Cloudflare token")?;
        match result.get("status").and_then(Value::as_str) {
            Some("active") => Ok(()),
            other => bail!("the Cloudflare token isn't active (status {})", other.unwrap_or("unknown")),
        }
    }

    /// The zone holding `domain`: the domain itself, or the nearest parent the token can see.
    pub async fn zone_for(&self, domain: &str) -> Result<Option<Zone>> {
        for candidate in parents(domain) {
            let result = self.call(Method::GET, &format!("/zones?name={candidate}"), None).await?;
            let zones: Vec<Zone> = serde_json::from_value(result).context("reading Cloudflare's zones")?;
            if let Some(zone) = zones.into_iter().next() {
                return Ok(Some(zone));
            }
        }
        Ok(None)
    }

    /// The records named `name` (exactly), of any type.
    pub async fn records(&self, zone: &str, name: &str) -> Result<Vec<Record>> {
        let result = self.call(Method::GET, &format!("/zones/{zone}/dns_records?name.exact={}", encode(name)), None).await?;
        serde_json::from_value(result).context("reading Cloudflare's records")
    }

    /// `name` A `ip`, DNS only, TTL 60: made, or changed if the installer made it before. A record of
    /// the name that the installer didn't make is left alone and reported.
    pub async fn set_a(&self, zone: &str, name: &str, ip: &str) -> Result<()> {
        let existing = self.records(zone, name).await?;
        if let Some(other) = existing.iter().find(|r| r.comment.as_deref() != Some(COMMENT)) {
            bail!("{name} already has a {} record ({}) that the installer didn't make; remove it in Cloudflare or pick another domain", other.kind, other.content);
        }
        let body = json!({"type": "A", "name": name, "content": ip, "ttl": 60, "proxied": false, "comment": COMMENT});
        match existing.first() {
            Some(own) if own.content == ip && own.kind == "A" => Ok(()),
            Some(own) => self.call(Method::PUT, &format!("/zones/{zone}/dns_records/{}", own.id), Some(body)).await.map(drop),
            None => self.call(Method::POST, &format!("/zones/{zone}/dns_records"), Some(body)).await.map(drop),
        }
    }

    pub async fn delete(&self, zone: &str, id: &str) -> Result<()> {
        self.call(Method::DELETE, &format!("/zones/{zone}/dns_records/{id}"), None).await.map(drop)
    }
}

/// `a.b.example.com`, then `b.example.com`, then `example.com`: never a bare top-level label.
pub fn parents(domain: &str) -> Vec<String> {
    let labels: Vec<&str> = domain.trim_end_matches('.').split('.').collect();
    (0..labels.len().saturating_sub(1)).map(|i| labels[i..].join(".")).collect()
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The page where a person makes a token for one zone's DNS, with the permission filled in.
/// Cloudflare's page for a new API token with everything filled in (DNS edit and zone read, on
/// every zone of the person's accounts, named forkrail): Continue to summary, then Create Token.
/// Limiting it to the one zone is a choice under Zone Resources on the same page.
pub const TOKEN_PAGE: &str = "https://dash.cloudflare.com/profile/api-tokens?permissionGroupKeys=%5B%7B%22key%22%3A%22dns%22%2C%22type%22%3A%22edit%22%7D%2C%7B%22key%22%3A%22zone%22%2C%22type%22%3A%22read%22%7D%5D&accountId=%2A&zoneId=all&name=forkrail";

/// Where a domain goes on Cloudflare.
pub const ADD_SITE_PAGE: &str = "https://dash.cloudflare.com/?to=/:account/add-site";

/// For a domain whose DNS is elsewhere: what to do first.
pub const MOVE_DNS: &str = "forkrail sets up your apps' addresses and HTTPS certificates by itself through Cloudflare's DNS, which is free. Your domain stays with the company you bought it from; only its DNS moves:
  1. Add the domain to Cloudflare (free plan): https://dash.cloudflare.com/?to=/:account/add-site
  2. Where you bought the domain (GoDaddy, Namecheap, Route 53, ...), replace its nameservers with the two Cloudflare shows you.
  3. When Cloudflare says the domain is active (usually within an hour), run `forkrail install` again.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents_walk_up_to_the_registered_domain() {
        assert_eq!(parents("apps.example.com"), vec!["apps.example.com", "example.com"]);
        assert_eq!(parents("example.com"), vec!["example.com"]);
        assert_eq!(parents("x.co.uk"), vec!["x.co.uk", "co.uk"]);
    }

    #[test]
    fn names_are_encoded() {
        assert_eq!(encode("*.example.com"), "%2A.example.com");
        assert_eq!(encode("forkrail install"), "forkrail%20install");
    }
}
