//! Verified crawlers and webhook senders: the address ranges their operators publish, so attack
//! mode can let them by without a challenge they couldn't solve. A crawler is known by where it
//! connects from, never by its user agent (anyone can claim to be Googlebot).
//!
//! The lists (`SOURCES`) are fetched once a day and kept in `FORKRAIL_BOTS_DIR` (default
//! `/var/lib/forkrail/bots`): a restart uses the saved copy at once, and fetches only when it's a
//! day old (a router restarts with every release; GitHub allows 60 unauthenticated requests an
//! hour). A list that can't be fetched keeps its last copy, and is tried again in an hour; one
//! never fetched is simply missing (its crawlers get the challenge). `FORKRAIL_BOTS=0` fetches
//! nothing (local runs).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use forkrail_common::firewall::{Cidr, IpSet};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use tracing::{info, warn};

pub const DEFAULT_DIR: &str = "/var/lib/forkrail/bots";
const FILE: &str = "bots.json";
const EVERY: Duration = Duration::from_secs(24 * 3600);
const RETRY: Duration = Duration::from_secs(3600);
const MAX_BYTES: usize = 4 << 20;

/// Who, and where they publish their addresses: search engines' crawlers and the fetchers that
/// act for their users, and webhook senders a site under attack still needs.
const SOURCES: &[(&str, &str)] = &[
    ("google", "https://developers.google.com/static/crawling/ipranges/common-crawlers.json"),
    ("google-special", "https://developers.google.com/static/crawling/ipranges/special-crawlers.json"),
    ("google-fetchers", "https://developers.google.com/static/crawling/ipranges/user-triggered-fetchers.json"),
    ("google-fetchers-google", "https://developers.google.com/static/crawling/ipranges/user-triggered-fetchers-google.json"),
    ("bing", "https://www.bing.com/toolbox/bingbot.json"),
    ("apple", "https://search.developer.apple.com/applebot.json"),
    ("duckduckgo", "https://duckduckgo.com/duckduckbot.json"),
    ("openai-search", "https://openai.com/searchbot.json"),
    ("openai-user", "https://openai.com/chatgpt-user.json"),
    ("stripe-webhooks", "https://stripe.com/files/ips/ips_webhooks.json"),
    ("github-webhooks", "https://api.github.com/meta"),
];

#[derive(Default)]
pub struct Bots {
    set: RwLock<Arc<IpSet<()>>>,
}

impl Bots {
    /// Whether `ip` is in a published range.
    pub fn verified(&self, ip: IpAddr) -> bool {
        let set = self.set.read().unwrap().clone();
        !set.is_empty() && set.contains(ip)
    }

    fn install(&self, lists: &BTreeMap<String, Vec<String>>) -> usize {
        let mut set = IpSet::default();
        let mut n = 0;
        for cidr in lists.values().flatten().filter_map(|c| Cidr::parse(c).ok()) {
            set.insert(cidr, ());
            n += 1;
        }
        *self.set.write().unwrap() = Arc::new(set);
        n
    }

    /// Keep the lists until the router exits: the saved copy first, then fresh ones every day.
    pub async fn follow(self: Arc<Self>, dir: PathBuf) {
        let path = dir.join(FILE);
        let mut lists: BTreeMap<String, Vec<String>> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if !lists.is_empty() {
            let n = self.install(&lists);
            info!(ranges = n, lists = lists.len(), "verified crawlers from the saved copy");
            let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
            if let Some(age) = age.filter(|a| *a < EVERY) {
                tokio::time::sleep(EVERY - age).await;
            }
        }
        let connector = match hyper_rustls::HttpsConnectorBuilder::new().with_provider_and_platform_verifier(rustls::crypto::ring::default_provider()) {
            Ok(b) => b.https_only().enable_http1().build(),
            Err(e) => {
                warn!(error = %e, "no HTTPS client for the crawler lists; attack mode challenges crawlers too");
                return;
            }
        };
        let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);
        loop {
            let mut failed = Vec::new();
            for (name, url) in SOURCES {
                match fetch(&client, url).await.and_then(|body| ranges(&body)) {
                    Ok(found) if !found.is_empty() => {
                        lists.insert((*name).to_owned(), found);
                    }
                    Ok(_) => failed.push(format!("{name}: no ranges")),
                    Err(e) => failed.push(format!("{name}: {e}")),
                }
            }
            let n = self.install(&lists);
            save(&path, &lists);
            if failed.is_empty() {
                info!(ranges = n, lists = lists.len(), "verified crawlers updated");
            } else {
                warn!(ranges = n, failed = failed.join("; "), "verified crawlers updated, some lists not");
            }
            tokio::time::sleep(if failed.is_empty() { EVERY } else { RETRY }).await;
        }
    }
}

type Https = Client<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>, Empty<Bytes>>;

/// GET `url`, following up to three redirects.
async fn fetch(client: &Https, url: &str) -> Result<Bytes, String> {
    let mut url = url.to_owned();
    for _ in 0..4 {
        let request = hyper::Request::get(&url)
            .header(hyper::header::USER_AGENT, "forkrail-router (verified crawler ranges)")
            .header(hyper::header::ACCEPT, "application/json")
            .body(Empty::new())
            .map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(Duration::from_secs(20), client.request(request)).await.map_err(|_| "timed out".to_owned())?.map_err(|e| e.to_string())?;
        if response.status().is_redirection() {
            let to = response.headers().get(hyper::header::LOCATION).and_then(|v| v.to_str().ok()).ok_or("a redirect without a location")?;
            url = if to.starts_with('/') { format!("{}{to}", origin(&url)) } else { to.to_owned() };
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let body = http_body_util::Limited::new(response.into_body(), MAX_BYTES).collect().await.map_err(|e| e.to_string())?;
        return Ok(body.to_bytes());
    }
    Err("too many redirects".into())
}

fn origin(url: &str) -> &str {
    let after = url.find("://").map_or(0, |i| i + 3);
    url[after..].find('/').map_or(url, |i| &url[..after + i])
}

/// The ranges in a published list: `prefixes[].ipv4Prefix|ipv6Prefix` (Google, Bing, Apple,
/// DuckDuckGo, OpenAI), `WEBHOOKS` (Stripe) or `hooks` (GitHub).
fn ranges(body: &[u8]) -> Result<Vec<String>, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| format!("not JSON: {e}"))?;
    let mut out = Vec::new();
    if let Some(prefixes) = v.get("prefixes").and_then(|p| p.as_array()) {
        for p in prefixes {
            for key in ["ipv4Prefix", "ipv6Prefix"] {
                if let Some(c) = p.get(key).and_then(|c| c.as_str()) {
                    out.push(c.to_owned());
                }
            }
        }
    }
    for key in ["WEBHOOKS", "hooks"] {
        if let Some(list) = v.get(key).and_then(|l| l.as_array()) {
            out.extend(list.iter().filter_map(|c| c.as_str()).map(str::to_owned));
        }
    }
    out.retain(|c| Cidr::parse(c).is_ok());
    Ok(out)
}

fn save(path: &Path, lists: &BTreeMap<String, Vec<String>>) {
    let Some(dir) = path.parent() else { return };
    let tmp = dir.join(format!(".{FILE}.{}", std::process::id()));
    let written = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&tmp, serde_json::to_vec(lists).unwrap_or_default())).and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        std::fs::remove_file(&tmp).ok();
        warn!(path = %path.display(), error = %e, "couldn't keep the crawler lists on disk");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_published_shape_reads() {
        let google = br#"{"creationTime": "x", "prefixes": [{"ipv6Prefix": "2001:4860:4801:10::/64"}, {"ipv4Prefix": "66.249.64.0/27"}]}"#;
        assert_eq!(ranges(google).unwrap(), vec!["2001:4860:4801:10::/64", "66.249.64.0/27"]);
        assert_eq!(ranges(br#"{"WEBHOOKS": ["3.18.12.63", "nope"]}"#).unwrap(), vec!["3.18.12.63"]);
        assert_eq!(ranges(br#"{"hooks": ["192.30.252.0/22"], "web": ["1.2.3.4/32"]}"#).unwrap(), vec!["192.30.252.0/22"]);
        assert!(ranges(b"<html>").is_err());
        assert_eq!(origin("https://developers.google.com/static/x.json"), "https://developers.google.com");
    }

    #[test]
    fn a_verified_address_is_one_in_a_list() {
        let bots = Bots::default();
        assert!(!bots.verified("66.249.64.5".parse().unwrap()));
        let lists = [("google".to_owned(), vec!["66.249.64.0/27".to_owned()])].into();
        assert_eq!(bots.install(&lists), 1);
        assert!(bots.verified("66.249.64.5".parse().unwrap()));
        assert!(!bots.verified("66.249.65.5".parse().unwrap()));
        let dir = std::env::temp_dir().join(format!("forkrail-bots-{}", forkrail_common::random_hex(4)));
        save(&dir.join(FILE), &lists);
        let back: BTreeMap<String, Vec<String>> = serde_json::from_slice(&std::fs::read(dir.join(FILE)).unwrap()).unwrap();
        assert_eq!(back, lists);
        std::fs::remove_dir_all(dir).ok();
    }
}
