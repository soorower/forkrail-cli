//! The loop guard: how deep a chain of a function's calls back to its own install has gone.
//!
//! A function that calls its own URL (or another project's of the same install, which calls it
//! back) can multiply its requests without end, each one a running worker: AWS's own loop
//! detection doesn't see calls over HTTP. Vercel stops these with 508 `INFINITE_LOOP_DETECTED`,
//! by a header its functions' outgoing calls carry. So here:
//!
//! - The router marks every request it passes on (`HEADER`: the chain, the request's depth in it,
//!   and an HMAC of both). A request without a good mark starts a chain of its own (its request id)
//!   at depth 0.
//! - The shims copy the mark of the request a function is serving onto its outgoing calls to the
//!   install's own hosts (`forkrail_common::loops`), and only those: calls elsewhere go out as the
//!   app made them. The router's own calls to its install's hosts (a rewrite to a URL that is one
//!   of them) carry it as well.
//! - A request whose good mark says depth `d` is at `d + 1`; past `max_depth` it's answered 508,
//!   and nothing of the deployment runs for it. The call fails at once, its function's request
//!   usually fails with it, and the chain unwinds.
//! - A depth limit alone lets a call that makes k more grow a chain as k to the depth (10 a level
//!   at depth 5 is 111,111 requests for one visitor's). So a chain may also have at most
//!   `max_chain` requests at depth 2 and deeper, counted on this router; past that, each is
//!   answered 508 too. Calls straight from the first request (depth 1: a schedule's run calling its
//!   own API for each of thousands of items) aren't counted.
//!
//! Anything else in the header is ignored and replaced. The key is derived from the routers' shared
//! challenge key, so a chain crossing regions keeps its depth; with a key of a router's own it
//! holds on that router only. A mark someone copies can only make their own requests deeper.
//!
//! Not seen: calls an app makes by raw sockets, through a subprocess (`curl`) or a program the shims
//! don't run in, by IP address, or through a service elsewhere. The spend budget stops those.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use ring::hmac;

pub use forkrail_common::loops::HEADER;

/// Vercel's own limit isn't documented (reports put it near 4); a Next.js page fetching its own
/// API route is at depth 1.
pub const DEFAULT_MAX_DEPTH: u32 = 5;
/// Requests at depth 2 and deeper one chain may have.
pub const DEFAULT_MAX_CHAIN: u32 = 1000;
/// A chain is forgotten this long after its first counted request: no request lives longer than
/// Lambda's 15 minutes.
const CHAIN_LIFE: Duration = Duration::from_secs(16 * 60);
/// Chains kept at once; past it the expired go, and if that isn't enough, all of them (the count
/// starts again rather than the router's memory growing).
const MAX_CHAINS: usize = 100_000;

pub struct Marks {
    key: hmac::Key,
    max_depth: u32,
    max_chain: u32,
    /// Chain → when its first counted request came, and how many have.
    chains: Mutex<HashMap<String, (Instant, u32)>>,
}

/// What a request is, by the mark it came with.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Within the limit: the mark to send with it.
    Pass { mark: String, depth: u32 },
    /// Past the depth limit, at this depth.
    Deep { depth: u32 },
    /// Past the number of requests a chain may have at depth 2 and deeper.
    Wide { depth: u32 },
}

impl Marks {
    pub fn new(key: hmac::Key, max_depth: u32, max_chain: u32) -> Self {
        Self { key, max_depth, max_chain, chains: Mutex::default() }
    }

    /// With a key of this process's own.
    pub fn local(max_depth: u32, max_chain: u32) -> Self {
        let mut secret = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut secret).expect("the system's randomness");
        Self::new(hmac::Key::new(hmac::HMAC_SHA256, &secret), max_depth, max_chain)
    }

    pub fn max_depth(&self) -> u32 {
        self.max_depth
    }

    pub fn max_chain(&self) -> u32 {
        self.max_chain
    }

    /// Count one more request at depth 2 or deeper in `chain`: false past the limit.
    fn counted(&self, chain: &str) -> bool {
        let now = Instant::now();
        let mut chains = self.chains.lock().unwrap();
        if chains.len() >= MAX_CHAINS && !chains.contains_key(chain) {
            chains.retain(|_, (first, _)| now.duration_since(*first) < CHAIN_LIFE);
            if chains.len() >= MAX_CHAINS {
                chains.clear();
            }
        }
        let entry = chains.entry(chain.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= CHAIN_LIFE {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 <= self.max_chain
    }

    /// The request with id `request_id`, which came with `incoming` in `HEADER` (if anything).
    pub fn step(&self, incoming: Option<&str>, request_id: &str) -> Step {
        let (chain, depth) = match incoming.and_then(|value| self.read(value)) {
            Some((chain, depth)) => (chain, depth.saturating_add(1)),
            None => (request_id.to_owned(), 0),
        };
        if depth > self.max_depth {
            return Step::Deep { depth };
        }
        if depth >= 2 && !self.counted(&chain) {
            return Step::Wide { depth };
        }
        Step::Pass { mark: self.mark(&chain, depth), depth }
    }

    /// `<chain>.<depth>.<hex HMAC>`.
    pub fn mark(&self, chain: &str, depth: u32) -> String {
        let tag = hmac::sign(&self.key, message(chain, depth).as_bytes());
        format!("{chain}.{depth}.{}", forkrail_common::hex(tag.as_ref()))
    }

    /// The chain and depth a mark names, if it's a router's.
    pub fn read(&self, value: &str) -> Option<(String, u32)> {
        let mut parts = value.trim().splitn(3, '.');
        let chain = parts.next()?;
        if chain.is_empty() || chain.len() > 64 || !chain.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let depth: u32 = parts.next()?.parse().ok()?;
        let tag = unhex(parts.next()?)?;
        hmac::verify(&self.key, message(chain, depth).as_bytes(), &tag).ok()?;
        Some((chain.to_owned(), depth))
    }
}

fn message(chain: &str, depth: u32) -> String {
    format!("forkrail-loop\0{chain}\0{depth}")
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// The install's own hosts, as the current routing table has them: kept by the registry on each
/// refresh, sent in each worker's payload.
static OWN_HOSTS: LazyLock<RwLock<Arc<Vec<String>>>> = LazyLock::new(Default::default);

pub fn own_hosts() -> Arc<Vec<String>> {
    OWN_HOSTS.read().unwrap().clone()
}

pub fn set_own_hosts(hosts: Vec<String>) {
    let mut own = OWN_HOSTS.write().unwrap();
    if **own != hosts {
        *own = Arc::new(hosts);
    }
}

/// Whether `host` is one of the install's own.
pub fn is_own(host: &str) -> bool {
    forkrail_common::loops::own_host(&own_hosts(), host)
}

/// `own_host`'s entries for a table: everything under the install's base domain
/// (`*.forkrail.com`, which holds every deployment's and preview's host), and every other host and
/// custom domain by its exact name (a wildcard domain's entry already is one). Never a host's
/// parent domain: an apex custom domain (`example.io`) would make all of `.io` the install's, and
/// a call to any of it would carry the mark.
pub fn own_hosts_of(routes: &forkrail_common::routes::Routes, base_domain: Option<&str>) -> Vec<String> {
    let mut own = std::collections::BTreeSet::new();
    if let Some(base) = base_domain {
        own.insert(format!("*.{base}"));
    }
    let under_base = |host: &str| base_domain.is_some_and(|base| host.ends_with(base) && host.len() > base.len() && host.as_bytes()[host.len() - base.len() - 1] == b'.');
    for host in routes.hosts.keys().chain(routes.domains.keys()) {
        let host = host.split(':').next().unwrap_or(host).trim_end_matches('.').to_ascii_lowercase();
        if !host.is_empty() && !under_base(&host) {
            own.insert(host);
        }
    }
    own.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_deepens_then_stops() {
        let marks = Marks::local(2, 1000);
        let Step::Pass { mark, depth: 0 } = marks.step(None, "ab12") else { panic!() };
        assert!(mark.starts_with("ab12.0."));
        let Step::Pass { mark, depth: 1 } = marks.step(Some(&mark), "cd34") else { panic!() };
        assert!(mark.starts_with("ab12.1."), "the chain keeps its first request's id");
        let Step::Pass { mark, depth: 2 } = marks.step(Some(&mark), "ef56") else { panic!() };
        assert_eq!(marks.step(Some(&mark), "0789"), Step::Deep { depth: 3 });
    }

    #[test]
    fn a_chain_widens_only_so_far_below_its_first_calls() {
        let marks = Marks::local(5, 3);
        let root = marks.mark("ab12", 0);
        // Any number of calls straight from the first request.
        for i in 0..50 {
            assert!(matches!(marks.step(Some(&root), &format!("{i:04x}")), Step::Pass { depth: 1, .. }));
        }
        let one = marks.mark("ab12", 1);
        for _ in 0..3 {
            assert!(matches!(marks.step(Some(&one), "cd34"), Step::Pass { depth: 2, .. }));
        }
        assert_eq!(marks.step(Some(&one), "cd34"), Step::Wide { depth: 2 });
        assert_eq!(marks.step(Some(&marks.mark("ab12", 3)), "cd34"), Step::Wide { depth: 4 });
        // Another chain has its own count.
        assert!(matches!(marks.step(Some(&marks.mark("ef56", 1)), "cd34"), Step::Pass { depth: 2, .. }));
    }

    #[test]
    fn anything_but_a_good_mark_starts_a_chain() {
        let marks = Marks::local(5, 1000);
        let other = Marks::local(5, 1000);
        let forged = other.mark("ab12", 4);
        for value in [forged.as_str(), "ab12.4.00", "ab12.4", "not a mark", "", "zz.1.aa"] {
            let Step::Pass { mark, depth: 0 } = marks.step(Some(value), "fe01") else { panic!("{value}") };
            assert!(mark.starts_with("fe01.0."), "{value}");
        }
        // A good mark with its depth changed is no longer good.
        let good = marks.mark("ab12", 1);
        let raised = good.replacen(".1.", ".0.", 1);
        assert_eq!(marks.read(&raised), None);
        assert_eq!(marks.read(&good), Some(("ab12".into(), 1)));
    }

    #[test]
    fn own_hosts_of_a_table() {
        let routes: forkrail_common::routes::Routes = serde_json::from_value(serde_json::json!({
            "deployments": {},
            "hosts": {"orbit.forkrail.com": "a", "orbit-1a2b.forkrail.com": "a", "localhost": "b", "nodeapp.1-2-3-4.sslip.io": "c", "example.net": "d"},
            "domains": {"atlas.example.net": {"project": "atlas"}, "*.example.com": {"project": "x"}, "example.net": {"project": "d"}}
        }))
        .unwrap();
        assert_eq!(
            own_hosts_of(&routes, Some("forkrail.com")),
            ["*.example.com", "*.forkrail.com", "atlas.example.net", "example.net", "localhost", "nodeapp.1-2-3-4.sslip.io"],
            "an apex domain is itself, never all of .net; another host is its own name, never all of sslip.io"
        );
        let own = own_hosts_of(&routes, Some("forkrail.com"));
        assert!(!forkrail_common::loops::own_host(&own, "api.github.io"));
        assert!(!forkrail_common::loops::own_host(&own, "other.sslip.io"));
        assert!(!forkrail_common::loops::own_host(&own, "www.example.net"), "a sibling of a custom domain isn't the install's");
        assert!(forkrail_common::loops::own_host(&own, "nodeapp-9f8e.forkrail.com"), "a preview's host, though the table may not list it");
        // Without a base domain (a bench or local router), every host by name.
        assert_eq!(
            own_hosts_of(&routes, None),
            ["*.example.com", "atlas.example.net", "example.net", "localhost", "nodeapp.1-2-3-4.sslip.io", "orbit-1a2b.forkrail.com", "orbit.forkrail.com"]
        );
    }
}
