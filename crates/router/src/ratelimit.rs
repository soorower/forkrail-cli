//! Rate-limit counters: one `Bucket` (16 bytes) per client per rule, in this router only. Vercel
//! counts per region the same way, and Global Accelerator keeps a client on the same router.
//!
//! A bucket is found by a keyed hash of (configuration, rule, client): 64 bits, so two clients
//! share one only by a chance no one can steer (the key is random per process). The map is cut
//! into shards, each behind its own lock, and capped in total (`FORKRAIL_RATE_LIMIT_BUCKETS`,
//! default 1,000,000, about 30 MB). Buckets that would start over anyway are dropped every few
//! seconds. When a shard is full of live buckets, a new client isn't counted (its request goes on)
//! and a warning says so once a minute: an attacker inventing keys can't make the router refuse
//! everyone, or grow without bound.

use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, Hasher, RandomState};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forkrail_common::firewall::{Bucket, Hit, Limiter, RateSpec};
use tracing::warn;

const SHARDS: usize = 64;
pub const DEFAULT_BUCKETS: usize = 1_000_000;
/// How often expired buckets are dropped.
pub const SWEEP_EVERY: Duration = Duration::from_secs(10);

/// The keys are hashes already.
#[derive(Default)]
struct Ident(u64);

impl Hasher for Ident {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = self.0.rotate_left(8) ^ *b as u64;
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

type Shard = HashMap<u64, Bucket, BuildHasherDefault<Ident>>;

pub struct Counters {
    shards: Box<[Mutex<Shard>]>,
    hasher: RandomState,
    per_shard: usize,
    /// Clients not counted because their shard was full, and when that was last said.
    uncounted: AtomicU64,
    warned_at: AtomicU64,
}

impl Counters {
    pub fn new(max_buckets: usize) -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
            hasher: RandomState::new(),
            per_shard: max_buckets.div_ceil(SHARDS).max(1),
            uncounted: AtomicU64::new(0),
            warned_at: AtomicU64::new(0),
        }
    }

    /// Count one request of `client` for `rule` of configuration `scope` (`None`: the install's).
    pub fn hit(&self, scope: Option<&str>, rule: &str, spec: RateSpec, client: &[u8]) -> Hit {
        self.hit_at(scope, rule, spec, client, now_ns())
    }

    fn hit_at(&self, scope: Option<&str>, rule: &str, spec: RateSpec, client: &[u8], now_ns: u64) -> Hit {
        let key = self.hasher.hash_one((scope, rule, client));
        // Neither the bits the map indexes by (low) nor those it tags with (top).
        let shard = &self.shards[(key >> 40) as usize % SHARDS];
        let mut map = shard.lock().unwrap();
        if let Some(bucket) = map.get_mut(&key) {
            return bucket.hit(false, spec, now_ns);
        }
        if map.len() >= self.per_shard {
            let now_s = (now_ns / 1_000_000_000) as u32;
            map.retain(|_, b| b.expires_s > now_s);
            if map.len() >= self.per_shard {
                drop(map);
                self.full(now_ns);
                return Hit::UNDER;
            }
        }
        let mut bucket = Bucket::default();
        let hit = bucket.hit(true, spec, now_ns);
        map.insert(key, bucket);
        hit
    }

    fn full(&self, now_ns: u64) {
        let n = self.uncounted.fetch_add(1, Ordering::Relaxed) + 1;
        let now_s = now_ns / 1_000_000_000;
        let last = self.warned_at.load(Ordering::Relaxed);
        if now_s >= last + 60 && self.warned_at.compare_exchange(last, now_s, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            warn!(uncounted = n, buckets = self.len(), "rate limits: too many clients counted at once; new ones go uncounted until old ones expire");
        }
    }

    /// Drop buckets that would start over anyway.
    pub fn sweep(&self) {
        let now_s = (now_ns() / 1_000_000_000) as u32;
        for shard in self.shards.iter() {
            shard.lock().unwrap().retain(|_, b| b.expires_s > now_s);
        }
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().len()).sum()
    }

    /// Sweep until the router exits.
    pub async fn sweep_every(self: std::sync::Arc<Self>, every: Duration) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.sweep();
        }
    }
}

/// One configuration's view of the counters.
pub struct Scoped<'a> {
    pub counters: &'a Counters,
    pub scope: Option<&'a str>,
}

impl Limiter for Scoped<'_> {
    fn hit(&self, rule: &str, spec: RateSpec, client: &[u8]) -> Hit {
        self.counters.hit(self.scope, rule, spec, client)
    }
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use forkrail_common::firewall::Algo;

    use super::*;

    const MINUTE: RateSpec = RateSpec { algo: Algo::FixedWindow, window_ns: 60_000_000_000, limit: 2 };

    #[test]
    fn clients_rules_and_configurations_count_apart() {
        let c = Counters::new(1000);
        let t = 1_800_000_000_000_000_000;
        let hits = |scope, rule, client: &[u8]| (0..3).map(|_| c.hit_at(scope, rule, MINUTE, client, t).over).collect::<Vec<_>>();
        assert_eq!(hits(Some("app"), "r", b"a"), vec![false, false, true]);
        assert_eq!(hits(Some("app"), "r", b"b"), vec![false, false, true]);
        assert_eq!(hits(Some("app"), "r2", b"a"), vec![false, false, true]);
        assert_eq!(hits(None, "r", b"a"), vec![false, false, true]);
        assert_eq!(hits(Some("other"), "r", b"a"), vec![false, false, true]);
        assert_eq!(c.len(), 5);
    }

    #[test]
    fn a_full_store_leaves_new_clients_uncounted_and_sweeps_free_room() {
        let c = Counters::new(SHARDS);
        let t = 1_800_000_000_000_000_000;
        // Fill well past one a shard: every shard fills, and new clients go uncounted.
        for i in 0..(SHARDS * 32) as u32 {
            c.hit_at(None, "r", MINUTE, &i.to_be_bytes(), t);
        }
        assert_eq!(c.len(), SHARDS);
        assert!(c.uncounted.load(Ordering::Relaxed) > 0);
        // A client that isn't counted is never refused.
        let full = (0..SHARDS * 8).map(|i| (i as u32 + 1_000_000).to_be_bytes()).find(|k| c.hit_at(None, "r", MINUTE, k, t) == Hit::UNDER && c.len() == SHARDS).unwrap();
        assert!((0..5).all(|_| !c.hit_at(None, "r", MINUTE, &full, t).over));
        // A minute later every bucket has expired: there's room again (one new client: each
        // shard holds one).
        let later = t + 61_000_000_000;
        let before = c.uncounted.load(Ordering::Relaxed);
        c.hit_at(None, "new", MINUTE, b"client", later);
        assert_eq!(c.uncounted.load(Ordering::Relaxed), before);
    }

    #[test]
    fn counting_is_quick_under_many_clients() {
        let c = Counters::new(DEFAULT_BUCKETS);
        let t = 1_800_000_000_000_000_000;
        let n = 200_000u32;
        let start = std::time::Instant::now();
        for i in 0..n {
            c.hit_at(Some("app"), "rule_abc123", MINUTE, &i.to_be_bytes(), t);
        }
        let each = start.elapsed() / n;
        eprintln!("{n} clients: {each:?} a count, {} buckets", c.len());
        if !cfg!(debug_assertions) {
            assert!(each < Duration::from_micros(2), "{each:?}");
        }
    }
}
