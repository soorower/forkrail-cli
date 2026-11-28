//! The public TLS certificates: one wildcard for the base domain (`*.forkrail.com`), which covers
//! every `<project>.<base>` and `<project>-<id>.<base>` host, and one per custom domain in the
//! routing table (`atlas.example.net`, `*.example.com`).
//!
//! Each lives in a store, `s3://bucket/prefix` or a local directory, under `<name>/`
//! (`<name>/staging/` for Let's Encrypt's staging server): `cert.pem` (the chain) and `key.pem`. The
//! base domain's directory also holds the ACME account, `account.json`. At start-up the router
//! loads them from there, and issues a new one through ACME (`acme.rs`) only if there's none or
//! less than a third of its lifetime is left, so restarts never reissue: Let's Encrypt allows 5
//! duplicate certificates a week. Every 12 hours it checks again, and swaps a renewed certificate
//! in without a restart: handshakes after that get the new one. SIGHUP renews the wildcard now
//! (a duplicate certificate, so sparingly); SIGUSR1 has custom domains check theirs, issuing only
//! what's missing or due, and so does a new `checked_at` on a domain's entry ("Check again"). A
//! failed issuance is logged and retried with backoff; the old certificate keeps serving while
//! it's valid.
//!
//! A custom domain's certificate is issued however `acme::plan` finds it can be: in its own
//! Cloudflare zone, through its delegated `_acme-challenge` CNAME, or over HTTP once it points
//! here. The issuer writes how that went to `status.json` beside the certificate
//! (`forkrail_common::domains::CertStatus`) after every look. It issues one certificate at a time,
//! and never asks Let's Encrypt about a domain more than `MAX_FAILURES` times an hour once it failed.
//! Every hour it logs `certificates` with the days left on the soonest to expire, which
//! log-export.sh turns into the `CertificateDaysLeft` metric the expiry alarm watches.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::ServerSideEncryption;
use forkrail_common::domains::{CertState, CertStatus, STATUS_FILE};
use forkrail_common::routes::{one_label_under, wildcard_above};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::sync::{Notify, mpsc, watch};
use tracing::{debug, info, warn};

use crate::BoxError;
use crate::acme::{self, Acme, Method};

/// How often the certificate is checked.
const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
/// After a failed issuance, retry after this, doubling up to `RETRY_MAX`. Let's Encrypt allows 5
/// failed validations per hostname an hour.
const RETRY_MIN: Duration = Duration::from_secs(15 * 60);
const RETRY_MAX: Duration = Duration::from_secs(6 * 3600);
/// A custom domain's first retry comes sooner: a missing zone or token permission fails before
/// Let's Encrypt is asked, and the doubling keeps real failed validations under 5 an hour.
const DOMAIN_RETRY_MIN: Duration = Duration::from_secs(5 * 60);
/// A router that doesn't issue (no ACME) looks in the store this often for a certificate it lacks,
/// such as a new custom domain's, which the issuing router stores once it has it.
const STORE_RETRY: Duration = Duration::from_secs(60);
/// And this often for a newer one, which the issuer stores when it renews.
const STORE_CHECK_EVERY: Duration = Duration::from_secs(3600);
/// A domain that has no way to a certificate yet (DNS not pointing here) is looked at again this
/// often: only its DNS is asked, no Let's Encrypt validation is spent on it. "Check again" (and the
/// dashboard, the moment it sees the records right) has it looked at at once.
const UNCONFIGURED_EVERY: Duration = Duration::from_secs(30);
/// A domain still waiting after a day (added and never set up, or moved away for good) is looked
/// at this often instead, on the issuer and in the other regions' store alike. "Check again" makes
/// it look at once and every 30 s again.
const UNCONFIGURED_SLOW: Duration = Duration::from_secs(5 * 60);
const WAITING_SLOWS_AFTER: Duration = Duration::from_secs(24 * 3600);
/// The issuer looks at the DNS of a domain whose certificate is fine this often, so a domain moved
/// away shows as such; renewal is decided at each look.
const DOMAIN_LOOK_EVERY: Duration = Duration::from_secs(5 * 60);
/// An unchanged status is written again after this: an hour for a domain whose certificate is
/// fine, 10 minutes for one that isn't. A change, or a check someone asked for, writes it at once.
/// (Written at every 30 s look, a domain left unconfigured cost $0.43 a month in S3 writes.)
const STATUS_REWRITE_EVERY: Duration = Duration::from_secs(3600);
const STATUS_REWRITE_WAITING: Duration = Duration::from_secs(10 * 60);
/// Failed issuances of one domain in the last hour after which it waits: Let's Encrypt allows 5
/// failed validations per name an hour.
const MAX_FAILURES: usize = 3;
/// How often the issuer logs the soonest expiry, for the alarm.
const EXPIRY_LOG_EVERY: Duration = Duration::from_secs(3600);

/// Where certificates are kept.
#[derive(Clone)]
pub enum Store {
    S3 { client: aws_sdk_s3::Client, bucket: String, prefix: String },
    Dir(PathBuf),
}

impl Store {
    /// `s3://bucket/prefix`, or a local directory.
    pub fn parse(spec: &str, s3: impl FnOnce() -> aws_sdk_s3::Client) -> Result<Self, BoxError> {
        match spec.strip_prefix("s3://") {
            Some(rest) => {
                let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
                if bucket.is_empty() {
                    return Err(format!("expected s3://bucket/prefix, got {spec}").into());
                }
                Ok(Store::S3 { client: s3(), bucket: bucket.to_owned(), prefix: prefix.trim_end_matches('/').to_owned() })
            }
            None => Ok(Store::Dir(PathBuf::from(spec))),
        }
    }

    /// The same store, one directory down.
    fn sub(&self, dir: &str) -> Store {
        match self {
            Store::S3 { client, bucket, prefix } => Store::S3 {
                client: client.clone(),
                bucket: bucket.clone(),
                prefix: if prefix.is_empty() { dir.to_owned() } else { format!("{prefix}/{dir}") },
            },
            Store::Dir(path) => Store::Dir(path.join(dir)),
        }
    }

    pub async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            Store::Dir(dir) => match tokio::fs::read(dir.join(name)).await {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(format!("{}: {e}", dir.join(name).display())),
            },
            Store::S3 { client, bucket, prefix } => {
                let key = format!("{prefix}/{name}");
                crate::spend::ledger().s3(forkrail_common::prices::Meter::S3Get);
                match client.get_object().bucket(bucket).key(&key).send().await {
                    Ok(out) => Ok(Some(out.body.collect().await.map_err(|e| e.to_string())?.to_vec())),
                    Err(e) if e.raw_response().is_some_and(|r| r.status().as_u16() == 404) => Ok(None),
                    Err(e) => Err(format!("s3://{bucket}/{key}: {}", DisplayErrorContext(&e))),
                }
            }
        }
    }

    pub async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<(), String> {
        match self {
            Store::Dir(dir) => {
                tokio::fs::create_dir_all(dir).await.map_err(|e| format!("{}: {e}", dir.display()))?;
                tokio::fs::write(dir.join(name), bytes).await.map_err(|e| format!("{}: {e}", dir.join(name).display()))
            }
            Store::S3 { client, bucket, prefix } => {
                let key = format!("{prefix}/{name}");
                crate::spend::ledger().s3(forkrail_common::prices::Meter::S3Put);
                client
                    .put_object()
                    .bucket(bucket)
                    .key(&key)
                    .server_side_encryption(ServerSideEncryption::Aes256)
                    .body(ByteStream::from(bytes))
                    .send()
                    .await
                    .map(drop)
                    .map_err(|e| format!("s3://{bucket}/{key}: {}", DisplayErrorContext(&e)))
            }
        }
    }

    pub async fn delete(&self, name: &str) -> Result<(), String> {
        match self {
            Store::Dir(dir) => match tokio::fs::remove_file(dir.join(name)).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", dir.join(name).display())),
                _ => Ok(()),
            },
            Store::S3 { client, bucket, prefix } => {
                let key = format!("{prefix}/{name}");
                client
                    .delete_object()
                    .bucket(bucket)
                    .key(&key)
                    .send()
                    .await
                    .map(drop)
                    .map_err(|e| format!("s3://{bucket}/{key}: {}", DisplayErrorContext(&e)))
            }
        }
    }

    /// A sibling of this store's prefix: `s3://b/state/certs` → `s3://b/state/<path>`.
    pub fn sibling(&self, path: &str) -> Store {
        match self {
            Store::S3 { client, bucket, prefix } => {
                let parent = prefix.rsplit_once('/').map_or("", |(p, _)| p);
                let prefix = if parent.is_empty() { path.to_owned() } else { format!("{parent}/{path}") };
                Store::S3 { client: client.clone(), bucket: bucket.clone(), prefix }
            }
            Store::Dir(dir) => Store::Dir(dir.parent().unwrap_or(dir).join(path)),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Store::S3 { bucket, prefix, .. } => format!("s3://{bucket}/{prefix}"),
            Store::Dir(dir) => dir.display().to_string(),
        }
    }
}

/// Picks a certificate by the name the client asks for: a custom domain's own, else the base
/// domain's wildcard for hosts one label under it, else none, which fails the handshake.
#[derive(Debug)]
pub struct Resolver {
    base: String,
    wildcard: RwLock<Option<Arc<CertifiedKey>>>,
    domains: RwLock<HashMap<String, Arc<CertifiedKey>>>,
    /// Handshakes for names it has no certificate for (scanners, mostly), and the latest such name.
    unknown: AtomicU64,
    last_unknown: std::sync::Mutex<Option<String>>,
}

impl Resolver {
    fn new(base: &str) -> Self {
        Self {
            base: base.to_owned(),
            wildcard: RwLock::new(None),
            domains: RwLock::new(HashMap::new()),
            unknown: AtomicU64::new(0),
            last_unknown: std::sync::Mutex::new(None),
        }
    }

    fn covers(&self, server_name: &str) -> bool {
        one_label_under(server_name, &self.base)
    }

    fn pick(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
        let name = server_name.trim_end_matches('.').to_ascii_lowercase();
        {
            let domains = self.domains.read().unwrap();
            if let Some(key) = domains.get(&name) {
                return Some(key.clone());
            }
            // A custom wildcard covers one label below it.
            if !domains.is_empty()
                && let Some(key) = wildcard_above(&name).and_then(|w| domains.get(&w))
            {
                return Some(key.clone());
            }
        }
        let key = if self.covers(&name) { self.wildcard.read().unwrap().clone() } else { None };
        if key.is_none() {
            self.unknown.fetch_add(1, Ordering::Relaxed);
            *self.last_unknown.lock().unwrap() = Some(name.chars().take(100).collect());
        }
        key
    }

    /// Handshakes refused for want of a certificate since the last call, and the latest name.
    pub fn take_unknown(&self) -> (u64, Option<String>) {
        (self.unknown.swap(0, Ordering::Relaxed), self.last_unknown.lock().unwrap().take())
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        match hello.server_name() {
            Some(name) => self.pick(name),
            None => {
                self.unknown.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }
}

/// A certificate as loaded: the key rustls serves, and when it's valid.
struct Loaded {
    key: Arc<CertifiedKey>,
    not_before: u64,
    not_after: u64,
}

fn load(cert_pem: &[u8], key_pem: &[u8]) -> Result<Loaded, BoxError> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<_, _>>()?;
    let leaf = chain.first().ok_or("cert.pem has no certificate")?;
    let (not_before, not_after) = validity(leaf).ok_or("can't read the certificate's validity")?;
    let key = PrivateKeyDer::from_pem_slice(key_pem)?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)?;
    let certified = CertifiedKey::new(chain, signing);
    certified.keys_match()?;
    Ok(Loaded { key: Arc::new(certified), not_before, not_after })
}

/// Whether a certificate valid from `not_before` to `not_after` (seconds since the epoch) is due
/// for renewal at `now`: a third of its lifetime or less is left.
pub fn due(not_before: u64, not_after: u64, now: u64) -> bool {
    let lifetime = not_after.saturating_sub(not_before);
    not_after.saturating_sub(now) <= lifetime / 3
}

/// One certificate the router keeps: the base domain's wildcard, or a custom domain's.
struct Slot {
    /// What it's issued for: `*.forkrail.com`, `atlas.example.net` or `*.example.com`.
    name: String,
    /// The base domain's wildcard, whose zone is the install's and which has no status file.
    base: bool,
    /// Its directory in the store.
    store: Store,
    /// The validity of the one in use.
    validity: std::sync::Mutex<Option<(u64, u64)>>,
    /// What the last look found, for `status.json`.
    status: std::sync::Mutex<CertStatus>,
    /// When Let's Encrypt last refused it, in seconds since the epoch, for `MAX_FAILURES`.
    failures: std::sync::Mutex<Vec<u64>>,
    /// The status last written, without its times, and when.
    written: std::sync::Mutex<Option<(std::time::Instant, CertStatus)>>,
}

impl Slot {
    fn new(name: &str, base: bool, store: Store) -> Self {
        Slot {
            name: name.to_owned(),
            base,
            store,
            validity: Default::default(),
            status: Default::default(),
            failures: Default::default(),
            written: Default::default(),
        }
    }

    /// Failures in the last hour, after forgetting older ones.
    fn recent_failures(&self, now: u64) -> usize {
        let mut failures = self.failures.lock().unwrap();
        failures.retain(|at| now.saturating_sub(*at) < 3600);
        failures.len()
    }
}

/// How the last look at a slot went, for when to look again.
#[derive(Debug, PartialEq, Eq)]
enum Looked {
    /// A certificate is in use and isn't due.
    Fine,
    /// No way to a certificate: DNS isn't set up. Nothing was asked of Let's Encrypt.
    Unconfigured(String),
}

pub struct Certs {
    base: String,
    /// The root of the store; each certificate has a directory under it.
    root: Store,
    acme: Option<Acme>,
    resolver: Arc<Resolver>,
    /// The base domain's wildcard. Its directory also holds the ACME account.
    wildcard: Slot,
    ready: watch::Sender<bool>,
    /// Set once the custom domains in the table at start-up have their stored certificates loaded.
    domains_loaded: watch::Sender<bool>,
    /// Asks every custom domain to check its certificate now (SIGHUP), without forcing a renewal.
    check_domains: Notify,
    /// Held while a certificate is issued: one at a time, so two challenges for the same name
    /// (`example.com` and `*.example.com`) never clean up each other's records.
    issuing: tokio::sync::Mutex<()>,
    /// Each certificate in use: when it expires, for the expiry log line.
    expiries: std::sync::Mutex<HashMap<String, u64>>,
}

impl Certs {
    /// `store` is the root; a certificate lives under `<name>/`, or `<name>/staging/` when `acme`
    /// uses Let's Encrypt's staging server: `<base>/` for the wildcard, `<domain>/` for a custom
    /// domain.
    pub fn new(base: &str, store: Store, acme: Option<Acme>) -> Arc<Self> {
        let staging = acme.as_ref().is_some_and(|a| a.staging);
        let wildcard = Slot::new(&format!("*.{base}"), true, dir_for(&store, base, staging));
        Arc::new(Self {
            base: base.to_owned(),
            root: store,
            acme,
            resolver: Arc::new(Resolver::new(base)),
            wildcard,
            ready: watch::channel(false).0,
            domains_loaded: watch::channel(false).0,
            check_domains: Notify::new(),
            issuing: tokio::sync::Mutex::new(()),
            expiries: Default::default(),
        })
    }

    pub fn resolver(&self) -> Arc<Resolver> {
        self.resolver.clone()
    }

    /// Have every custom domain check its certificate now (SIGUSR1): issue the missing ones, after
    /// the token was given a new zone, say. Never renews one that isn't due.
    pub fn check_domains_now(&self) {
        info!("custom domains: checking certificates now (SIGUSR1)");
        self.check_domains.notify_waiters();
    }

    /// Resolves once the wildcard is in use.
    pub async fn ready(&self) {
        let _ = self.ready.subscribe().wait_for(|ready| *ready).await;
    }

    /// Resolves once the custom domains the table had at start-up have their stored certificates
    /// in use (or none was stored), so HTTPS never starts without them after a restart.
    pub async fn domains_loaded(&self) {
        let _ = self.domains_loaded.subscribe().wait_for(|loaded| *loaded).await;
    }

    /// Keep a valid wildcard in use until the router exits. A message on `force` renews it now and
    /// has every custom domain check its own.
    pub async fn maintain(self: Arc<Self>, mut force: mpsc::Receiver<()>) {
        let mut retry = RETRY_MIN;
        let mut forced = false;
        loop {
            let wait = match self.check(&self.wildcard, forced).await {
                Ok(_) => {
                    retry = RETRY_MIN;
                    self.check_every()
                }
                Err(e) => {
                    let wait = if self.acme.is_some() { retry } else { STORE_RETRY };
                    warn!(base = %self.base, error = %e, retry_s = wait.as_secs(), "certificate check failed; keeping the current one");
                    retry = (retry * 2).min(RETRY_MAX);
                    wait
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => forced = false,
                Some(()) = force.recv() => {
                    info!(base = %self.base, "renewing the certificate now (SIGHUP)");
                    forced = true;
                    self.check_domains.notify_waiters();
                }
            }
        }
    }

    /// Keep a certificate for every custom domain in the routing table, until the router exits. A
    /// domain that leaves the table stops being served at once; one whose `checked_at` changes is
    /// looked at again now.
    pub async fn maintain_domains(self: Arc<Self>, mut domains: watch::Receiver<BTreeMap<String, Option<String>>>) {
        struct Task {
            abort: tokio::task::AbortHandle,
            poke: Arc<Notify>,
            checked_at: Option<String>,
        }
        let mut tasks: HashMap<String, Task> = HashMap::new();
        // First the stored certificates of the domains the table has now, all at once, before
        // HTTPS starts; each domain's task then keeps its slot.
        let mut loading = tokio::task::JoinSet::new();
        let initial = domains.borrow_and_update().clone();
        for (domain, checked_at) in initial {
            let certs = self.clone();
            loading.spawn(async move {
                let slot = certs.domain_slot(&domain);
                if let Err(e) = certs.load_stored(&slot).await {
                    warn!(%domain, error = %e, "loading the stored certificate failed");
                }
                (slot, checked_at)
            });
        }
        while let Some(loaded) = loading.join_next().await {
            if let Ok((slot, checked_at)) = loaded {
                let domain = slot.name.clone();
                let poke = Arc::new(Notify::new());
                let abort = tokio::spawn(self.clone().maintain_domain(slot, poke.clone())).abort_handle();
                tasks.insert(domain, Task { abort, poke, checked_at });
            }
        }
        self.domains_loaded.send_replace(true);
        if self.acme.is_some() {
            tokio::spawn(self.clone().log_expiries());
        }
        loop {
            let wanted = domains.borrow_and_update().clone();
            let gone: Vec<String> = tasks.keys().filter(|d| !wanted.contains_key(*d)).cloned().collect();
            for domain in gone {
                if let Some(task) = tasks.remove(&domain) {
                    task.abort.abort();
                }
                self.resolver.domains.write().unwrap().remove(&domain);
                self.expiries.lock().unwrap().remove(&domain);
                info!(%domain, "custom domain left the routing table; its certificate is no longer served");
            }
            for (domain, checked_at) in wanted {
                match tasks.entry(domain) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        let poke = Arc::new(Notify::new());
                        let abort = tokio::spawn(self.clone().maintain_domain(self.domain_slot(entry.key()), poke.clone())).abort_handle();
                        entry.insert(Task { abort, poke, checked_at });
                    }
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        if entry.get().checked_at != checked_at {
                            info!(domain = %entry.key(), "asked to check the domain again");
                            entry.get().poke.notify_one();
                            entry.get_mut().checked_at = checked_at;
                        }
                    }
                }
            }
            if domains.changed().await.is_err() {
                return;
            }
        }
    }

    /// Log the soonest expiry every hour (the issuer only), for the alarm: `certificates` with
    /// `days_left` and the certificate's name.
    async fn log_expiries(self: Arc<Self>) {
        // The first one after the certificates in the table had their first look.
        tokio::time::sleep(Duration::from_secs(120)).await;
        loop {
            let soonest = self.expiries.lock().unwrap().iter().min_by_key(|(_, to)| **to).map(|(n, to)| (n.clone(), *to));
            if let Some((name, not_after)) = soonest {
                let days_left = not_after.saturating_sub(unix_now()) as f64 / 86_400.0;
                let count = self.expiries.lock().unwrap().len();
                info!(soonest = %name, days_left = (days_left * 10.0).round() / 10.0, count, "certificates");
            }
            tokio::time::sleep(EXPIRY_LOG_EVERY).await;
        }
    }

    /// How long a certificate in use goes unchecked: the issuer renews at a third of its lifetime
    /// left, and a router that only reads the store looks for the renewed one more often.
    fn check_every(&self) -> Duration {
        if self.acme.is_some() { CHECK_EVERY } else { STORE_CHECK_EVERY }
    }

    fn domain_slot(&self, domain: &str) -> Slot {
        let staging = self.acme.as_ref().is_some_and(|a| a.staging);
        Slot::new(domain, false, dir_for(&self.root, domain, staging))
    }

    async fn maintain_domain(self: Arc<Self>, slot: Slot, poke: Arc<Notify>) {
        let domain = slot.name.clone();
        let mut retry = DOMAIN_RETRY_MIN;
        // Asked to look now: what's remembered of Cloudflare's zones is asked again too, and the
        // status is written whatever it says.
        let mut fresh = false;
        // Since when it has had no way to a certificate (no certificate yet, for a router that
        // only reads the store), while it hasn't.
        let mut waiting_since: Option<std::time::Instant> = None;
        fn waiting(since: &mut Option<std::time::Instant>, slow: Duration, normal: Duration) -> Duration {
            if since.get_or_insert_with(std::time::Instant::now).elapsed() >= WAITING_SLOWS_AFTER { slow } else { normal }
        }
        loop {
            let wait = match self.check_domain(&slot, fresh).await {
                Ok(Looked::Fine) => {
                    retry = DOMAIN_RETRY_MIN;
                    waiting_since = None;
                    if self.acme.is_some() { DOMAIN_LOOK_EVERY } else { STORE_CHECK_EVERY }
                }
                Ok(Looked::Unconfigured(why)) => {
                    let wait = waiting(&mut waiting_since, UNCONFIGURED_SLOW, UNCONFIGURED_EVERY);
                    debug!(%domain, why, retry_s = wait.as_secs(), "custom domain isn't set up for a certificate yet");
                    wait
                }
                Err(e) => {
                    let wait = if self.acme.is_some() { retry } else { waiting(&mut waiting_since, UNCONFIGURED_SLOW, STORE_RETRY) };
                    // A router that only reads the store waits for the issuer, which says why in the
                    // domain's status.json: no certificate yet is the usual case for a domain waiting
                    // on its DNS, so it's no warning here (a store that can't be read is warned of
                    // where it's read).
                    if self.acme.is_some() {
                        warn!(%domain, error = %e, retry_s = wait.as_secs(), "custom domain certificate check failed");
                    } else {
                        debug!(%domain, error = %e, retry_s = wait.as_secs(), "no certificate for the custom domain in the store yet");
                    }
                    retry = (retry * 2).min(RETRY_MAX);
                    wait
                }
            };
            if self.acme.is_some() {
                self.write_status(&slot, wait, fresh).await;
            }
            fresh = false;
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.check_domains.notified() => {
                    retry = DOMAIN_RETRY_MIN;
                    fresh = true;
                }
                _ = poke.notified() => {
                    retry = DOMAIN_RETRY_MIN;
                    fresh = true;
                }
            }
            if fresh {
                waiting_since = None;
            }
        }
    }

    /// Write what the last look found, how often it looks and when next, beside the certificate:
    /// when it changed, when someone asked for the look (`asked`), or after `STATUS_REWRITE_*`.
    async fn write_status(&self, slot: &Slot, wait: Duration, asked: bool) {
        let status = {
            let mut status = slot.status.lock().unwrap();
            status.next_check_at = Some(rfc3339(unix_now() + wait.as_secs()));
            status.every_s = Some(wait.as_secs());
            if let Some((from, to)) = *slot.validity.lock().unwrap() {
                status.not_before = Some(rfc3339(from));
                status.not_after = Some(rfc3339(to));
            }
            status.clone()
        };
        let timeless = CertStatus { checked_at: String::new(), next_check_at: None, ..status.clone() };
        // Renewing has just started failing (as this process knows: a restart may say it again).
        let was = slot.written.lock().unwrap().as_ref().map(|(_, last)| last.state);
        if status.state == CertState::RenewalFailed && was != Some(CertState::RenewalFailed) {
            crate::events::send(
                crate::events::CERTIFICATE_RENEW_FAILED,
                serde_json::json!({"domain": {"name": slot.name}, "error": status.error, "method": status.method, "expires": status.not_after}),
            );
        }
        let rewrite_after = if status.state == CertState::Issued { STATUS_REWRITE_EVERY } else { STATUS_REWRITE_WAITING };
        if !asked
            && let Some((at, last)) = &*slot.written.lock().unwrap()
            && *last == timeless
            && at.elapsed() < rewrite_after
        {
            return;
        }
        match serde_json::to_vec_pretty(&status) {
            Ok(bytes) => match slot.store.put(STATUS_FILE, bytes).await {
                Ok(()) => *slot.written.lock().unwrap() = Some((std::time::Instant::now(), timeless)),
                Err(e) => warn!(domain = %slot.name, error = %e, "writing the certificate status failed"),
            },
            Err(e) => warn!(domain = %slot.name, error = %e, "the certificate status didn't serialize"),
        }
    }

    /// Load the stored certificate if it's newer than the one in use, and issue a new one if it's
    /// due (or `forced`). For a custom domain, the issuer first looks at its DNS (`acme::plan`):
    /// every time, for the status, and to find how to prove it ours when it's due.
    async fn check(&self, slot: &Slot, forced: bool) -> Result<Looked, BoxError> {
        self.look(slot, forced, false).await
    }

    /// `check` for a custom domain; `fresh` asks Cloudflare again for its zone.
    async fn check_domain(&self, slot: &Slot, fresh: bool) -> Result<Looked, BoxError> {
        self.look(slot, false, fresh).await
    }

    async fn look(&self, slot: &Slot, forced: bool, fresh: bool) -> Result<Looked, BoxError> {
        if let Err(e) = self.load_stored(slot).await {
            warn!(store = %slot.store.describe(), error = %e, "loading the stored certificate failed");
        }
        let now = unix_now();
        let current = *slot.validity.lock().unwrap();
        let fine = !forced && current.is_some_and(|(from, to)| !due(from, to, now));
        let Some(acme) = &self.acme else {
            if fine {
                return Ok(Looked::Fine);
            }
            if current.is_none() {
                return Err(format!("no certificate in {} and no ACME configured", slot.store.describe()).into());
            }
            warn!(name = %slot.name, "the certificate is due for renewal, and no ACME is configured");
            return Ok(Looked::Fine);
        };
        let method = if slot.base {
            if fine {
                return Ok(Looked::Fine);
            }
            Method::Cloudflare { zone: acme.base_zone().await? }
        } else {
            let plan = acme::plan(acme, &slot.name, fresh).await;
            let mut status = CertStatus {
                state: if fine { CertState::Issued } else { CertState::Pending },
                method: plan.method.as_ref().map(Method::kind),
                checked_at: rfc3339(now),
                dns: plan.findings.clone(),
                ..Default::default()
            };
            let previous = slot.status.lock().unwrap().clone();
            if fine {
                *slot.status.lock().unwrap() = status;
                return Ok(Looked::Fine);
            }
            let Some(method) = plan.method else {
                let why = plan.reason.unwrap_or_else(|| "no way to prove the domain is ours".to_owned());
                status.state = if current.is_some() { CertState::RenewalFailed } else { CertState::Invalid };
                status.error = Some(why.clone());
                *slot.status.lock().unwrap() = status;
                return Ok(Looked::Unconfigured(why));
            };
            let failures = slot.recent_failures(now);
            if failures >= MAX_FAILURES {
                status.state = if current.is_some() { CertState::RenewalFailed } else { CertState::Pending };
                status.error = Some(format!(
                    "waiting: Let's Encrypt refused it {failures} times in the last hour (it allows 5); last: {}",
                    previous.error.as_deref().unwrap_or("unknown")
                ));
                *slot.status.lock().unwrap() = status;
                return Err("too many failed validations in the last hour".into());
            }
            *slot.status.lock().unwrap() = status;
            method
        };
        let _one_at_a_time = self.issuing.lock().await;
        info!(name = %slot.name, method = method.kind().name(), staging = acme.staging, forced, "issuing a certificate");
        let started = std::time::Instant::now();
        let issued = match acme::issue(acme, &slot.name, &method, &self.wildcard.store).await {
            Ok(issued) => issued,
            Err(e) => {
                if !slot.base {
                    slot.failures.lock().unwrap().push(unix_now());
                    let mut status = slot.status.lock().unwrap();
                    status.state = if current.is_some() { CertState::RenewalFailed } else { CertState::Pending };
                    status.error = Some(format!("Let's Encrypt: {e}"));
                }
                return Err(e);
            }
        };
        let loaded = load(issued.cert_pem.as_bytes(), issued.key_pem.as_bytes())?;
        // Stored before use, so a restart finds it instead of issuing again.
        slot.store.put("key.pem", issued.key_pem.into_bytes()).await?;
        slot.store.put("cert.pem", issued.cert_pem.into_bytes()).await?;
        info!(name = %slot.name, method = method.kind().name(), secs = started.elapsed().as_secs(), "certificate issued and stored");
        self.install(slot, loaded, "issued");
        if !slot.base {
            let mut status = slot.status.lock().unwrap();
            status.state = CertState::Issued;
            status.error = None;
        }
        Ok(Looked::Fine)
    }

    /// Put the stored certificate in use if it expires later than the one in use.
    async fn load_stored(&self, slot: &Slot) -> Result<(), BoxError> {
        let (Some(cert), Some(key)) = (slot.store.get("cert.pem").await?, slot.store.get("key.pem").await?) else {
            return Ok(());
        };
        let loaded = load(&cert, &key)?;
        let in_use = slot.validity.lock().unwrap().map_or(0, |(_, to)| to);
        if loaded.not_after > in_use && loaded.not_after > unix_now() {
            self.install(slot, loaded, "store");
        }
        Ok(())
    }

    fn install(&self, slot: &Slot, loaded: Loaded, source: &str) {
        let days_left = loaded.not_after.saturating_sub(unix_now()) / 86_400;
        info!(
            name = %slot.name,
            source,
            not_before = %rfc3339(loaded.not_before),
            not_after = %rfc3339(loaded.not_after),
            days_left,
            "certificate in use"
        );
        if slot.base {
            *self.resolver.wildcard.write().unwrap() = Some(loaded.key);
        } else {
            self.resolver.domains.write().unwrap().insert(slot.name.clone(), loaded.key);
        }
        *slot.validity.lock().unwrap() = Some((loaded.not_before, loaded.not_after));
        self.expiries.lock().unwrap().insert(slot.name.clone(), loaded.not_after);
        if slot.base {
            self.ready.send_replace(true);
        }
    }
}

/// A certificate's directory: `<name>/`, or `<name>/staging/` for Let's Encrypt's staging server.
fn dir_for(root: &Store, name: &str, staging: bool) -> Store {
    let dir = root.sub(name);
    if staging { dir.sub("staging") } else { dir }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The validity of a DER certificate, as seconds since the epoch: `notBefore` and `notAfter`.
fn validity(der: &[u8]) -> Option<(u64, u64)> {
    let (_, cert, _) = tlv(der).filter(|(tag, _, _)| *tag == 0x30)?;
    let (_, tbs, _) = tlv(cert).filter(|(tag, _, _)| *tag == 0x30)?;
    let mut rest = tbs;
    // version [0] EXPLICIT, optional
    if rest.first() == Some(&0xa0) {
        rest = tlv(rest)?.2;
    }
    // serialNumber, signature, issuer
    for _ in 0..3 {
        rest = tlv(rest)?.2;
    }
    let (_, validity, _) = tlv(rest).filter(|(tag, _, _)| *tag == 0x30)?;
    let (tag, not_before, rest) = tlv(validity)?;
    let not_before = time(tag, not_before)?;
    let (tag, not_after, _) = tlv(rest)?;
    Some((not_before, time(tag, not_after)?))
}

/// One DER element: its tag, its contents, and what follows it.
fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n].iter().fold(0usize, |acc, &b| acc << 8 | b as usize);
        (len, &rest[n..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`), as seconds since the epoch.
fn time(tag: u8, value: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(value).ok()?.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 if text.len() == 12 => {
            let yy: i64 = text[..2].parse().ok()?;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, &text[2..])
        }
        0x18 if text.len() == 14 => (text[..4].parse().ok()?, &text[4..]),
        _ => return None,
    };
    let field = |i: usize| rest.get(i..i + 2)?.parse::<i64>().ok();
    let (month, day, hour, minute, second) = (field(0)?, field(2)?, field(4)?, field(6)?, field(8)?);
    let days = days_from_civil(year, month, day);
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds since the epoch as RFC 3339, to the second.
fn rfc3339(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Valid from 2026-11-04 to the given date.
    fn self_signed(names: &[&str], until: (i32, u8, u8)) -> (String, String, u64, u64) {
        let mut params = rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
        params.not_before = rcgen::date_time_ymd(2026, 11, 4);
        params.not_after = rcgen::date_time_ymd(until.0, until.1, until.2);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let day = |y: i32, m: u8, d: u8| days_from_civil(y.into(), m.into(), d.into()) as u64 * 86_400;
        (cert.pem(), key.serialize_pem(), day(2026, 11, 4), day(until.0, until.1, until.2))
    }

    #[test]
    fn reads_validity() {
        let (cert, key, from, to) = self_signed(&["*.forkrail.test"], (2027, 2, 2));
        let loaded = load(cert.as_bytes(), key.as_bytes()).unwrap();
        assert_eq!((loaded.not_before, loaded.not_after), (from, to));
        assert_eq!(to - from, 90 * 86_400);
        assert_eq!(rfc3339(from), "2026-11-04T00:00:00Z");
        assert_eq!(time(0x17, b"261104000000Z"), Some(from));
        // Past 2049, certificates use GeneralizedTime.
        let y2051 = days_from_civil(2051, 1, 1) as u64 * 86_400;
        assert_eq!((time(0x18, b"20510101000000Z"), rfc3339(y2051).as_str()), (Some(y2051), "2051-01-01T00:00:00Z"));
        // Before the epoch, or malformed.
        assert_eq!(time(0x17, b"510101000000Z"), None);
        assert_eq!(time(0x17, b"2611040000Z"), None);
        assert!(validity(b"\x30\x03\x02\x01\x01").is_none());
    }

    #[test]
    fn renews_at_a_third_of_the_lifetime() {
        let day = 86_400;
        let (from, to) = (1_000 * day, 1_090 * day);
        assert!(!due(from, to, from));
        assert!(!due(from, to, 1_059 * day));
        assert!(due(from, to, 1_060 * day));
        assert!(due(from, to, 1_100 * day));
        // Shorter lifetimes renew sooner: a 6-day certificate after 4 days.
        assert!(!due(0, 6 * day, 3 * day));
        assert!(due(0, 6 * day, 4 * day));
    }

    #[test]
    fn resolver_covers_one_label_only() {
        let resolver = Resolver::new("forkrail.com");
        assert!(resolver.covers("atlas.forkrail.com"));
        assert!(resolver.covers("atlas-a1b2c3d4.forkrail.com"));
        assert!(!resolver.covers("forkrail.com"));
        assert!(!resolver.covers("a.b.forkrail.com"));
        assert!(!resolver.covers("forkrail.com.evil.test"));
    }

    async fn put_cert(dir: &std::path::Path, sub: &str, names: &[&str]) -> u64 {
        let (cert, key, _, to) = self_signed(names, (2036, 11, 1));
        let store = Store::Dir(dir.join(sub));
        store.put("cert.pem", cert.into_bytes()).await.unwrap();
        store.put("key.pem", key.into_bytes()).await.unwrap();
        to
    }

    #[tokio::test]
    async fn status_is_written_on_change_or_when_asked() {
        let dir = std::env::temp_dir().join(format!("forkrail-status-{}", forkrail_common::random_hex(4)));
        let certs = Certs::new("forkrail.test", Store::Dir(dir.clone()), None);
        let slot = certs.domain_slot("api.example.test");
        let file = dir.join("api.example.test").join(STATUS_FILE);
        let read = || serde_json::from_slice::<CertStatus>(&std::fs::read(&file).unwrap()).unwrap();
        *slot.status.lock().unwrap() = CertStatus { state: CertState::Invalid, checked_at: "t1".into(), ..Default::default() };
        certs.write_status(&slot, UNCONFIGURED_EVERY, false).await;
        assert_eq!((read().state, read().every_s), (CertState::Invalid, Some(30)));
        // The same again, a look later: not written.
        std::fs::remove_file(&file).unwrap();
        slot.status.lock().unwrap().checked_at = "t2".into();
        certs.write_status(&slot, UNCONFIGURED_EVERY, false).await;
        assert!(!file.exists());
        // Asked for, or slower now, or something else found: written.
        certs.write_status(&slot, UNCONFIGURED_EVERY, true).await;
        assert_eq!(read().checked_at, "t2");
        std::fs::remove_file(&file).unwrap();
        certs.write_status(&slot, UNCONFIGURED_SLOW, false).await;
        assert_eq!(read().every_s, Some(300));
        std::fs::remove_file(&file).unwrap();
        slot.status.lock().unwrap().dns.points_here = true;
        certs.write_status(&slot, UNCONFIGURED_SLOW, false).await;
        assert!(read().dns.points_here);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn serves_the_wildcard_and_custom_domains_from_a_directory() {
        let dir = std::env::temp_dir().join(format!("forkrail-certs-{}", forkrail_common::random_hex(4)));
        let to = put_cert(&dir, "forkrail.test", &["*.forkrail.test"]).await;
        put_cert(&dir, "api.example.test", &["api.example.test"]).await;
        let certs = Certs::new("forkrail.test", Store::Dir(dir.clone()), None);
        certs.check(&certs.wildcard, false).await.unwrap();
        certs.ready().await;
        assert_eq!(certs.wildcard.validity.lock().unwrap().map(|(_, t)| t), Some(to));
        // Not due, and no ACME: nothing to do. Forced, it says it can't.
        certs.check(&certs.wildcard, true).await.unwrap();

        let resolver = certs.resolver();
        assert!(resolver.pick("pyapp.forkrail.test").is_some());
        assert!(resolver.pick("PyApp.Forkrail.Test.").is_some());
        assert!(resolver.pick("api.example.test").is_none(), "not in the table yet");
        assert!(resolver.pick("a.b.forkrail.test").is_none());
        assert_eq!(resolver.take_unknown(), (2, Some("a.b.forkrail.test".into())));
        assert_eq!(resolver.take_unknown(), (0, None));

        // A custom domain in the table gets its own certificate; one without a stored certificate
        // (and no ACME) keeps failing quietly; leaving the table stops it being served.
        let (tx, rx) = watch::channel(BTreeMap::from([("api.example.test".to_owned(), None), ("missing.example.test".to_owned(), None)]));
        tokio::spawn(certs.clone().maintain_domains(rx));
        // At start-up, stored certificates are in use before HTTPS would start.
        tokio::time::timeout(Duration::from_secs(5), certs.domains_loaded()).await.unwrap();
        assert!(resolver.pick("api.example.test").is_some());
        for _ in 0..100 {
            if resolver.pick("api.example.test").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(resolver.pick("api.example.test").is_some());
        assert!(resolver.pick("missing.example.test").is_none());
        tx.send_replace(BTreeMap::new());
        for _ in 0..100 {
            if resolver.pick("api.example.test").is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(resolver.pick("api.example.test").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
