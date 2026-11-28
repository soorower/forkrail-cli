//! The routing table and a scheduler pool per deployment.
//!
//! The table (`routes.json`, see `forkrail_common::routes`) lives in S3, or in a local file for
//! tests. The router keeps it in memory, so a request costs a hash-map lookup, and reads it again:
//! - at start-up;
//! - when a request names a host the table doesn't have, at most once a second, so a new
//!   deployment answers its first request. With a base domain, only hosts one label under it
//!   count, so a scanner sending random hosts costs nothing;
//! - every `FORKRAIL_ROUTES_POLL_MS`, with a conditional GET that downloads nothing unless the
//!   file changed. That catches promotions, rollbacks and removals, which a miss can't.
//!
//! A deployment's pool is created on its first request. When the deployment leaves the table its
//! pool closes: requests already in it finish, idle workers retire at once, and the pool is dropped
//! once its last worker has gone.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use aws_sdk_s3::error::DisplayErrorContext;
use forkrail_common::routes::{Deployment, DomainRedirect, KeepWarm, ProjectRouting, Routes, one_label_under};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::BoxError;
use crate::pool::{Pool, PoolConfig};
use crate::protection::{Gates, Guard};
use crate::routers::Routers;
use crate::spawner::{Lambdas, Spawner};

/// Refreshes triggered by unknown hosts are at least this far apart.
const MISS_REFRESH_EVERY: Duration = Duration::from_secs(1);
/// While reading the table keeps failing, warn at most this often.
const FAILURE_WARN_EVERY: Duration = Duration::from_secs(60);
/// Environments keep-warm found ready are believed still there for this long after (as a pool
/// believes of its own, `pool.rs`'s `ENVS_WINDOW`): a round comes every two minutes.
const KEPT_FOR: Duration = Duration::from_secs(180);

/// Where the routing table is read from.
pub enum Source {
    S3 { client: aws_sdk_s3::Client, bucket: String, key: String },
    File(PathBuf),
}

impl Source {
    /// `s3://bucket/key`, or a local path.
    pub fn parse(spec: &str, s3: impl FnOnce() -> aws_sdk_s3::Client) -> Result<Self, BoxError> {
        match spec.strip_prefix("s3://") {
            Some(rest) => match rest.split_once('/') {
                Some((bucket, key)) if !bucket.is_empty() && !key.is_empty() => {
                    Ok(Source::S3 { client: s3(), bucket: bucket.to_owned(), key: key.to_owned() })
                }
                _ => Err(format!("FORKRAIL_ROUTES: expected s3://bucket/key, got {spec}").into()),
            },
            None => Ok(Source::File(PathBuf::from(spec))),
        }
    }

    /// The table, unless its version is still `etag`.
    async fn fetch(&self, etag: Option<&str>) -> Result<Fetched, String> {
        match self {
            Source::File(path) => match tokio::fs::read(path).await {
                Ok(bytes) => {
                    let tag = forkrail_common::hex(&Sha256::digest(&bytes));
                    if etag == Some(tag.as_str()) {
                        return Ok(Fetched::NotModified);
                    }
                    Ok(Fetched::Body { bytes, etag: Some(tag) })
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Fetched::Missing),
                Err(e) => Err(format!("{}: {e}", path.display())),
            },
            Source::S3 { client, bucket, key } => {
                crate::spend::ledger().s3(forkrail_common::prices::Meter::S3Get);
                let got = client.get_object().bucket(bucket).key(key).set_if_none_match(etag.map(str::to_owned)).send().await;
                match got {
                    Ok(out) => {
                        let etag = out.e_tag().map(str::to_owned);
                        let bytes = out.body.collect().await.map_err(|e| e.to_string())?.to_vec();
                        Ok(Fetched::Body { bytes, etag })
                    }
                    Err(e) => match e.raw_response().map(|r| r.status().as_u16()) {
                        Some(304) => Ok(Fetched::NotModified),
                        // Without s3:ListBucket, S3 answers 403 for a missing key instead.
                        Some(404) => Ok(Fetched::Missing),
                        _ => Err(DisplayErrorContext(&e).to_string()),
                    },
                }
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Source::S3 { bucket, key, .. } => format!("s3://{bucket}/{key}"),
            Source::File(path) => path.display().to_string(),
        }
    }
}

enum Fetched {
    Body { bytes: Vec<u8>, etag: Option<String> },
    NotModified,
    /// No table yet, as before the first deploy.
    Missing,
}

/// How workers start: Lambda, with clients shared by every pool, or local processes.
#[derive(Clone)]
pub enum Backend {
    Lambda(Lambdas),
    Local(PathBuf),
}

/// What skew protection did with a request that named a deployment (`Registry::pin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    pub outcome: crate::skew::Outcome,
    /// Take `dpl` out of the request's query: it named one of the project's deployments.
    pub strip_query: bool,
}

/// Where a request goes: its deployment (`None` for the default pool) and that deployment's pool.
pub struct Target {
    pub deployment: Option<String>,
    /// `None` for a deployment of static files alone, which has no function.
    pub pool: Option<Arc<Pool>>,
    pub project: String,
    /// Where its workers run; this router's own region for a deployment without a function.
    pub region: String,
    /// `production` or `preview`; empty for the default pool.
    pub environment: String,
    /// The deployment's static files' manifest, if it has one.
    pub static_files: Option<String>,
    /// Its routing rules' program, if it has one.
    pub rules: Option<String>,
    /// Its image settings, if it has them: `/_vercel/image` is then the router's to answer.
    pub images: Option<String>,
    /// The project's own bulk redirects and routing rules, which every deployment of it runs first.
    pub project_routing: ProjectRouting,
    /// The project's protection, when it covers this host.
    pub protection: Option<Guard>,
    /// A Next.js deployment's shape and config (N3, `proxy/nextjs.rs`).
    pub next: Option<forkrail_common::routes::NextRef>,
}

impl Target {
    fn of_pool(deployment: Option<String>, pool: Arc<Pool>, static_files: Option<String>, rules: Option<String>, images: Option<String>) -> Self {
        Self {
            deployment,
            project: pool.project().to_owned(),
            region: pool.region().to_owned(),
            environment: pool.config().environment.clone(),
            pool: Some(pool),
            static_files,
            rules,
            images,
            project_routing: ProjectRouting::default(),
            protection: None,
            next: None,
        }
    }

    /// Whether anything routes its requests before its function.
    pub fn routes(&self) -> bool {
        self.pool.is_none() || self.next.is_some() || self.rules.is_some() || self.static_files.is_some() || self.images.is_some() || self.project_routing.has_rules()
    }
}

pub struct Registry {
    source: Option<Source>,
    backend: Backend,
    /// Settings every deployment's pool starts from; a table entry may override some.
    template: PoolConfig,
    /// For hosts the table doesn't know, when the router runs one (`FORKRAIL_FUNCTION`), on the
    /// tester's port only.
    default_pool: Option<Arc<Pool>>,
    /// Deployments' hosts are one label under it (`FORKRAIL_BASE_DOMAIN`).
    base_domain: Option<String>,
    inner: Mutex<Inner>,
    /// Held while the table is fetched, so there's one refresh at a time.
    refresh: tokio::sync::Mutex<RefreshState>,
    /// The table's custom domains that need a certificate of their own, each with when it was
    /// last asked to be checked again, for the certificates.
    domains: watch::Sender<BTreeMap<String, Option<String>>>,
    /// The firewall configurations the table names, for `firewall.rs`.
    firewalls: watch::Sender<crate::firewall::Refs>,
    /// The table's system bypasses, pause and attack modes (`shield.rs`).
    shield: watch::Sender<Arc<crate::shield::Policy>>,
    /// The table's purges of the response cache, by project (`cache/tags.rs`).
    purges: watch::Sender<Arc<BTreeMap<String, Vec<forkrail_common::purge::Purge>>>>,
}

struct Inner {
    routes: Arc<Routes>,
    /// Deployments that have had a request, with the entry their pool was built from as this
    /// router runs it (`local`).
    pools: HashMap<String, (Deployment, Arc<Pool>)>,
    /// Pools whose deployment left the table, while their workers drain. A worker spawned just
    /// before still finds its pool here when it connects.
    draining: Vec<Weak<Pool>>,
    /// When the latest refresh started.
    refreshed_at: Option<Instant>,
    /// Whether any custom domain redirects, so requests skip looking when none does.
    redirects: bool,
    /// Each protected project's gate.
    gates: Gates,
    /// How many environments keep-warm last found ready for a deployment, and when
    /// (`kept_warm`).
    kept: HashMap<String, (usize, Instant)>,
    /// The install's routers (`routers.rs`): which regions have one, so a pool is made knowing how
    /// its requests reach its function.
    routers: Arc<Routers>,
}

#[derive(Default)]
struct RefreshState {
    etag: Option<String>,
    last_miss: Option<Instant>,
    missing_logged: bool,
    /// When reading last failed with a warning, while it keeps failing.
    failure_warned: Option<Instant>,
}

impl Registry {
    pub fn new(
        source: Option<Source>,
        backend: Backend,
        template: PoolConfig,
        default_pool: Option<Arc<Pool>>,
        base_domain: Option<String>,
    ) -> Arc<Self> {
        let here = match &backend {
            Backend::Lambda(lambdas) => lambdas.here.clone(),
            Backend::Local(_) => String::new(),
        };
        Arc::new(Self {
            source,
            backend,
            template,
            default_pool,
            base_domain,
            inner: Mutex::new(Inner {
                routes: Arc::default(),
                pools: HashMap::new(),
                draining: Vec::new(),
                refreshed_at: None,
                redirects: false,
                gates: Gates::default(),
                kept: HashMap::new(),
                routers: Arc::new(Routers::alone(&here)),
            }),
            refresh: tokio::sync::Mutex::default(),
            domains: watch::channel(BTreeMap::new()).0,
            firewalls: watch::channel(crate::firewall::Refs::default()).0,
            shield: watch::channel(Arc::default()).0,
            purges: watch::channel(Arc::default()).0,
        })
    }

    /// This router's region (empty for local workers).
    pub fn here(&self) -> String {
        match &self.backend {
            Backend::Lambda(lambdas) => lambdas.here.clone(),
            Backend::Local(_) => String::new(),
        }
    }

    /// The firewall configurations in the current table, updated with it.
    pub fn firewall_refs(&self) -> watch::Receiver<crate::firewall::Refs> {
        self.firewalls.subscribe()
    }

    /// The system mitigations' bypasses, pause and attack modes, as the table changes.
    pub fn shield_policy(&self) -> watch::Receiver<Arc<crate::shield::Policy>> {
        self.shield.subscribe()
    }

    /// The table's purges of the response cache by project, as the table changes.
    pub fn table_purges(&self) -> watch::Receiver<Arc<BTreeMap<String, Vec<forkrail_common::purge::Purge>>>> {
        self.purges.subscribe()
    }

    /// The project and environment of the live worker `worker_id`, if `token` is its own: all a
    /// purge from its app may touch (`tunnel.rs`). None for the default pool's workers.
    pub fn worker_scope(&self, worker_id: &str, token: &str) -> Option<(String, String)> {
        let (deployment, _) = worker_id.split_once('.')?;
        let candidates: Vec<Arc<Pool>> = {
            let inner = self.inner.lock().unwrap();
            let active = inner.pools.get(deployment).map(|(_, pool)| pool.clone());
            active.into_iter().chain(inner.draining.iter().filter_map(Weak::upgrade).filter(|p| p.name() == deployment)).collect()
        };
        candidates.into_iter().find_map(|pool| pool.live(worker_id, token).map(|_| (pool.project().to_owned(), pool.config().environment.clone())))
    }

    /// The project `host` belongs to, by a deployment's host or a custom domain, and the
    /// environment of the deployment it points at (empty for a domain that only redirects).
    pub fn project_of(&self, host: &str) -> Option<(String, String)> {
        let inner = self.inner.lock().unwrap();
        if let Some((_, deployment)) = inner.routes.lookup(host) {
            return Some((deployment.project.clone(), deployment.target.clone().unwrap_or_default()));
        }
        inner.routes.domain(host).map(|(_, d)| (d.project.clone(), String::new()))
    }

    /// The custom domains in the current table, updated with it.
    pub fn domains(&self) -> watch::Receiver<BTreeMap<String, Option<String>>> {
        self.domains.subscribe()
    }

    /// Where requests to `host` go instead, if it's a custom domain that redirects (by its name or
    /// a wildcard's). Asked before the host is looked up: a redirecting domain serves nothing.
    pub fn domain_redirect(&self, host: &str) -> Option<DomainRedirect> {
        let inner = self.inner.lock().unwrap();
        if !inner.redirects {
            return None;
        }
        inner.routes.domain(host).and_then(|(_, d)| d.redirect.clone())
    }

    /// Whether `host` is one of the table's custom domains, by its name or a wildcard's.
    pub fn is_custom_domain(&self, host: &str) -> bool {
        self.inner.lock().unwrap().routes.domain(host).is_some()
    }

    /// Where a run of `project`'s schedule `key` goes: its production deployment
    /// (`<project>.<base>`), if that has a schedule of that key.
    pub fn schedule(&self, project: &str, key: &str) -> Option<crate::schedules::Found> {
        let host = format!("{project}.{}", self.base_domain.as_deref()?);
        let inner = self.inner.lock().unwrap();
        let (id, deployment) = inner.routes.lookup(&host)?;
        if deployment.project != project || !deployment.has_function() {
            return None;
        }
        let schedule = deployment.schedules.iter().find(|s| s.key == key)?.clone();
        Some(crate::schedules::Found {
            host,
            deployment: id.to_owned(),
            schedule,
            max_duration: Duration::from_secs(deployment.max_duration_s),
        })
    }

    pub fn default_pool(&self) -> Option<&Arc<Pool>> {
        self.default_pool.as_ref()
    }

    /// The pool for `host` (already normalised): its deployment's, or with `use_default`, the
    /// default pool.
    pub async fn resolve(&self, host: &str, use_default: bool) -> Option<Target> {
        let arrived = Instant::now();
        if let Some(target) = self.lookup(host) {
            return Some(target);
        }
        if self.may_be_new(host) {
            self.refresh_after_miss(arrived).await;
            if let Some(target) = self.lookup(host) {
                return Some(target);
            }
        }
        self.default_pool.clone().filter(|_| use_default).map(|pool| Target::of_pool(None, pool, None, None, None))
    }

    /// Whether an unknown host could be a deployment the table doesn't have yet. Deployments have
    /// hostnames, so a bare IP address (load tests, health checks) never waits for a refresh.
    fn may_be_new(&self, host: &str) -> bool {
        if self.source.is_none() || host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
            return false;
        }
        self.base_domain.as_deref().is_none_or(|base| one_label_under(host, base))
    }

    fn lookup(&self, host: &str) -> Option<Target> {
        let mut inner = self.inner.lock().unwrap();
        let routes = inner.routes.clone();
        let (id, deployment) = routes.lookup(host)?;
        Some(self.target(&mut inner, &routes, host, id, deployment))
    }

    /// Where requests to `host` for deployment `id` go, if the table has it: for a page another
    /// router asks this one to make (`cache/peers.rs`).
    pub fn deployment_target(&self, host: &str, id: &str) -> Option<Target> {
        let mut inner = self.inner.lock().unwrap();
        let routes = inner.routes.clone();
        let (id, deployment) = routes.deployments.get_key_value(id)?;
        Some(self.target(&mut inner, &routes, host, id, deployment))
    }

    /// Every deployment in the table now, with its project.
    /// Let go of every worker of `project`'s deployments now (it's paused: `budgets.rs`).
    pub fn stop_project(&self, project: &str) {
        let pools: Vec<Arc<Pool>> = self.inner.lock().unwrap().pools.values().filter(|(_, p)| p.project() == project).map(|(_, p)| p.clone()).collect();
        for pool in pools {
            pool.stop_all("paused");
        }
    }

    pub fn deployment_projects(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().routes.deployments.iter().map(|(id, d)| (id.clone(), d.project.clone())).collect()
    }

    /// Every deployment in the table now.
    pub fn deployment_ids(&self) -> Vec<String> {
        self.inner.lock().unwrap().routes.deployments.keys().cloned().collect()
    }

    /// Skew protection (`skew.rs`): when `target`'s project has it on and the request names another
    /// of its deployments that may serve it, that deployment's target instead. Also what happened,
    /// for the log, and whether to take `dpl` out of the query. `crawler` says whether the client
    /// comes from a verified crawler's address; asked only of requests that name a deployment.
    pub fn pin(&self, target: Target, host: &str, query: Option<&str>, headers: &hyper::HeaderMap, now: u64, crawler: impl FnOnce() -> bool) -> (Target, Option<Pin>) {
        // Most requests stop here, on what the target already holds, without the lock.
        let Some(current) = target.deployment.clone() else { return (target, None) };
        let Some(setting) = target.project_routing.skew_on().cloned() else { return (target, None) };
        let Some((id, source)) = crate::skew::requested(query, headers) else { return (target, None) };
        let mut inner = self.inner.lock().unwrap();
        let routes = inner.routes.clone();
        // By its id, else by the name its framework gave it (a Build Output's `deploymentId`), the
        // newest of the project's deployments with that name.
        let candidate = routes.deployments.get_key_value(id.as_str()).filter(|(_, d)| d.project == target.project).or_else(|| {
            routes
                .deployments
                .iter()
                .filter(|(_, d)| d.project == target.project && d.skew_id.as_deref() == Some(id.as_str()))
                .max_by_key(|(_, d)| d.created)
        });
        let ask = crate::skew::Ask {
            host,
            headers,
            now,
            production_host: crate::protection::production_name(&routes, self.base_domain.as_deref(), host, &target.project),
            verified_crawler: crawler(),
        };
        let named = candidate.map_or(id.as_str(), |(key, _)| key.as_str());
        let mut outcome = crate::skew::decide(&setting, &ask, &current, named, candidate.map(|(_, d)| d));
        // Off production names, only to a deployment the visitor could reach at its own address.
        if outcome == crate::skew::Outcome::Pinned && !ask.production_host && !inner.gates.pin_reaches(&routes, host, &target.project, named) {
            outcome = crate::skew::Outcome::Protected;
        }
        // A `dpl` naming one of the project's deployments goes, whether or not it pinned: Vercel's
        // apps never see one.
        let pin = Pin { outcome, strip_query: source == crate::skew::Source::Query && candidate.is_some() };
        match candidate {
            Some((id, deployment)) if outcome == crate::skew::Outcome::Pinned => (self.target(&mut inner, &routes, host, id, deployment), Some(pin)),
            _ => (target, Some(pin)),
        }
    }

    /// Where requests to `host` for deployment `id` go, its pool created on first use.
    fn target(&self, inner: &mut Inner, routes: &Routes, host: &str, id: &str, deployment: &Deployment) -> Target {
        let project_routing = routes.projects.get(&deployment.project).cloned().unwrap_or_default();
        let protection = inner.gates.for_host(routes, host, &deployment.project);
        // What a request is judged by (the firewall's and the cache's environment): production at
        // a production name, whatever the deployment was built as (a preview promoted as it is).
        let serving = crate::protection::production_name(routes, self.base_domain.as_deref(), host, &deployment.project).then(|| "production".to_owned());
        if !deployment.has_function() {
            let here = match &self.backend {
                Backend::Lambda(lambdas) => lambdas.here.clone(),
                Backend::Local(_) => String::new(),
            };
            return Target {
                deployment: Some(id.to_owned()),
                pool: None,
                project: deployment.project.clone(),
                region: here,
                environment: serving.unwrap_or_else(|| deployment.target.clone().unwrap_or_default()),
                static_files: deployment.static_files.clone(),
                rules: deployment.rules.clone(),
                images: deployment.images.clone(),
                project_routing,
                protection,
                next: None,
            };
        }
        let pool = match inner.pools.get(id) {
            Some((_, pool)) => pool.clone(),
            None => {
                let local = self.local(&inner.routers, deployment);
                let relay = self.relay_for(&inner.routers, &local);
                let relayed = relay.is_some();
                let pool = self.pool_for(id, &local, relay);
                tokio::spawn(pool.clone().reap());
                let region = local.region.as_deref().unwrap_or_default();
                match &self.backend {
                    Backend::Lambda(lambdas) if region != lambdas.here && relayed => warn!(
                        deployment = id,
                        project = %local.project,
                        region,
                        qualifier = %local.qualifier,
                        "pool created; the deployment has no version in this region: its requests go to that region's router while the link to it is up, else its workers run there and dial back"
                    ),
                    Backend::Lambda(lambdas) if region != lambdas.here => warn!(
                        deployment = id,
                        project = %local.project,
                        region,
                        qualifier = %local.qualifier,
                        "pool created; the deployment has no version in this region, and its region has no router: its workers run there and dial back"
                    ),
                    _ => info!(deployment = id, project = %local.project, region, qualifier = %local.qualifier, "pool created"),
                }
                inner.pools.insert(id.to_owned(), (local, pool.clone()));
                pool
            }
        };
        let mut target = Target::of_pool(Some(id.to_owned()), pool, deployment.static_files.clone(), deployment.rules.clone(), deployment.images.clone());
        target.project_routing = project_routing;
        target.protection = protection;
        target.next = deployment.next.clone();
        if let Some(serving) = serving {
            target.environment = serving;
        }
        target
    }

    /// `deployment` as this router runs it: the region and version it invokes (see
    /// `Deployment::placement`), and no replicas. A change elsewhere, such as a version published
    /// in another region, then leaves this router's pool alone. Its creation time is routing's
    /// business, so filling it in later (as a tool may for old entries) keeps the pool too.
    fn local(&self, routers: &Routers, deployment: &Deployment) -> Deployment {
        let mut local = deployment.clone();
        local.replicas.clear();
        local.created = None;
        if let Backend::Lambda(lambdas) = &self.backend {
            let (region, version) = deployment.placement(&lambdas.here, &lambdas.home, |r| routers.has(r));
            (local.region, local.qualifier) = (Some(region.to_owned()), version.to_owned());
        }
        local
    }

    /// How requests for `local` (as `local` gives it) reach its function from this router: the
    /// link to its region's router when its version is in another region that has one (`relay.rs`),
    /// else none (here, or invoked from afar).
    fn relay_for(&self, routers: &Routers, local: &Deployment) -> Option<Arc<crate::relay::Slot>> {
        self.relay_region(routers, local).map(crate::relay::slot)
    }

    /// The region `relay_for` links to.
    fn relay_region<'a>(&self, routers: &Routers, local: &'a Deployment) -> Option<&'a str> {
        match (&self.backend, local.region.as_deref()) {
            (Backend::Lambda(lambdas), Some(region)) if region != lambdas.here && routers.has(region) => Some(region),
            _ => None,
        }
    }

    /// The routers changed (`routers.rs`): pools whose requests would now go another way (a
    /// router came to or left their region, or another region is now nearer counting the
    /// routers) close, and the next request makes them again.
    pub fn set_routers(&self, routers: Arc<Routers>) {
        let mut inner = self.inner.lock().unwrap();
        if inner.routers == routers {
            return;
        }
        inner.routers = routers;
        let routes = inner.routes.clone();
        let gone = self.close_changed(&mut inner, &routes);
        if !gone.is_empty() {
            info!(closed = ?gone, "the routers changed: these deployments' requests reach their functions another way from now on");
        }
    }

    /// Close the pools of deployments that left `routes`, or whose entry or way there changed.
    fn close_changed(&self, inner: &mut Inner, routes: &Routes) -> Vec<String> {
        let routers = inner.routers.clone();
        let gone: Vec<String> = inner
            .pools
            .iter()
            .filter(|(id, (entry, pool))| match routes.deployments.get(*id).map(|d| self.local(&routers, d)) {
                Some(local) => local != *entry || pool.config().relay.as_ref().map(|s| s.region()) != self.relay_region(&routers, &local),
                None => true,
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &gone {
            if let Some((_, pool)) = inner.pools.remove(id) {
                pool.close();
                inner.draining.push(Arc::downgrade(&pool));
            }
        }
        inner.draining.retain(|pool| pool.strong_count() > 0);
        gone
    }

    /// A pool for `deployment` as `local` gives it, reaching it by `relay` (`relay_for`).
    fn pool_for(&self, id: &str, deployment: &Deployment, relay: Option<Arc<crate::relay::Slot>>) -> Arc<Pool> {
        let mut cfg = self.template.clone();
        cfg.name = id.to_owned();
        if let Some(n) = deployment.max_inflight {
            cfg.max_inflight = n;
        }
        if let Some(pct) = deployment.cpu_target_pct {
            cfg.cpu_target_pct = pct;
        }
        if let Some(n) = deployment.max_workers {
            cfg.max_workers = n as usize;
        }
        if let Some(ms) = deployment.idle_ms {
            cfg.idle = Duration::from_millis(ms);
        } else if cfg.idle_auto {
            cfg.idle = Duration::from_millis(forkrail_common::routes::economic_idle_ms(deployment.memory_mb));
        }
        if let Some(pct) = deployment.headroom_pct {
            cfg.headroom_pct = pct;
        }
        cfg.max_duration = Duration::from_secs(deployment.max_duration_s);
        cfg.project = deployment.project.clone();
        cfg.memory_mb = deployment.memory_mb;
        cfg.region = deployment.region.clone().unwrap_or_default();
        // A version in another region with a router: that router runs it while the link to it is
        // up (`relay.rs`).
        cfg.relay = relay;
        cfg.environment = deployment.target.clone().unwrap_or_default();
        cfg.runtime = deployment.runtime.clone().unwrap_or_default();
        cfg.entry = deployment.entry.clone().unwrap_or_default();
        cfg.egress = deployment.egress.clone();
        Pool::new(cfg, self.spawner_for(deployment))
    }

    /// What starts invocations of `deployment` as `local` gives it.
    pub fn spawner_for(&self, deployment: &Deployment) -> Spawner {
        match &self.backend {
            Backend::Lambda(lambdas) => Spawner::Lambda {
                client: lambdas.client(deployment.region.as_deref().unwrap_or(&lambdas.here)),
                function: deployment.function.clone(),
                qualifier: Some(deployment.qualifier.clone()),
            },
            Backend::Local(bin) => Spawner::Local {
                bin: bin.clone(),
                env: deployment.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                isolated: true,
            },
        }
    }

    /// The deployments this router keeps environments alive for (`keepwarm.rs`): every project's
    /// production deployment (what `<project>.<base>` and its production domains point at) that
    /// has a function, with what its project asks for (`default` when it says nothing; none for
    /// 0). Each as this router runs it (`local`), and only where the keeping is this router's: its
    /// own region's version, or a version in a region with no router of its own (`routers.rs`),
    /// which whoever invokes it must keep. With what its pool's invocations did since the last call
    /// (`Pool::activity`).
    pub fn keep_warm(&self, default: KeepWarm) -> Vec<crate::keepwarm::Target> {
        let inner = self.inner.lock().unwrap();
        let routes = &inner.routes;
        let mut ids = std::collections::BTreeSet::new();
        if let Some(base) = self.base_domain.as_deref() {
            let projects: std::collections::BTreeSet<&str> = routes.deployments.values().map(|d| d.project.as_str()).collect();
            ids.extend(projects.into_iter().filter_map(|project| {
                let id = routes.hosts.get(&format!("{project}.{base}"))?;
                (routes.deployments.get(id)?.project == project).then_some(id)
            }));
        }
        ids.extend(routes.domains.iter().filter(|(_, d)| d.production()).filter_map(|(domain, _)| routes.hosts.get(domain)));
        let here = match &self.backend {
            Backend::Lambda(lambdas) => lambdas.here.as_str(),
            Backend::Local(_) => "",
        };
        ids.into_iter()
            .filter_map(|id| {
                let deployment = routes.deployments.get(id).filter(|d| d.has_function())?;
                let keep = routes.projects.get(&deployment.project).and_then(|p| p.keep_warm).unwrap_or(default);
                let entry = self.local(&inner.routers, deployment);
                let region = entry.region.clone().unwrap_or_default();
                let ours = region == here || !inner.routers.has(&region);
                // A paused project spends nothing, keep-warm's invocations included.
                if !keep.on() || !ours || crate::budgets::paused(&deployment.project).is_some() {
                    return None;
                }
                let activity = inner.pools.get(id).map(|(_, pool)| pool.activity()).unwrap_or_default();
                Some(crate::keepwarm::Target { deployment: id.clone(), region, keep, activity, entry })
            })
            .collect()
    }

    /// A keep-warm round found `environments` ready for `deployment` just now.
    pub fn kept_warm(&self, deployment: &str, environments: usize) {
        let mut inner = self.inner.lock().unwrap();
        inner.kept.retain(|_, (_, at)| at.elapsed() < KEPT_FOR);
        inner.kept.insert(deployment.to_owned(), (environments, Instant::now()));
    }

    /// For starting a worker at a visitor's TLS handshake (`early.rs`): the pool of the deployment
    /// `host` (normalised) points at, made if it's the first time, its project, and how many
    /// environments keep-warm found ready for it lately. None for a host the table doesn't have
    /// (never waiting for the table to be read again), a domain that only redirects, and a
    /// deployment of static files alone, whose answers need no worker.
    pub fn at_handshake(&self, host: &str) -> Option<(Arc<Pool>, String, usize)> {
        if self.domain_redirect(host).is_some() {
            return None;
        }
        let target = self.lookup(host)?;
        if crate::budgets::paused(&target.project).is_some() {
            return None;
        }
        // A protected host's deployment runs nothing for a visitor who hasn't got in, and a
        // handshake hasn't shown anything yet.
        if target.protection.is_some() {
            return None;
        }
        let pool = target.pool?;
        let kept = self.inner.lock().unwrap().kept.get(pool.name()).filter(|(_, at)| at.elapsed() < KEPT_FOR).map_or(0, |(n, _)| *n);
        Some((pool, target.project, kept))
    }

    /// The pool that spawned `worker_id`, if its token checks out, and how long ago it spawned.
    pub fn claim(&self, worker_id: &str, token: &str) -> Option<(Arc<Pool>, Duration)> {
        let Some((deployment, _)) = worker_id.split_once('.') else {
            let pool = self.default_pool.clone()?;
            return pool.claim(worker_id, token).map(|since| (pool, since));
        };
        let candidates: Vec<Arc<Pool>> = {
            let inner = self.inner.lock().unwrap();
            let active = inner.pools.get(deployment).map(|(_, pool)| pool.clone());
            active.into_iter().chain(inner.draining.iter().filter_map(Weak::upgrade).filter(|p| p.name() == deployment)).collect()
        };
        candidates.into_iter().find_map(|pool| pool.claim(worker_id, token).map(|since| (pool, since)))
    }

    /// For an egress connection from worker `worker_id`: its pool must allow `target`, and the
    /// worker must be connected with `token`. Returns its pool, and the receiver that ends when
    /// the worker goes.
    pub fn egress(&self, worker_id: &str, token: &str, target: &str) -> Option<(Arc<Pool>, tokio::sync::watch::Receiver<()>)> {
        let (deployment, _) = worker_id.split_once('.')?;
        let candidates: Vec<Arc<Pool>> = {
            let inner = self.inner.lock().unwrap();
            let active = inner.pools.get(deployment).map(|(_, pool)| pool.clone());
            active.into_iter().chain(inner.draining.iter().filter_map(Weak::upgrade).filter(|p| p.name() == deployment)).collect()
        };
        // The worker's own pool (it may be draining while a newer one is active), if it allows the target.
        candidates.into_iter().find_map(|pool| if pool.config().egress.iter().any(|t| t == target) { pool.live(worker_id, token).map(|gone| (pool, gone)) } else { None })
    }

    /// Read the table again unless a refresh began after the miss at `arrived`, waiting for the
    /// once-a-second slot if one was used just now.
    async fn refresh_after_miss(&self, arrived: Instant) {
        let mut state = self.refresh.lock().await;
        if self.inner.lock().unwrap().refreshed_at.is_some_and(|at| at >= arrived) {
            return;
        }
        if let Some(last) = state.last_miss {
            tokio::time::sleep_until((last + MISS_REFRESH_EVERY).into()).await;
        }
        state.last_miss = Some(Instant::now());
        self.refresh_locked(&mut state).await;
    }

    /// Read the table again; with `If-None-Match`, so an unchanged file costs nothing.
    pub async fn refresh(&self) {
        let mut state = self.refresh.lock().await;
        self.refresh_locked(&mut state).await;
    }

    async fn refresh_locked(&self, state: &mut RefreshState) {
        let Some(source) = &self.source else { return };
        self.inner.lock().unwrap().refreshed_at = Some(Instant::now());
        let fetched = match source.fetch(state.etag.as_deref()).await {
            Ok(fetched) => {
                if state.failure_warned.take().is_some() {
                    info!(source = %source.describe(), "reading routes works again");
                }
                fetched
            }
            Err(e) => {
                if state.failure_warned.is_none_or(|at| at.elapsed() >= FAILURE_WARN_EVERY) {
                    warn!(source = %source.describe(), error = %e, "reading routes failed; keeping the current table");
                    state.failure_warned = Some(Instant::now());
                }
                return;
            }
        };
        let (routes, etag) = match fetched {
            Fetched::NotModified => return,
            Fetched::Missing => {
                if !state.missing_logged {
                    warn!(source = %source.describe(), "no routes file yet; serving no deployments");
                    state.missing_logged = true;
                }
                (Routes::default(), None)
            }
            Fetched::Body { bytes, etag } => {
                state.missing_logged = false;
                match serde_json::from_slice::<Routes>(&bytes).map_err(|e| e.to_string()).and_then(|r| r.validate().map(|()| r)) {
                    Ok(routes) => (routes, etag),
                    Err(e) => {
                        warn!(source = %source.describe(), error = %e, "invalid routes file; keeping the current table");
                        // Remember its version, so it isn't fetched again until it changes.
                        state.etag = etag;
                        return;
                    }
                }
            }
        };
        state.etag = etag;
        self.apply(routes);
    }

    /// Swap in a new table. Pools of deployments that left it, or whose entry changed, close.
    pub(crate) fn apply(&self, routes: Routes) {
        let mut inner = self.inner.lock().unwrap();
        let gone = self.close_changed(&mut inner, &routes);
        let changed = *inner.routes != routes;
        if changed {
            crate::loops::set_own_hosts(crate::loops::own_hosts_of(&routes, self.base_domain.as_deref()));
            crate::budgets::budgets().set_table(&routes);
        }
        if changed || !gone.is_empty() {
            info!(
                deployments = routes.deployments.len(),
                hosts = routes.hosts.len(),
                closed = ?gone,
                draining = inner.draining.len(),
                "routes loaded"
            );
        }
        // Domains one label under the base (`app.forkrail.com`, kept for the site) have the wildcard.
        let wildcard = |d: &str| self.base_domain.as_deref().is_some_and(|base| forkrail_common::routes::one_label_under(d, base));
        let domains: BTreeMap<String, Option<String>> =
            routes.domains.iter().filter(|(d, _)| !wildcard(d)).map(|(d, entry)| (d.clone(), entry.checked_at.clone())).collect();
        self.domains.send_if_modified(|current| {
            let changed = *current != domains;
            *current = domains;
            changed
        });
        let firewalls = crate::firewall::Refs {
            install: routes.firewall.clone(),
            projects: routes.projects.iter().filter_map(|(p, r)| Some((p.clone(), r.firewall.clone()?))).collect(),
        };
        self.firewalls.send_if_modified(|current| {
            let changed = *current != firewalls;
            *current = firewalls;
            changed
        });
        self.shield.send_replace(Arc::new(crate::shield::Policy::from_routes(&routes)));
        let purges: BTreeMap<String, Vec<forkrail_common::purge::Purge>> =
            routes.projects.iter().filter(|(_, r)| !r.purges.is_empty()).map(|(p, r)| (p.clone(), r.purges.clone())).collect();
        self.purges.send_if_modified(|current| {
            let changed = **current != purges;
            if changed {
                *current = Arc::new(purges);
            }
            changed
        });
        inner.redirects = routes.domains.values().any(|d| d.redirect.is_some());
        inner.gates = Gates::build(&routes, self.base_domain.as_deref());
        inner.routes = Arc::new(routes);
    }

    /// Re-read the table every `every`, until the router exits.
    pub async fn poll(self: Arc<Self>, every: Duration) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            self.refresh().await;
        }
    }

    #[cfg(test)]
    fn pool_count(&self) -> (usize, usize) {
        let inner = self.inner.lock().unwrap();
        (inner.pools.len(), inner.draining.iter().filter(|p| p.strong_count() > 0).count())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use forkrail_common::routes::{DEFAULT_MEMORY_MB, normalize_host};

    use super::*;
    use crate::pool::WorkerBudget;

    fn template() -> PoolConfig {
        PoolConfig {
            name: String::new(),
            max_inflight: 10,
            idle: Duration::from_millis(100),
            idle_auto: false,
            max_age: Duration::from_secs(780),
            headroom_pct: 10,
            tunnel_addr: "127.0.0.1:1".into(),
            cert_sha256: String::new(),
            acquire_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            cpu_target_pct: 0,
            profile_pct: 0,
            max_workers: 20,
            budget: WorkerBudget::new(100),
            max_duration: Duration::from_secs(300),
            project: String::new(),
            region: String::new(),
            memory_mb: 0,
            relay: None,
            environment: String::new(),
            runtime: String::new(),
            entry: String::new(),
            egress: Vec::new(),
        }
    }

    fn deployment(project: &str, max_duration_s: u64) -> Deployment {
        Deployment {
            project: project.into(),
            function: "local".into(),
            qualifier: "1".into(),
            region: None,
            replicas: BTreeMap::new(),
            max_inflight: Some(4),
            cpu_target_pct: None,
            max_workers: Some(2),
            idle_ms: None,
            headroom_pct: None,
            max_duration_s,
            memory_mb: DEFAULT_MEMORY_MB,
            target: None,
            runtime: None,
            entry: None,
            env: BTreeMap::new(),
            static_files: None,
            egress: Vec::new(),
            rules: None,
            images: None,
            schedules: Vec::new(),
            created: None,
            skew_id: None,
            next: None,
        }
    }

    fn write(path: &std::path::Path, routes: &Routes) {
        std::fs::write(path, serde_json::to_vec(routes).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn reload_adds_removes_and_drops_pools() {
        let dir = std::env::temp_dir().join(format!("forkrail-registry-{}", forkrail_common::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("routes.json");
        let registry = Registry::new(
            Some(Source::File(path.clone())),
            Backend::Local(PathBuf::from("/nonexistent/forkrail-runtime")),
            template(),
            None,
            Some("forkrail.test".into()),
        );

        // No file yet: nothing is served, and that isn't an error.
        registry.refresh().await;
        assert!(registry.resolve("a.forkrail.test", true).await.is_none());

        let mut routes = Routes::default();
        routes.deployments.insert("aaaa1111".into(), deployment("a", 2));
        routes.deployments.insert("bbbb2222".into(), deployment("b", 300));
        routes.hosts.insert("a.forkrail.test".into(), "aaaa1111".into());
        routes.hosts.insert("b-bbbb2222.forkrail.test".into(), "bbbb2222".into());
        // A deployment of static files alone has no function, and so no pool.
        let mut site = deployment("site", 300);
        (site.function, site.qualifier, site.static_files, site.rules) = (String::new(), String::new(), Some("ab".repeat(32)), Some("cd".repeat(32)));
        routes.deployments.insert("cccc3333".into(), site);
        routes.hosts.insert("site.forkrail.test".into(), "cccc3333".into());
        write(&path, &routes);

        // A host that isn't in the table yet triggers a refresh, so it answers at once.
        let a = registry.resolve(&normalize_host("A.Forkrail.Test:8080"), false).await.expect("refreshed on miss");
        assert_eq!(a.deployment.as_deref(), Some("aaaa1111"));
        assert_eq!(a.pool.as_ref().unwrap().max_duration(), Duration::from_secs(2));
        let b = registry.resolve("b-bbbb2222.forkrail.test", false).await.unwrap();
        assert_eq!(b.pool.as_ref().unwrap().name(), "bbbb2222");
        // The same deployment gets the same pool.
        assert!(Arc::ptr_eq(a.pool.as_ref().unwrap(), registry.resolve("a.forkrail.test", false).await.unwrap().pool.as_ref().unwrap()));
        assert_eq!(registry.pool_count(), (2, 0));
        let site = registry.resolve("site.forkrail.test", false).await.unwrap();
        assert!(site.pool.is_none());
        assert_eq!((site.project.as_str(), site.rules.as_deref()), ("site", Some("cd".repeat(32).as_str())));
        assert_eq!(registry.pool_count(), (2, 0));

        // Repoint a's host at b, and remove a.
        routes.deployments.remove("aaaa1111");
        routes.hosts.insert("a.forkrail.test".into(), "bbbb2222".into());
        write(&path, &routes);
        registry.refresh().await;
        let repointed = registry.resolve("a.forkrail.test", false).await.unwrap();
        assert!(Arc::ptr_eq(repointed.pool.as_ref().unwrap(), b.pool.as_ref().unwrap()));
        assert_eq!(registry.pool_count(), (1, 1));

        // The closed pool has no workers, so its reaper stops and the pool is dropped.
        let weak = Arc::downgrade(a.pool.as_ref().unwrap());
        drop(a);
        for _ in 0..40 {
            if weak.strong_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(weak.strong_count(), 0, "closed pool should be dropped once drained");
        assert_eq!(registry.pool_count(), (1, 0));

        // An invalid file keeps the current table.
        std::fs::write(&path, b"{\"hosts\": {\"x.test\": \"nope\"}}").unwrap();
        registry.refresh().await;
        assert!(registry.resolve("b-bbbb2222.forkrail.test", false).await.is_some());

        // Only hosts one label under the base domain may be new deployments.
        assert!(registry.may_be_new("c.forkrail.test"));
        assert!(!registry.may_be_new("c.d.forkrail.test"));
        assert!(!registry.may_be_new("random.example.com"));
        assert!(!registry.may_be_new("10.0.0.1"));

        // Worker ids name their pool; unknown deployments and bad tokens are refused.
        assert!(registry.claim("bbbb2222.00ff", "token").is_none());
        assert!(registry.claim("zzzz.00ff", "token").is_none());
        assert!(registry.claim("00ff", "token").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn skew_protection_pins_to_the_projects_own_deployments() {
        use forkrail_common::routes::SkewProtection;
        use hyper::header::HeaderValue;
        use crate::skew::Outcome;

        let now = 1_794_800_000;
        let registry = Registry::new(None, Backend::Local(PathBuf::from("/nonexistent/forkrail-runtime")), template(), None, Some("forkrail.test".into()));
        let mut routes = Routes::default();
        let at = |project: &str, target: &str, age: u64| {
            let mut d = deployment(project, 300);
            (d.target, d.created) = (Some(target.into()), Some(now - age));
            d
        };
        routes.deployments.insert("new1".into(), at("shop", "production", 60));
        routes.deployments.insert("old1".into(), at("shop", "production", 3_600));
        routes.deployments.insert("pre1".into(), at("shop", "preview", 600));
        routes.deployments.insert("oth1".into(), at("other", "production", 600));
        routes.hosts.insert("shop.forkrail.test".into(), "new1".into());
        routes.hosts.insert("shop-git-main.forkrail.test".into(), "new1".into());
        routes.hosts.insert("other.forkrail.test".into(), "oth1".into());
        routes.projects.entry("shop".into()).or_default().skew = Some(SkewProtection { enabled: true, ..Default::default() });
        registry.apply(routes.clone());

        let ask = |host: &str, query: Option<&str>, headers: &hyper::HeaderMap| {
            let target = registry.lookup(host).unwrap();
            let (target, pin) = registry.pin(target, host, query, headers, now, || false);
            (target.deployment.unwrap(), pin.map(|p| (p.outcome, p.strip_query)))
        };
        let none = hyper::HeaderMap::new();
        assert_eq!(ask("shop.forkrail.test", Some("dpl=old1"), &none), ("old1".into(), Some((Outcome::Pinned, true))));
        let mut header = hyper::HeaderMap::new();
        header.insert("x-deployment-id", HeaderValue::from_static("old1"));
        assert_eq!(ask("shop.forkrail.test", None, &header), ("old1".into(), Some((Outcome::Pinned, false))));
        // Another project's deployment, or an id no deployment has: the current one, `dpl` kept.
        assert_eq!(ask("shop.forkrail.test", Some("dpl=oth1"), &none), ("new1".into(), Some((Outcome::Unknown, false))));
        assert_eq!(ask("shop.forkrail.test", Some("dpl=nope"), &none), ("new1".into(), Some((Outcome::Unknown, false))));
        // The current one's id goes from the query too.
        assert_eq!(ask("shop.forkrail.test", Some("dpl=new1"), &none), ("new1".into(), Some((Outcome::Current, true))));
        // A preview: not on the production name, but on the branch's host.
        assert_eq!(ask("shop.forkrail.test", Some("dpl=pre1"), &none), ("new1".into(), Some((Outcome::Environment, true))));
        assert_eq!(ask("shop-git-main.forkrail.test", Some("dpl=pre1"), &none), ("pre1".into(), Some((Outcome::Pinned, true))));
        // A framework's own name for a deployment (a Build Output's `deploymentId`): that one.
        let mut named = at("shop", "production", 1_200);
        named.skew_id = Some("build-2026-11-16".into());
        routes.deployments.insert("nam1".into(), named);
        registry.apply(routes.clone());
        assert_eq!(ask("shop.forkrail.test", Some("dpl=build-2026-11-16"), &none), ("nam1".into(), Some((Outcome::Pinned, true))));
        let mut by_header = hyper::HeaderMap::new();
        by_header.insert("x-deployment-id", HeaderValue::from_static("build-2026-11-16"));
        assert_eq!(ask("shop.forkrail.test", None, &by_header), ("nam1".into(), Some((Outcome::Pinned, false))));
        // Not another project's.
        let mut theirs = at("other", "production", 60);
        theirs.skew_id = Some("theirs".into());
        routes.deployments.insert("oth2".into(), theirs);
        registry.apply(routes.clone());
        assert_eq!(ask("shop.forkrail.test", Some("dpl=theirs"), &none), ("new1".into(), Some((Outcome::Unknown, false))));
        // Protected previews: a branch's host opens with a sign-in or share link for that host
        // alone, so it pins nothing; production names pin as before.
        routes.protection = Some(forkrail_common::protection::ProtectionConfig { keys: Vec::new(), login: None, default: forkrail_common::protection::ProtectionScope::Standard });
        registry.apply(routes.clone());
        assert_eq!(ask("shop-git-main.forkrail.test", Some("dpl=pre1"), &none), ("new1".into(), Some((Outcome::Protected, true))));
        assert_eq!(ask("shop.forkrail.test", Some("dpl=old1"), &none), ("old1".into(), Some((Outcome::Pinned, true))));
        routes.protection = None;
        registry.apply(routes.clone());
        // A project without skew protection: nothing happens, `dpl` stays.
        assert_eq!(ask("other.forkrail.test", Some("dpl=oth1"), &none), ("oth1".into(), None));
        // Off but set: likewise.
        routes.projects.get_mut("shop").unwrap().skew.as_mut().unwrap().enabled = false;
        registry.apply(routes);
        assert_eq!(ask("shop.forkrail.test", Some("dpl=old1"), &none), ("new1".into(), None));
    }

    #[test]
    fn a_creation_time_filled_in_later_keeps_the_pool() {
        let registry = Registry::new(None, Backend::Local(PathBuf::new()), template(), None, None);
        let d = deployment("shop", 300);
        let mut dated = d.clone();
        dated.created = Some(1_794_800_000);
        assert_eq!(local_of(&registry, &d), local_of(&registry, &dated));
    }

    #[tokio::test]
    async fn unknown_hosts_go_to_the_default_pool_on_the_testers_port() {
        let default = Pool::new(template(), Spawner::Local { bin: PathBuf::new(), env: Vec::new(), isolated: false });
        let registry = Registry::new(None, Backend::Local(PathBuf::new()), template(), Some(default.clone()), None);
        let target = registry.resolve("127.0.0.1", true).await.unwrap();
        assert!(target.deployment.is_none());
        assert!(Arc::ptr_eq(target.pool.as_ref().unwrap(), &default));
        // Not on the public ports.
        assert!(registry.resolve("127.0.0.1", false).await.is_none());
    }

    #[tokio::test]
    async fn runs_the_version_in_its_own_region() {
        use aws_config::{BehaviorVersion, Region};
        let config = aws_config::SdkConfig::builder()
            .region(Region::new("us-west-1"))
            .behavior_version(BehaviorVersion::latest())
            .build();
        let lambdas = Lambdas::new(config, Some("us-east-1".into()));
        let registry = Registry::new(None, Backend::Lambda(lambdas), template(), None, None);
        let mut d = deployment("atlas", 300);
        d.qualifier = "13".into();
        // No version here: the home region's, from afar.
        let local = local_of(&registry, &d);
        assert_eq!((local.region.as_deref(), local.qualifier.as_str()), (Some("us-east-1"), "13"));
        // Once it's published here, the version here.
        d.region = Some("us-east-1".into());
        d.replicas.insert("us-west-1".into(), "2".into());
        let local = local_of(&registry, &d);
        assert_eq!((local.region.as_deref(), local.qualifier.as_str()), (Some("us-west-1"), "2"));
        assert!(local.replicas.is_empty());
        // A version in a third region changes nothing here, so the pool stays.
        let mut more = d.clone();
        more.replicas.insert("eu-central-1".into(), "7".into());
        assert_eq!(local_of(&registry, &more), local);
    }

    /// `local`, with the routers the registry knows now.
    fn local_of(registry: &Registry, d: &Deployment) -> Deployment {
        let routers = registry.inner.lock().unwrap().routers.clone();
        registry.local(&routers, d)
    }

    /// How a router reaches a function in another region is settled with the deployment's pool,
    /// from the routers it knows: that region's router while it has one, else from afar. A router
    /// that comes or goes makes the pools it changes again; the rest stay.
    #[tokio::test]
    async fn reaches_another_regions_function_through_its_router_if_it_has_one() {
        use aws_config::{BehaviorVersion, Region};
        let config = aws_config::SdkConfig::builder().region(Region::new("us-west-1")).behavior_version(BehaviorVersion::latest()).build();
        let registry = Registry::new(None, Backend::Lambda(Lambdas::new(config, Some("us-east-1".into()))), template(), None, None);
        let mut routes = Routes::default();
        // `api` runs in us-east-1 alone; `web` here too.
        let mut api = deployment("api", 300);
        api.region = Some("us-east-1".into());
        let mut web = api.clone();
        web.project = "web".into();
        web.replicas.insert("us-west-1".into(), "3".into());
        routes.deployments.insert("a1".into(), api);
        routes.deployments.insert("w1".into(), web);
        routes.hosts.insert("api.test".into(), "a1".into());
        routes.hosts.insert("web.test".into(), "w1".into());
        registry.apply(routes);
        let routers = |regions: &[&str]| Arc::new(Routers::merge("us-west-1", None, &regions.iter().map(|r| (r.to_string(), format!("{r}.test:7445"))).collect(), &[]));
        let via = |host: &str, id: &str| registry.deployment_target(host, id).unwrap().pool.unwrap().config().relay.as_ref().map(|s| s.region().to_owned());

        // Alone in us-west-1: `api` is invoked in us-east-1 from here.
        let first = registry.deployment_target("api.test", "a1").unwrap().pool.unwrap();
        assert_eq!(first.config().region, "us-east-1");
        assert_eq!(via("api.test", "a1"), None);
        assert_eq!(via("web.test", "w1"), None);
        // A router in us-east-1 is heard of: `api` goes through it from its next pool; `web`'s stays.
        let web_pool = registry.deployment_target("web.test", "w1").unwrap().pool.unwrap();
        registry.set_routers(routers(&["us-east-1", "us-west-1"]));
        assert_eq!(registry.pool_count(), (1, 1), "api's pool closed, web's kept");
        assert_eq!(via("api.test", "a1").as_deref(), Some("us-east-1"));
        assert!(Arc::ptr_eq(&registry.deployment_target("web.test", "w1").unwrap().pool.unwrap(), &web_pool));
        // The same routers again: nothing changes.
        let second = registry.deployment_target("api.test", "a1").unwrap().pool.unwrap();
        registry.set_routers(routers(&["us-east-1", "us-west-1"]));
        assert!(Arc::ptr_eq(&registry.deployment_target("api.test", "a1").unwrap().pool.unwrap(), &second));
        // That router is gone: from afar again.
        registry.set_routers(routers(&["us-west-1"]));
        assert_eq!(via("api.test", "a1"), None);
        assert!(!Arc::ptr_eq(&registry.deployment_target("api.test", "a1").unwrap().pool.unwrap(), &second));
    }

    /// Which deployments a router keeps environments alive for, and where.
    #[tokio::test]
    async fn keeps_production_warm_where_it_is_this_routers_to_keep() {
        use aws_config::{BehaviorVersion, Region};
        use forkrail_common::routes::{CustomDomain, ProjectRouting};
        let lambdas = |here: &str| {
            let config = aws_config::SdkConfig::builder().region(Region::new(here.to_owned())).behavior_version(BehaviorVersion::latest()).build();
            Backend::Lambda(Lambdas::new(config, Some("us-east-1".into())))
        };
        let mut routes = Routes::default();
        // `api`: production `a2` (published in both regions), an older production `a1` and a preview.
        let mut a2 = deployment("api", 300);
        (a2.region, a2.qualifier, a2.target) = (Some("us-east-1".into()), "12".into(), Some("production".into()));
        a2.replicas.insert("us-west-1".into(), "7".into());
        let mut a1 = a2.clone();
        a1.qualifier = "11".into();
        a1.replicas.clear();
        let mut preview = a1.clone();
        (preview.qualifier, preview.target) = ("13".into(), Some("preview".into()));
        // `site`: production only in us-east-1, reached by a custom domain too; keeps 3.
        let mut s1 = deployment("site", 300);
        (s1.region, s1.target) = (Some("us-east-1".into()), Some("production".into()));
        // `docs`: static files alone, nothing to keep. `off`: its project keeps none.
        let mut docs = deployment("docs", 300);
        (docs.function, docs.qualifier, docs.static_files) = (String::new(), String::new(), Some("a".repeat(64)));
        let off = deployment("off", 300);
        for (id, d) in [("a1", a1), ("a2", a2), ("a3", preview), ("s1", s1), ("d1", docs), ("o1", off)] {
            routes.deployments.insert(id.into(), d);
        }
        for (host, id) in [("api.example.com", "a2"), ("api-a1.example.com", "a1"), ("api-a3.example.com", "a3"), ("site.example.com", "s1"), ("www.site.io", "s1"), ("docs.example.com", "d1"), ("off.example.com", "o1")] {
            routes.hosts.insert(host.into(), id.into());
        }
        routes.domains.insert("www.site.io".into(), CustomDomain::new("site"));
        routes.projects.insert("site".into(), ProjectRouting { keep_warm: Some(KeepWarm::Count(3)), ..Default::default() });
        routes.projects.insert("off".into(), ProjectRouting { keep_warm: Some(KeepWarm::Count(0)), ..Default::default() });
        routes.validate().unwrap();
        let both = ["us-east-1".to_owned(), "us-west-1".to_owned()];
        let kept = |registry: &Registry, default: u32, regions: &[String]| -> Vec<(String, String, String, KeepWarm)> {
            let here = registry.here();
            let pinned = regions.iter().map(|r| (r.clone(), format!("{r}.test:7445"))).collect();
            registry.set_routers(Arc::new(Routers::merge(&here, None, &pinned, &[])));
            registry.keep_warm(KeepWarm::Count(default)).into_iter().map(|t| (t.deployment, t.region, t.entry.qualifier, t.keep)).collect()
        };
        let row = |d: &str, r: &str, q: &str, n: u32| (d.to_owned(), r.to_owned(), q.to_owned(), KeepWarm::Count(n));

        // The router at home keeps both productions' versions there: never an old production
        // deployment, a preview, a deployment without a function or a project that asks for none.
        let east = Registry::new(None, lambdas("us-east-1"), template(), None, Some("example.com".into()));
        east.apply(routes.clone());
        assert_eq!(kept(&east, 1, &both), [row("a2", "us-east-1", "12", 1), row("s1", "us-east-1", "1", 3)]);
        // The other region's keeps its own version of `api`; `site` runs at home, whose router keeps it.
        let west = Registry::new(None, lambdas("us-west-1"), template(), None, Some("example.com".into()));
        west.apply(routes.clone());
        assert_eq!(kept(&west, 1, &both), [row("a2", "us-west-1", "7", 1)]);
        // Unless home has no router of its own: then whoever invokes there keeps it.
        assert_eq!(kept(&west, 1, &["us-west-1".to_owned()]), [row("a2", "us-west-1", "7", 1), row("s1", "us-east-1", "1", 3)]);
        // The router's default of 0 keeps only projects that ask.
        assert_eq!(kept(&east, 0, &both), [row("s1", "us-east-1", "1", 3)]);
        // Without a base domain production is known by its custom domains alone.
        let bare = Registry::new(None, lambdas("us-east-1"), template(), None, None);
        bare.apply(routes.clone());
        assert_eq!(kept(&bare, 1, &both), [row("s1", "us-east-1", "1", 3)]);
        // A router whose default is auto: so for every project that doesn't say.
        kept(&east, 1, &both);
        let auto: Vec<_> = east.keep_warm(KeepWarm::Auto).into_iter().map(|t| (t.deployment, t.keep)).collect();
        assert_eq!(auto, [("a2".to_owned(), KeepWarm::Auto), ("s1".to_owned(), KeepWarm::Count(3))]);
        // Production moves: the new deployment is kept from the next look, the old one no more.
        routes.hosts.insert("api.example.com".into(), "a1".into());
        east.apply(routes);
        assert_eq!(kept(&east, 1, &both), [row("a1", "us-east-1", "11", 1), row("s1", "us-east-1", "1", 3)]);
    }

    /// What a TLS handshake may start a worker for: a host in the table with a function.
    #[tokio::test]
    async fn the_handshake_finds_a_deployment_only_for_a_host_the_table_has() {
        use forkrail_common::routes::CustomDomain;
        let mut routes = Routes::default();
        let mut docs = deployment("docs", 300);
        (docs.function, docs.qualifier, docs.static_files) = (String::new(), String::new(), Some("a".repeat(64)));
        routes.deployments.insert("a1".into(), deployment("api", 300));
        routes.deployments.insert("d1".into(), docs);
        for (host, id) in [("api.example.com", "a1"), ("docs.example.com", "d1"), ("www.api.io", "a1")] {
            routes.hosts.insert(host.into(), id.into());
        }
        routes.domains.insert("www.api.io".into(), CustomDomain::new("api"));
        let registry = Registry::new(None, Backend::Local(PathBuf::from("/nonexistent")), template(), None, Some("example.com".into()));
        registry.apply(routes);
        let (pool, project, kept) = registry.at_handshake("api.example.com").expect("in the table");
        assert_eq!((pool.name(), project.as_str(), kept), ("a1", "api", 0));
        assert!(registry.at_handshake("www.api.io").is_some());
        assert!(registry.at_handshake("docs.example.com").is_none(), "static files alone");
        assert!(registry.at_handshake("nope.example.com").is_none());
        // What keep-warm found lately comes with it.
        registry.kept_warm("a1", 2);
        assert_eq!(registry.at_handshake("api.example.com").unwrap().2, 2);
    }

    #[test]
    fn source_specs() {
        assert!(matches!(Source::parse("/tmp/routes.json", || panic!("no S3 client for a file")).unwrap(), Source::File(_)));
        assert!(Source::parse("s3://bucket", || panic!("bad spec needs no client")).is_err());
    }
}
