//! The install's routers, which every router knows: which regions have a router, and where each
//! is on the routers' link (`cache/peers.rs`). Kept in memory and read again in the background, so
//! how a deployment's requests reach its function is settled when its pool is made
//! (`registry.rs`), and again only when the routers change, never per request:
//!
//! - Its version runs in this router's region: on this router's own workers.
//! - In another region with a router: through that router, over the link this router keeps open
//!   to it (`relay.rs`); it runs them on its own workers. The faster way (2026-11-22, skip
//!   tracing from us-west-1: queue p50 192 ms to 17).
//! - In a region with no router: invoked there from here, its workers dialing back to this router.
//!
//! Where they come from:
//!
//! - `FORKRAIL_PEERS` (`region=host:port,…`), as set by hand. It wins for the regions it names.
//! - What each router says of itself in the state bucket: one object per router,
//!   `state/routers/<region>/<host:port>`, written when it starts and every `SAY_EVERY` after, so
//!   a router added later is known to the others without a restart. One LIST reads them all
//!   (every `READ_EVERY`): the keys and their times are the whole record. One not written for
//!   `GONE_AFTER` is taken as gone. A router says where it is by `FORKRAIL_PEER_ADDR`, else its
//!   own `FORKRAIL_PEERS` entry, else the public host its workers dial (`FORKRAIL_TUNNEL_ADDR`)
//!   with the peer port.
//!
//! Several routers in one region are all known; requests for that region go to the first (the one
//! `FORKRAIL_PEERS` names, else the one heard from last). Routers of a router's own region are
//! never sent its requests: it runs that region's versions itself.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use tokio::sync::watch;
use tracing::{info, warn};

/// Where routers say where they are, in the state bucket.
pub const PREFIX: &str = "state/routers/";
/// How often a router says it's there.
const SAY_EVERY: Duration = Duration::from_secs(10 * 60);
/// How often it reads where the others are.
const READ_EVERY: Duration = Duration::from_secs(5 * 60);
/// A router not heard from for this long is taken as gone.
const GONE_AFTER: Duration = Duration::from_secs(30 * 60);

/// The routers this one knows of, itself among them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Routers {
    /// This router's region.
    pub me: String,
    /// Each region with a router, with its routers' addresses on the link (`host:port`), the one
    /// to use first. This router's own region is here even when it has no address to give.
    pub by_region: BTreeMap<String, Vec<String>>,
}

impl Routers {
    /// Just this router, in `me`.
    pub fn alone(me: &str) -> Self {
        Routers { me: me.to_owned(), by_region: BTreeMap::from([(me.to_owned(), Vec::new())]) }
    }

    /// Whether `region` has a router.
    pub fn has(&self, region: &str) -> bool {
        self.by_region.contains_key(region)
    }

    /// One address for each region that has one, this router's own included: the link's peers.
    pub fn peers(&self) -> BTreeMap<String, String> {
        self.by_region.iter().filter_map(|(region, addrs)| Some((region.clone(), addrs.first()?.clone()))).collect()
    }

    /// The other regions' routers, one address each: where this router's requests for their
    /// regions' versions go.
    pub fn others(&self) -> BTreeMap<String, String> {
        let mut peers = self.peers();
        peers.remove(&self.me);
        peers
    }

    /// The routers known from `pinned` (`FORKRAIL_PEERS`), from `heard` (what the state bucket says:
    /// region, address, how long ago it was said) and this router itself, in `me` at `own`.
    pub fn merge(me: &str, own: Option<&str>, pinned: &BTreeMap<String, String>, heard: &[(String, String, Duration)]) -> Self {
        let mut by_region: BTreeMap<String, Vec<String>> = pinned.iter().map(|(r, a)| (r.clone(), vec![a.clone()])).collect();
        let mine = by_region.entry(me.to_owned()).or_default();
        if let Some(own) = own.filter(|o| !mine.iter().any(|a| a == o)) {
            mine.push(own.to_owned());
        }
        let mut heard: Vec<&(String, String, Duration)> = heard.iter().filter(|(_, _, ago)| *ago < GONE_AFTER).collect();
        heard.sort_by_key(|(_, _, ago)| *ago);
        for (region, addr, _) in heard {
            let addrs = by_region.entry(region.clone()).or_default();
            if !addrs.contains(addr) {
                addrs.push(addr.clone());
            }
        }
        Routers { me: me.to_owned(), by_region }
    }
}

/// The region and address a key under `PREFIX` names, if it's one a router wrote.
pub fn parse_key(key: &str) -> Option<(String, String)> {
    let (region, addr) = key.strip_prefix(PREFIX)?.split_once('/')?;
    let region_ok = !region.is_empty() && region.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let addr_ok = addr.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && !host.contains('/') && port.parse::<u16>().is_ok_and(|p| p > 0));
    (region_ok && addr_ok).then(|| (region.to_owned(), addr.to_owned()))
}

/// Where this router is on the link, for the others: `FORKRAIL_PEER_ADDR`, else its own entry in
/// `FORKRAIL_PEERS`, else the host workers dial (`FORKRAIL_TUNNEL_ADDR`) with the port the link
/// listens on. None for a host others can't reach (a local run).
pub fn own_addr(explicit: Option<&str>, me: &str, pinned: &BTreeMap<String, String>, tunnel_addr: &str, peer_listen: &str) -> Option<String> {
    if let Some(addr) = explicit.map(str::trim).filter(|a| !a.is_empty()) {
        return Some(addr.to_owned());
    }
    if let Some(addr) = pinned.get(me) {
        return Some(addr.clone());
    }
    let (host, _) = tunnel_addr.rsplit_once(':')?;
    let (_, port) = peer_listen.rsplit_once(':')?;
    let local = match host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip.is_unspecified(),
        Err(_) => host.is_empty() || host == "localhost",
    };
    (!local).then(|| format!("{host}:{port}"))
}

/// The state bucket, where routers say where they are.
pub struct Bucket {
    pub client: aws_sdk_s3::Client,
    pub name: String,
}

/// The routers as this one knows them now, and how it keeps knowing.
pub struct Directory {
    me: String,
    own: Option<String>,
    pinned: BTreeMap<String, String>,
    bucket: Option<Bucket>,
    known: watch::Sender<Arc<Routers>>,
    /// Whether a failure to say or to read was logged, so it's said once until it works again.
    say_failed: AtomicBool,
    read_failed: AtomicBool,
}

impl Directory {
    pub fn new(me: String, own: Option<String>, pinned: BTreeMap<String, String>, bucket: Option<Bucket>) -> Arc<Self> {
        let first = Routers::merge(&me, own.as_deref(), &pinned, &[]);
        Arc::new(Directory {
            me,
            own,
            pinned,
            bucket,
            known: watch::Sender::new(Arc::new(first)),
            say_failed: AtomicBool::new(false),
            read_failed: AtomicBool::new(false),
        })
    }

    /// The routers now.
    pub fn get(&self) -> Arc<Routers> {
        self.known.borrow().clone()
    }

    /// The routers now, and each change after.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Routers>> {
        self.known.subscribe()
    }

    /// Say where this router is and read where the others are, once: at start, before anything
    /// uses them.
    pub async fn load(&self) {
        self.say().await;
        self.read().await;
        let known = self.get();
        info!(me = %known.me, routers = ?known.by_region, says = self.own.as_deref(), "routers known");
    }

    /// Keep saying where this router is, and reading where the others are, for good.
    pub async fn run(self: Arc<Self>) {
        if self.bucket.is_none() {
            return;
        }
        let mut read = tokio::time::interval(READ_EVERY);
        let mut say = tokio::time::interval(SAY_EVERY);
        read.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        say.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Both just happened (`load`).
        read.tick().await;
        say.tick().await;
        loop {
            tokio::select! {
                _ = read.tick() => self.read().await,
                _ = say.tick() => { self.say().await; }
            }
        }
    }

    /// Write this router's object (`PREFIX`), if it has an address to give. Whether it was written.
    async fn say(&self) -> bool {
        let (Some(bucket), Some(own)) = (&self.bucket, &self.own) else { return false };
        crate::spend::ledger().s3(forkrail_common::prices::Meter::S3Put);
        let body = serde_json::json!({ "region": self.me, "peer": own, "pid": std::process::id(), "version": env!("CARGO_PKG_VERSION") }).to_string();
        let key = format!("{PREFIX}{}/{own}", self.me);
        match bucket.client.put_object().bucket(&bucket.name).key(&key).content_type("application/json").body(ByteStream::from(body.into_bytes())).send().await {
            Ok(_) => {
                if self.say_failed.swap(false, Ordering::Relaxed) {
                    info!(key, "saying where this router is works again");
                }
                true
            }
            Err(e) => {
                if !self.say_failed.swap(true, Ordering::Relaxed) {
                    warn!(bucket = %bucket.name, key, error = %DisplayErrorContext(&e), "can't say where this router is: the other routers know of it only from FORKRAIL_PEERS");
                }
                false
            }
        }
    }

    /// List what the routers said, and take it in.
    async fn read(&self) {
        let Some(bucket) = &self.bucket else { return };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        let mut heard = Vec::new();
        let mut pages = bucket.client.list_objects_v2().bucket(&bucket.name).prefix(PREFIX).into_paginator().send();
        loop {
            let page = pages.next().await;
            if page.is_some() {
                // A LIST is priced as a PUT.
                crate::spend::ledger().s3(forkrail_common::prices::Meter::S3Put);
            }
            match page {
                None => break,
                Some(Ok(page)) => {
                    for object in page.contents() {
                        let Some((region, addr)) = object.key().and_then(parse_key) else { continue };
                        let said = object.last_modified().map_or(0, |t| t.secs());
                        heard.push((region, addr, Duration::from_secs(now.saturating_sub(said).max(0) as u64)));
                    }
                }
                Some(Err(e)) => {
                    if !self.read_failed.swap(true, Ordering::Relaxed) {
                        warn!(bucket = %bucket.name, error = %DisplayErrorContext(&e), "can't read where the other routers are: keeping what was known");
                    }
                    return;
                }
            }
        }
        if self.read_failed.swap(false, Ordering::Relaxed) {
            info!("reading where the other routers are works again");
        }
        let next = Arc::new(Routers::merge(&self.me, self.own.as_deref(), &self.pinned, &heard));
        self.known.send_if_modified(|current| {
            if **current == *next {
                return false;
            }
            info!(routers = ?next.by_region, before = ?current.by_region, "the routers changed");
            *current = next;
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned(spec: &str) -> BTreeMap<String, String> {
        crate::cache::peers::parse_peers(spec).unwrap()
    }

    #[test]
    fn keys() {
        assert_eq!(parse_key("state/routers/us-west-1/54.1.2.3:7445"), Some(("us-west-1".into(), "54.1.2.3:7445".into())));
        assert_eq!(parse_key("state/routers/eu-central-1/[2600:1f18::1]:7445"), Some(("eu-central-1".into(), "[2600:1f18::1]:7445".into())));
        assert_eq!(parse_key("state/routers/us-west-1/"), None);
        assert_eq!(parse_key("state/routers/us-west-1/host"), None);
        assert_eq!(parse_key("state/routers/us-west-1/a/b:1"), None);
        assert_eq!(parse_key("state/routers/US/1.2.3.4:1"), None);
        assert_eq!(parse_key("state/routes.json"), None);
    }

    #[test]
    fn where_a_router_says_it_is() {
        let none = BTreeMap::new();
        assert_eq!(own_addr(Some("10.0.0.1:9"), "us-east-1", &none, "1.2.3.4:7443", "0.0.0.0:7445").as_deref(), Some("10.0.0.1:9"));
        let peers = pinned("us-east-1=5.6.7.8:7445");
        assert_eq!(own_addr(None, "us-east-1", &peers, "1.2.3.4:7443", "0.0.0.0:7445").as_deref(), Some("5.6.7.8:7445"));
        assert_eq!(own_addr(None, "us-west-1", &peers, "1.2.3.4:7443", "0.0.0.0:7445").as_deref(), Some("1.2.3.4:7445"));
        // A local run says nothing.
        assert_eq!(own_addr(None, "us-west-1", &none, "127.0.0.1:7443", "0.0.0.0:7445"), None);
        assert_eq!(own_addr(None, "us-west-1", &none, "localhost:7443", "0.0.0.0:7445"), None);
    }

    #[test]
    fn known_routers() {
        let min = |m: u64| Duration::from_secs(m * 60);
        let heard = vec![
            ("us-west-1".to_owned(), "9.9.9.9:7445".to_owned(), min(2)),
            ("eu-central-1".to_owned(), "3.3.3.3:7445".to_owned(), min(40)),
            ("ap-south-1".to_owned(), "4.4.4.4:7445".to_owned(), min(20)),
            ("ap-south-1".to_owned(), "4.4.4.5:7445".to_owned(), min(1)),
        ];
        let routers = Routers::merge("us-east-1", Some("1.1.1.1:7445"), &pinned("us-west-1=2.2.2.2:7445"), &heard);
        // FORKRAIL_PEERS first, then what was said; long silent is gone; the latest heard first.
        assert_eq!(routers.by_region["us-west-1"], ["2.2.2.2:7445", "9.9.9.9:7445"]);
        assert!(!routers.has("eu-central-1"));
        assert_eq!(routers.by_region["ap-south-1"], ["4.4.4.5:7445", "4.4.4.4:7445"]);
        assert!(routers.has("us-east-1"));
        assert_eq!(routers.peers()["us-east-1"], "1.1.1.1:7445");
        let others = routers.others();
        assert_eq!(others.keys().collect::<Vec<_>>(), ["ap-south-1", "us-west-1"]);
        assert_eq!(others["us-west-1"], "2.2.2.2:7445");

        // Alone, with nothing to say: its own region still has a router.
        let alone = Routers::merge("us-east-1", None, &BTreeMap::new(), &[]);
        assert_eq!(alone, Routers::alone("us-east-1"));
        assert!(alone.has("us-east-1") && alone.others().is_empty() && alone.peers().is_empty());
    }
}
