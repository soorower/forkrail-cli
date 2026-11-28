//! Scheduler for one deployment: which workers are live, how busy each one is,
//! when to start another, and when to retire one.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use forkrail_common::{Inflight, InflightGuard, InvokePayload, Preface, WorkerReport, random_hex};
use hyper::client::conn::http2::SendRequest;
use tokio::sync::{Notify, watch};
use tracing::{info, warn};

use crate::logs::{self, Follow, LogLimiter};
use crate::proxy::ProxyBody;
use crate::spawner::{InvokeFailure, Spawner};
use crate::wire;

/// How often the reaper retires workers and starts spawns that are due.
const REAP_INTERVAL: Duration = Duration::from_millis(50);
/// Below this `idle`, the reaper also wakes when a slot frees and when a worker's `idle` runs out,
/// so workers retire on time rather than at the next tick.
const PRECISE_IDLE: Duration = Duration::from_millis(250);
/// Extra time the worker waits before retiring itself, in case the router
/// never closes the tunnel. Long, because the router keeps idle spares.
const WORKER_IDLE_GRACE: Duration = Duration::from_secs(60);
/// The router retires a kept spare this long before the worker's own idle timer
/// would fire, so the router, never the worker, closes tunnels.
const RETIRE_BEFORE_WORKER: Duration = Duration::from_secs(5);
/// A worker this close to `max_age` no longer counts as capacity, so its
/// replacement starts before it stops taking requests.
const REPLACE_LEAD: Duration = Duration::from_secs(30);
/// Back-off after failed spawns: doubles from the first value up to the second.
const BACKOFF_MIN: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// Beyond this many waiting requests, new ones fail at once instead of queueing.
const MAX_WAITING: usize = 1000;
/// Headroom is at least this share of one worker's slots.
const MIN_HEADROOM_PCT_OF_WORKER: usize = 30;
/// Spares are retired against the peak demand of the last one to two of these
/// windows, so a momentary dip doesn't retire a worker that's needed a moment later.
const PEAK_WINDOW: Duration = Duration::from_secs(10);
/// A spawn that hasn't connected after this long is probably a cold start: a thaw connects in
/// 17-31 ms, a cold start takes 250 ms to 2 s. At least; see `hedge_after`.
const HEDGE_AFTER: Duration = Duration::from_millis(100);
/// A spawn is taken for a cold start once it's taken this many times as long as this pool's
/// thaws do lately: a worker in another region thaws in about 194 ms (dialing back across the
/// country), past `HEDGE_AFTER`, which made 46% of one such pool's invocations second invokes
/// that weren't needed (2026-11-22).
const HEDGE_THAWS: u32 = 2;
/// Lambda keeps a frozen environment for about 5-10 minutes. Invocations open at once are
/// remembered for one to two of these windows, to guess how many environments are still frozen.
const ENVS_WINDOW: Duration = Duration::from_secs(180);
/// Share of a worker's memory limit kept free.
const MEMORY_MARGIN_PCT: u64 = 10;
/// Expected memory of a request on a route nothing has been learned about yet, in KiB.
const DEFAULT_ROUTE_KB: u64 = 64 * 1024;
const MIN_ROUTE_KB: u64 = 1024;
/// Routes whose memory cost is remembered; the rest use the default.
const MAX_ROUTES: usize = 1000;
/// Expected CPU share of a request on a route nothing has been learned about yet, in thousandths
/// of a core: the CPU time it uses over the time it takes.
const DEFAULT_ROUTE_CPU: u64 = 100;
const MIN_ROUTE_CPU: u64 = 1;
/// Lambda gives a function one vCPU at this memory size, and CPU in proportion below and above it.
const MB_PER_VCPU: u64 = 1769;
/// Retry budget: each request earns a tenth of a retry, so retries add at most about 10% load.
const RETRY_EARN: f64 = 0.1;
const RETRY_MAX: f64 = 10.0;
/// While a worker cap holds spawns back, warn at most this often.
const CAP_WARN_EVERY: Duration = Duration::from_secs(10);
/// How many invocations open at once are tracked for keeping environments alive: as many as a
/// project may have kept.
const KEEP_TRACKED: usize = forkrail_common::routes::KEEP_WARM_MAX as usize;

/// Workers open across every pool, against `FORKRAIL_MAX_WORKERS` (`usize::MAX` when there's no
/// cap, the default: then Lambda's account concurrency is the limit).
pub struct WorkerBudget {
    max: usize,
    live: AtomicUsize,
}

impl WorkerBudget {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self { max, live: AtomicUsize::new(0) })
    }

    pub fn live(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }

    fn try_take(&self) -> bool {
        self.live.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < self.max).then_some(n + 1)).is_ok()
    }

    fn give_back(&self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub struct PoolConfig {
    /// The deployment this pool runs, or empty for the default pool. Worker ids start with it
    /// (`<deployment>.<hex>`), so the tunnel listener knows which pool a worker belongs to.
    pub name: String,
    pub max_inflight: u32,
    /// Retire a worker after this long with nothing in flight.
    pub idle: Duration,
    /// `FORKRAIL_IDLE_MS=auto`: each deployment's `idle` is `economic_idle_ms` of its memory size,
    /// unless its table entry sets one.
    pub idle_auto: bool,
    /// Stop routing to a worker this long after it connects, before Lambda's 15-minute limit.
    pub max_age: Duration,
    /// Free slots to keep ahead of demand, as a share of demand (at least 30% of one
    /// worker), so most requests never wait for a spawn. Zero turns pre-spawning off.
    /// Without demand there is no headroom: once nothing has been in flight or waiting
    /// for `idle`, every worker retires, and the next request thaws a frozen environment.
    pub headroom_pct: u32,
    /// Address workers dial, as reachable from Lambda.
    pub tunnel_addr: String,
    pub cert_sha256: String,
    /// How long a request may wait for a free worker.
    pub acquire_timeout: Duration,
    /// Give up on a spawned worker that hasn't connected by then.
    pub connect_timeout: Duration,
    /// CPU budget per worker, as a percentage of the CPU its memory size buys (at most one core,
    /// as the app runs one thread): a request joins a busy worker only if the learned CPU shares
    /// of its requests, this one's included, stay within it. Zero turns CPU admission off.
    pub cpu_target_pct: u32,
    /// Share of requests admitted to an empty worker that get their CPU use sampled over time,
    /// for a tuner to read in the access log. Zero turns it off.
    pub profile_pct: u32,
    /// Workers this pool may have open at once, retired ones still draining included;
    /// `usize::MAX` for no cap.
    pub max_workers: usize,
    /// Shared by every pool.
    pub budget: Arc<WorkerBudget>,
    /// A request still unanswered after this long gets 504 `FUNCTION_INVOCATION_TIMEOUT`.
    pub max_duration: Duration,
    /// For the log: the deployment's project, the region its workers run in, and the function's
    /// memory size. Empty and zero for the default pool.
    pub project: String,
    pub region: String,
    pub memory_mb: u32,
    /// The link to the router that runs this deployment's requests (`relay.rs`): set when its
    /// version is in another region than this router's and that region has a router
    /// (`routers.rs`), when the pool is made. While the link is up, its requests go there instead
    /// of to workers started from here; with none, its workers run there and dial back.
    pub relay: Option<Arc<crate::relay::Slot>>,
    /// Also for the log: `production` or `preview`, the Lambda runtime and the app's entry file,
    /// as the routing table has them. Empty when it doesn't.
    pub environment: String,
    pub runtime: String,
    pub entry: String,
    /// Egress targets the deployment's apps may reach through the router, by name.
    pub egress: Vec<String>,
}

pub struct Pool {
    cfg: PoolConfig,
    spawner: Spawner,
    /// Its deployment left the routing table: no headroom, idle workers retire at once, and the
    /// reaper stops once the last one has.
    closed: AtomicBool,
    /// Shared by every worker's in-flight counter, so waiters wake when any slot frees up.
    changed: Arc<Notify>,
    state: Mutex<State>,
    /// Counts requests admitted alone, to profile `profile_pct` of them.
    alone_admitted: AtomicU64,
    /// The deployment's budgets of app log lines and outgoing call reports, shared by its workers.
    log_limiter: Arc<LogLimiter>,
    /// Connected workers by id, retired ones included until they're gone, for egress requests.
    connected: Mutex<HashMap<String, Weak<Worker>>>,
}

struct State {
    workers: Vec<Arc<Worker>>,
    /// Spawned workers that haven't connected yet, by worker id.
    starting: HashMap<String, Starting>,
    /// Requests currently waiting for a slot, how many of them must run alone, and how many are
    /// retries that avoid workers they already failed on.
    waiting: usize,
    waiting_alone: usize,
    waiting_avoiding: usize,
    /// Learned CPU shares of the waiting requests, in thousandths of a core.
    waiting_cpu: u64,
    /// Failed spawns in a row, and when the next spawn may start.
    spawn_failures: u32,
    next_spawn: Option<Instant>,
    /// Learned memory cost of one request, per route, in KiB.
    routes: HashMap<String, u64>,
    /// Learned CPU share of one request, per route, in thousandths of a core.
    route_cpu: HashMap<String, u64>,
    /// A worker's CPU budget, as of the last one that connected; zero if off or unknown.
    cpu_budget: u64,
    retry_tokens: f64,
    peak: Peak,
    /// Most invocations open at once lately: about how many environments Lambda still has, so
    /// `live` below it means some are probably frozen, and a new invoke would thaw one.
    envs: Peak,
    /// When demand was last above zero.
    last_demand: Instant,
    /// Workers whose invocation is open, retired ones still draining included.
    live: usize,
    /// When a worker cap last held a spawn back.
    capped_at: Option<Instant>,
    /// What each open invocation's `invocation` line will say, by worker id.
    lives: HashMap<String, Life>,
    /// What keeping environments alive goes by (`keepwarm.rs`): when a request last asked for a
    /// worker; `at_least[k - 1]`, the last moment at least k invocations were open at once (k up
    /// to `KEEP_TRACKED`; a moment that is still going on while `live` is k or more); and the
    /// most open at once since `activity` last asked.
    last_request: Option<Instant>,
    at_least: Vec<Instant>,
    busiest: usize,
    /// Requests expected soon, by the TLS handshakes that started workers for them
    /// (`start_early`): idle workers are kept for them until then.
    expecting: HashMap<u64, Instant>,
    next_expecting: u64,
    /// How long this pool's thaws take to connect, as a moving average: across regions it's the
    /// round trips of the worker's dial, not the thaw.
    thaw_connect: Option<Duration>,
}

/// What the log's `invocation` line says about a worker, gathered over its life: one line per
/// Lambda invocation, which the analytics store reads for cold starts and billed time.
#[derive(Default)]
struct Life {
    hedge: bool,
    /// From the invoke until its tunnel was up.
    connect_ms: Option<u64>,
    /// Why the router retired it (`idle`, `max_age`, `closing`), and after how long idle.
    retired: Option<&'static str>,
    idle_ms: u64,
    /// Its tunnel failed with requests in flight.
    died: bool,
    /// How long it was kept, with nothing in flight, for work its app was doing behind an answer.
    background_ms: Option<u64>,
    /// Started at a visitor's TLS handshake (`start_early`): whether a request used it since.
    early: Option<Arc<AtomicBool>>,
    /// A handle on its tunnel's socket, kept until the kernel's byte counts are read from it:
    /// when the tunnel ends, or at the invocation's end if the tunnel is still up then (a worker
    /// that vanished without closing it).
    socket: Option<OwnedFd>,
    /// What crossed between the router and the worker, as the kernel counted it: the tunnel, and
    /// the egress connections that ended before the invocation did.
    wire: Option<wire::Bytes>,
    /// The tunnel on the spend ledger, read as it runs (`spend.rs`).
    spend: Option<Arc<crate::spend::Conn>>,
    /// What the app sent the internet itself, as its log stream said (`LogLine::Net`).
    net_streamed: Arc<AtomicU64>,
}

impl Life {
    fn crossed(&mut self, bytes: Option<wire::Bytes>) {
        if let Some(bytes) = bytes {
            *self.wire.get_or_insert_default() += bytes;
        }
    }

    /// Read the tunnel's byte counts, unless they've been read, and give up its socket for the
    /// caller to close.
    fn count_tunnel(&mut self) -> Option<OwnedFd> {
        if let Some(spend) = self.spend.take() {
            spend.close(crate::spend::ledger());
        }
        let socket = self.socket.take()?;
        self.crossed(wire::bytes(&socket));
        Some(socket)
    }
}

/// Highest demand seen in the current and the previous `PEAK_WINDOW`.
struct Peak {
    window: Duration,
    current: usize,
    previous: usize,
    window_start: Instant,
}

impl Peak {
    fn new(window: Duration) -> Self {
        Self { window, current: 0, previous: 0, window_start: Instant::now() }
    }

    fn record(&mut self, value: usize, now: Instant) {
        let elapsed = now - self.window_start;
        if elapsed >= self.window {
            // Nothing recorded for a whole window: the last one's peak is too old to count.
            self.previous = if elapsed >= 2 * self.window { 0 } else { self.current };
            self.current = 0;
            self.window_start = now;
        }
        self.current = self.current.max(value);
    }

    fn get(&self) -> usize {
        self.current.max(self.previous)
    }
}

struct Starting {
    token: String,
    since: Instant,
    /// Its preface checked out; it's still counted as starting until it registers, so requests
    /// waiting for it don't start another worker meanwhile.
    claimed: bool,
    /// A second invoke was started beside it because it seemed stuck in a cold start, or it is
    /// that second invoke: either way it counts as capacity however long it takes.
    hedged: bool,
    /// Started at a visitor's TLS handshake, before the request.
    early: bool,
}

pub struct Worker {
    pub id: String,
    sender: SendRequest<ProxyBody>,
    inflight: Arc<Inflight>,
    connected_at: Instant,
    idle_since: Mutex<Option<Instant>>,
    /// The function's memory size in KiB. Zero means unknown, which turns memory admission off.
    limit_kb: u64,
    /// Memory the requests in flight don't account for, as of the last report.
    base_kb: AtomicU64,
    /// CPU the requests in flight may use together, in thousandths of a core; zero means no limit.
    cpu_budget: u64,
    load: Mutex<Load>,
    /// Its tunnel failed with requests in flight.
    died: AtomicBool,
    /// Running a request that must not share the worker.
    running_alone: AtomicBool,
    /// How its environment started: `cold` (new) or `thaw` (frozen, re-invoked). A request that
    /// waited for it to connect says so in the access log.
    pub start: &'static str,
    /// Its log stream, which ends when the last handle to the worker drops, so it never keeps a
    /// retired worker's tunnel open.
    _logs: Option<Follow>,
    /// Its Next.js cache stream (`cache/next.rs`), which ends the same way.
    _cache: Option<crate::cache::next::Follow>,
    /// Work its app is doing behind its answers (`waitUntil`, Next's `after()`), as its log stream
    /// says: while there is some it isn't idle, up to the deployment's maxDuration.
    background: Arc<logs::Background>,
    /// Started at a visitor's TLS handshake: set once a request uses it, for its `invocation` line.
    early_used: Option<Arc<AtomicBool>>,
    /// The token it connected with, which its egress requests present too.
    token: String,
    /// Dropped with the worker, which ends its egress connections.
    gone: watch::Sender<()>,
    /// Told when its project is paused (`budgets.rs`): its tunnel closes at once, requests in
    /// flight and all, so its invocation returns and nothing more is spent.
    pub stop: Arc<tokio::sync::Notify>,
    /// Not a worker but the link to another router, which runs the requests it's sent
    /// (`relay.rs`): what goes on it must prove it's this router's, and what comes back the other's.
    relay: Option<crate::relay::Proofs>,
}

/// Compares secrets without stopping at the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What the requests in flight on a worker are expected to use.
#[derive(Default)]
struct Load {
    reserved_kb: u64,
    reserved_cpu: u64,
    routes: Vec<String>,
}

impl Worker {
    /// The link to another router as a worker that never retires (`relay.rs`): a pool leases it
    /// for any number of requests at once, and the other router finds them workers.
    pub fn relay(id: String, sender: SendRequest<ProxyBody>, proofs: crate::relay::Proofs) -> Arc<Worker> {
        let changed = Arc::new(Notify::new());
        Arc::new(Worker {
            id,
            sender,
            inflight: Arc::new(Inflight::new(changed.clone())),
            connected_at: Instant::now(),
            idle_since: Mutex::new(None),
            limit_kb: 0,
            base_kb: AtomicU64::new(0),
            cpu_budget: 0,
            load: Mutex::default(),
            died: AtomicBool::new(false),
            running_alone: AtomicBool::new(false),
            start: "relay",
            _logs: None,
            _cache: None,
            background: Arc::new(logs::Background::new(changed)),
            early_used: None,
            token: String::new(),
            gone: watch::channel(()).0,
            stop: Arc::default(),
            relay: Some(proofs),
        })
    }

    /// For the link to another router: its proofs.
    pub fn relayed(&self) -> Option<&crate::relay::Proofs> {
        self.relay.as_ref()
    }

    /// For the link to another router: stop using it; `relay.rs` opens it again.
    pub fn close_link(&self) {
        self.died.store(true, Ordering::Relaxed);
    }

    /// Open, and no request found it broken.
    pub fn usable(&self) -> bool {
        !self.died.load(Ordering::Relaxed) && !self.sender.is_closed()
    }

    pub fn inflight(&self) -> usize {
        self.inflight.count()
    }

    fn budget_kb(&self) -> u64 {
        self.limit_kb * (100 - MEMORY_MARGIN_PCT) / 100
    }

    /// Not dead, closed or held by a request running alone.
    fn healthy(&self) -> bool {
        !self.died.load(Ordering::Relaxed) && !self.running_alone.load(Ordering::Relaxed) && !self.sender.is_closed()
    }

    fn has_room(&self, max_inflight: usize, cost_kb: u64, cpu: u64, alone: bool) -> bool {
        let count = self.inflight.count();
        if count >= max_inflight || !self.healthy() {
            return false;
        }
        if alone {
            return count == 0;
        }
        // An empty worker takes any request, or one that's too big would never run.
        if count == 0 {
            return true;
        }
        let load = self.load.lock().unwrap();
        let memory = self.limit_kb == 0 || self.base_kb.load(Ordering::Relaxed) + load.reserved_kb + cost_kb <= self.budget_kb();
        let cpu = self.cpu_budget == 0 || load.reserved_cpu + cpu <= self.cpu_budget;
        memory && cpu
    }
}

/// A slot on a worker. Held until the worker has sent the whole response;
/// dropping the last lease of a retired worker closes its tunnel.
pub struct Lease {
    pub worker: Arc<Worker>,
    pub leased_at: Instant,
    /// `warm`: an open worker took it. `cold` or `thaw`: it waited for this worker to start.
    pub start: &'static str,
    route: String,
    cost_kb: u64,
    cpu: u64,
    /// Admitted to an empty worker, so its memory growth can be measured.
    admitted_alone: bool,
    /// Nothing else may join the worker while this lease lasts.
    exclusive: bool,
    _guard: InflightGuard,
}

impl Lease {
    pub fn sender(&self) -> SendRequest<ProxyBody> {
        self.worker.sender.clone()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut load = self.worker.load.lock().unwrap();
        load.reserved_kb = load.reserved_kb.saturating_sub(self.cost_kb);
        load.reserved_cpu = load.reserved_cpu.saturating_sub(self.cpu);
        if let Some(i) = load.routes.iter().position(|r| *r == self.route) {
            load.routes.swap_remove(i);
        }
        if self.exclusive {
            self.worker.running_alone.store(false, Ordering::Relaxed);
        }
    }
}

/// No worker could take the request in time, or too many are waiting.
#[derive(Debug)]
pub struct Unavailable {
    pub retry_after: Duration,
}

/// What a pool's invocations did lately, for keeping environments alive (`Pool::activity`).
#[derive(Debug, Clone, Default)]
pub struct Activity {
    /// The most invocations open at once since the last time it was asked: how many environments
    /// the deployment needed then.
    pub busiest: usize,
    /// `at_least[k - 1]`: how long ago at least k invocations were last open at once, zero while
    /// they are; as far as k has gone (at most `KEEP_TRACKED`). Each of those k had an environment
    /// of its own, whose idle clock started no earlier.
    pub at_least: Vec<Duration>,
    /// How long ago a request last asked for a worker; None if none has.
    pub since_request: Option<Duration>,
}

/// A request expected soon, for which a TLS handshake started a worker (`Pool::start_early`):
/// idle workers are kept until it drops (when the connection's first request has its answer, or
/// the connection ends) or its time is up.
pub struct Expected {
    pool: Weak<Pool>,
    id: u64,
}

impl Drop for Expected {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.state.lock().unwrap().expecting.remove(&self.id);
            // In case a worker is idle past its time only for this: it retires now.
            pool.changed.notify_waiters();
        }
    }
}

impl Pool {
    pub fn new(cfg: PoolConfig, spawner: Spawner) -> Arc<Self> {
        let state = State {
            workers: Vec::new(),
            starting: HashMap::new(),
            lives: HashMap::new(),
            waiting: 0,
            waiting_alone: 0,
            waiting_avoiding: 0,
            waiting_cpu: 0,
            spawn_failures: 0,
            next_spawn: None,
            routes: HashMap::new(),
            route_cpu: HashMap::new(),
            cpu_budget: 0,
            retry_tokens: RETRY_MAX,
            peak: Peak::new(PEAK_WINDOW),
            envs: Peak::new(ENVS_WINDOW),
            last_demand: Instant::now(),
            live: 0,
            capped_at: None,
            last_request: None,
            at_least: Vec::new(),
            busiest: 0,
            expecting: HashMap::new(),
            next_expecting: 0,
            thaw_connect: None,
        };
        let log_limiter = Arc::new(LogLimiter::new(if cfg.name.is_empty() { "default" } else { &cfg.name }));
        Arc::new(Self {
            cfg,
            spawner,
            closed: AtomicBool::new(false),
            changed: Arc::new(Notify::new()),
            state: Mutex::new(state),
            alone_admitted: AtomicU64::new(0),
            log_limiter,
            connected: Mutex::new(HashMap::new()),
        })
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    pub fn max_duration(&self) -> Duration {
        self.cfg.max_duration
    }

    /// For keeping environments alive (`keepwarm.rs`): what its invocations did lately. Each
    /// invocation open holds an environment of its own (workers still starting or draining
    /// included), whose idle clock starts when it ends. Takes the busiest moment: the next call
    /// gets the busiest since this one.
    pub fn activity(&self) -> Activity {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let busiest = state.busiest.max(state.live);
        state.busiest = state.live;
        let live = state.live;
        let at_least = state.at_least.iter().enumerate().map(|(i, at)| if live > i { Duration::ZERO } else { now - *at }).collect();
        Activity { busiest, at_least, since_request: state.last_request.map(|at| now - at) }
    }

    /// Start a worker for a request expected soon: a visitor's TLS handshake names this deployment
    /// (`early.rs`), and the request after it would otherwise wait for a cold start. Only then:
    /// none while a worker has a free slot, a start is coming that no waiting request has
    /// claimed, an environment is probably frozen (this pool's own count of them, and `kept`,
    /// those keep-warm kept lately), another start of this kind is in flight, or spawning backs
    /// off. The worker is kept for the request until the guard drops or `hold` passes, whichever
    /// is first; then the usual idle rule applies, and Lambda keeps its environment frozen for the
    /// next one.
    pub fn start_early(self: &Arc<Self>, kept: usize, hold: Duration) -> Option<Expected> {
        // Another router runs its requests: it starts the workers.
        if self.closed.load(Ordering::Relaxed) || self.relayed() {
            return None;
        }
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        let max = self.cfg.max_inflight as usize;
        let frozen = state.envs.get().max(kept).saturating_sub(state.live);
        if frozen > 0
            || state.starting.values().any(|s| s.early)
            || state.starting.len() > state.waiting
            || state.next_spawn.is_some_and(|at| now < at)
            || state.workers.iter().any(|w| w.inflight.count() < max && w.healthy() && !self.aging(w, now))
        {
            return None;
        }
        if !self.spawn(&mut state, now, false, true) {
            return None;
        }
        let id = state.next_expecting;
        state.next_expecting += 1;
        state.expecting.insert(id, now + hold);
        Some(Expected { pool: Arc::downgrade(self), id })
    }

    /// Take no more headroom: the deployment is gone. Requests already here still run.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    /// Lease a slot for a request on `route`, on any worker not in `avoid`. With
    /// `alone`, the request gets a worker to itself, so if it is what kills workers,
    /// it kills nothing else.
    pub async fn acquire(self: &Arc<Self>, route: &str, avoid: &[String], alone: bool) -> Result<Lease, Unavailable> {
        // A paused project starts nothing, a retry of a request its stopped worker had included.
        if crate::budgets::paused(&self.cfg.project).is_some() {
            return Err(Unavailable { retry_after: Duration::from_secs(60) });
        }
        if let Some(lease) = self.relay_lease(route, avoid) {
            return Ok(lease);
        }
        let deadline = tokio::time::Instant::now() + self.cfg.acquire_timeout;
        let mut waiting = Waiting { pool: self, counted: false, alone, avoiding: !avoid.is_empty(), cpu: 0 };
        let asked = Instant::now();
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            // Paused while it waited (`stop_all` wakes every waiter).
            if crate::budgets::paused(&self.cfg.project).is_some() {
                return Err(Unavailable { retry_after: Duration::from_secs(60) });
            }
            if let Some(mut lease) = self.lease_or_spawn(route, avoid, alone, &mut waiting)? {
                if lease.worker.connected_at > asked {
                    lease.start = lease.worker.start;
                }
                return Ok(lease);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(Unavailable { retry_after: retry_after(&self.state.lock().unwrap()) });
            }
        }
    }

    /// The link to the router that runs this deployment (`relay.rs`), if it has one, it's up, and
    /// the request didn't fail on it already. It takes any number of requests, which that router
    /// finds workers for, so nothing here counts them.
    fn relay_lease(&self, route: &str, avoid: &[String]) -> Option<Lease> {
        let worker = self.cfg.relay.as_ref()?.worker()?;
        if avoid.contains(&worker.id) {
            return None;
        }
        let mut lease = lease(worker, route, 0, 0, false);
        // Until the answer says how that router's worker started.
        lease.start = "relay";
        Some(lease)
    }

    /// Whether its requests go to another router now (`relay_lease`).
    pub fn relayed(&self) -> bool {
        self.cfg.relay.as_ref().is_some_and(|slot| slot.worker().is_some())
    }

    /// `waiting` says whether this request is counted in `State::waiting`; it is
    /// exactly while it has no lease, so the spawn rules see real demand.
    fn lease_or_spawn(
        self: &Arc<Self>,
        route: &str,
        avoid: &[String],
        alone: bool,
        waiting: &mut Waiting<'_>,
    ) -> Result<Option<Lease>, Unavailable> {
        let mut state = self.state.lock().unwrap();
        let now = Instant::now();
        state.last_request = Some(now);
        let max = self.cfg.max_inflight as usize;
        let cost_kb = state.routes.get(route).copied().unwrap_or(DEFAULT_ROUTE_KB);
        let cpu = state.route_cpu.get(route).copied().unwrap_or(DEFAULT_ROUTE_CPU);
        // Pack: the busiest worker that still has room, so the others drain and retire.
        // Workers about to be replaced come last, so they drain too.
        let best = state
            .workers
            .iter()
            .filter(|w| !avoid.contains(&w.id) && w.has_room(max, cost_kb, cpu, alone))
            .max_by_key(|w| (!self.aging(w, now), w.inflight.count()))
            .cloned();
        let lease = best.map(|worker| lease(worker, route, cost_kb, cpu, alone));
        match (&lease, waiting.counted) {
            (Some(_), true) => {
                state.waiting -= 1;
                state.waiting_alone -= usize::from(alone);
                state.waiting_avoiding -= usize::from(waiting.avoiding);
                state.waiting_cpu -= waiting.cpu;
            }
            (None, false) if state.waiting >= MAX_WAITING => {
                return Err(Unavailable { retry_after: retry_after(&state) });
            }
            (None, false) => {
                state.waiting += 1;
                state.waiting_alone += usize::from(alone);
                state.waiting_avoiding += usize::from(waiting.avoiding);
                state.waiting_cpu += cpu;
                waiting.cpu = cpu;
            }
            _ => {}
        }
        waiting.counted = lease.is_none();
        self.scale(&mut state, now);
        Ok(lease)
    }

    fn aging(&self, worker: &Worker, now: Instant) -> bool {
        now - worker.connected_at >= self.cfg.max_age.saturating_sub(REPLACE_LEAD)
    }

    /// Start workers for waiting requests and for pre-spawn headroom, unless spawning is backing off.
    fn scale(self: &Arc<Self>, state: &mut State, now: Instant) {
        let connect_timeout = self.cfg.connect_timeout;
        let expired: Vec<String> = state
            .starting
            .iter()
            .filter(|(_, s)| now - s.since >= connect_timeout)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            state.starting.remove(&id);
            self.spawn_failed(state, &id, "never connected");
        }
        let demand = demand(state);
        state.peak.record(demand, now);
        state.envs.record(state.live, now);
        if demand > 0 {
            state.last_demand = now;
        }
        if state.next_spawn.is_some_and(|at| now < at) {
            return;
        }
        let per_worker = self.per_worker(state);
        // Connected workers with nothing in flight count like starting ones: a waiting request is
        // about to take each, since it just connected or its last request just ended. Retries
        // that avoid workers might not take one, so these cover only the other waiting requests.
        let ready = state
            .workers
            .iter()
            .filter(|w| w.inflight.count() == 0 && w.healthy() && !self.aging(w, now))
            .count()
            .min(state.waiting - state.waiting_avoiding);
        // Bounded in case the rules disagree with themselves; the reaper calls again soon.
        for _ in 0..64 {
            // Workers the waiting requests need: one each for those that must run alone,
            // and enough full workers for the rest.
            let alone = state.waiting_alone;
            let needed = alone + (state.waiting - alone).div_ceil(per_worker);
            let coming = state.starting.len() + ready;
            // A spawn that seems stuck in a cold start stops counting while Lambda probably still
            // has a frozen environment: waiting requests shouldn't sit out 1-2 s when another
            // invoke would likely thaw one in about 20 ms. They take whichever worker connects
            // first; the other serves later requests or retires.
            let stuck = self.stuck(state, now);
            let hedge = needed <= coming && needed > coming - stuck.len();
            if needed <= coming - if hedge { stuck.len() } else { 0 }
                && !self.short_of_headroom(state, now, 0, demand, per_worker)
            {
                break;
            }
            if !self.spawn(state, now, hedge, false) {
                break;
            }
            if hedge && let Some(s) = stuck.first().and_then(|id| state.starting.get_mut(id)) {
                s.hedged = true;
            }
        }
    }

    /// Spawns that seem stuck in a cold start and have no second invoke beside them yet, oldest
    /// first; at most as many as there are probably frozen environments for another invoke to
    /// thaw, and none if there are none.
    fn stuck(&self, state: &State, now: Instant) -> Vec<String> {
        let frozen = state.envs.get().saturating_sub(state.live);
        if frozen == 0 {
            return Vec::new();
        }
        let after = hedge_after(state.thaw_connect);
        let mut stuck: Vec<(&String, Instant)> = state
            .starting
            .iter()
            .filter(|(_, s)| !s.claimed && !s.hedged && now - s.since >= after)
            .map(|(id, s)| (id, s.since))
            .collect();
        stuck.sort_by_key(|(_, since)| *since);
        stuck.into_iter().take(frozen).map(|(id, _)| id.clone()).collect()
    }

    /// How many requests one worker takes at once: `max_inflight`, or fewer where the CPU
    /// budget binds first for the kind of requests now in flight and waiting. With no demand it's
    /// `max_inflight`, so idle spares retire quickly: the simulator in `tools/tune-memory.py` found
    /// that cheaper than keeping them, since a frozen environment thaws in about 25 ms.
    fn per_worker(&self, state: &State) -> usize {
        let max = self.cfg.max_inflight as usize;
        let demand = demand(state) as u64;
        if state.cpu_budget == 0 || demand == 0 {
            return max;
        }
        let in_flight: u64 = state.workers.iter().map(|w| w.load.lock().unwrap().reserved_cpu).sum();
        let typical = ((in_flight + state.waiting_cpu) / demand).max(MIN_ROUTE_CPU);
        ((state.cpu_budget / typical) as usize).clamp(1, max)
    }

    /// Whether free slots on fresh and starting workers, with `fewer` fresh workers
    /// than there are, would fall below the headroom for `demand`. Zero demand needs none.
    fn short_of_headroom(&self, state: &State, now: Instant, fewer: usize, demand: usize, max: usize) -> bool {
        let pct = self.cfg.headroom_pct as usize;
        if pct == 0 || demand == 0 || self.closed.load(Ordering::Relaxed) {
            return false;
        }
        let fresh = state.workers.iter().filter(|w| !self.aging(w, now)).count().saturating_sub(fewer);
        let headroom = (demand * pct).div_ceil(100).max((max * MIN_HEADROOM_PCT_OF_WORKER).div_ceil(100));
        (fresh + state.starting.len()) * max < demand + headroom
    }

    /// Start a worker, unless this pool or the router is at its cap. A `hedge` is a second invoke
    /// beside a spawn that seems stuck in a cold start; an `early` one is for a request whose TLS
    /// handshake has begun (`start_early`).
    fn spawn(self: &Arc<Self>, state: &mut State, now: Instant, hedge: bool, early: bool) -> bool {
        if crate::budgets::paused(&self.cfg.project).is_some() {
            return false;
        }
        if state.live >= self.cfg.max_workers || !self.cfg.budget.try_take() {
            if state.capped_at.is_none_or(|at| now - at >= CAP_WARN_EVERY) {
                warn!(
                    pool = %self.cfg.name,
                    live = state.live,
                    max_workers = self.cfg.max_workers,
                    router_live = self.cfg.budget.live(),
                    router_max = self.cfg.budget.max,
                    "worker cap reached; requests wait for a free slot"
                );
            }
            state.capped_at = Some(now);
            return false;
        }
        state.live += 1;
        state.envs.record(state.live, now);
        state.busiest = state.busiest.max(state.live);
        if state.at_least.len() < state.live.min(KEEP_TRACKED) {
            state.at_least.push(now);
        }
        let worker_id = match self.cfg.name.as_str() {
            "" => random_hex(8),
            name => format!("{name}.{}", random_hex(8)),
        };
        let token = random_hex(16);
        state.starting.insert(worker_id.clone(), Starting { token: token.clone(), since: Instant::now(), claimed: false, hedged: hedge, early });
        state.lives.insert(worker_id.clone(), Life { hedge, early: early.then(Arc::default), ..Life::default() });
        let payload = InvokePayload {
            worker_id,
            router_addr: self.cfg.tunnel_addr.clone(),
            token,
            cert_sha256: self.cfg.cert_sha256.clone(),
            max_inflight: self.cfg.max_inflight,
            idle_ms: (self.cfg.idle + WORKER_IDLE_GRACE).as_millis() as u64,
            max_duration_s: Some(self.cfg.max_duration.as_secs()),
            own_hosts: Some(crate::loops::own_hosts().to_vec()),
            // On Lambda only: a local worker shares this machine's network (`net.rs`).
            report_net: matches!(self.spawner, Spawner::Lambda { .. }),
        };
        if hedge {
            info!(worker = %payload.worker_id, hedge, "spawning worker");
        } else if early {
            info!(worker = %payload.worker_id, early, "spawning worker");
        } else {
            info!(worker = %payload.worker_id, "spawning worker");
        }
        let pool = self.clone();
        tokio::spawn(async move {
            // Runs for the worker's whole life, and is billed for all of it (`spend.rs`).
            let spent = crate::spend::ledger().invocation(pool.spend_owner(), pool.cfg.memory_mb, false);
            let (secs, outcome) = pool.spawner.invoke(&payload).await;
            drop(spent);
            pool.ended(&payload.worker_id, secs, outcome);
        });
        true
    }

    fn spawn_failed(&self, state: &mut State, worker_id: &str, reason: &str) {
        state.spawn_failures += 1;
        let backoff = BACKOFF_MIN.saturating_mul(1 << (state.spawn_failures - 1).min(16)).min(BACKOFF_MAX);
        state.next_spawn = Some(Instant::now() + backoff);
        warn!(
            worker = %worker_id,
            reason,
            failures = state.spawn_failures,
            backoff_ms = backoff.as_millis() as u64,
            "spawn failed"
        );
    }

    /// Check a connecting worker's preface. Each token works once.
    /// Returns how long ago the worker was spawned.
    pub fn claim(&self, worker_id: &str, token: &str) -> Option<Duration> {
        let mut state = self.state.lock().unwrap();
        let since = match state.starting.get_mut(worker_id) {
            Some(s) if constant_time_eq(s.token.as_bytes(), token.as_bytes()) && !s.claimed => {
                s.claimed = true;
                s.since.elapsed()
            }
            _ => return None,
        };
        if let Some(life) = state.lives.get_mut(worker_id) {
            life.connect_ms = Some(since.as_millis() as u64);
        }
        state.spawn_failures = 0;
        state.next_spawn = None;
        Some(since)
    }

    /// A worker's tunnel is up. `socket` is a second handle on its connection (`wire::hold`),
    /// for reading what crossed it when it ends.
    pub fn register(&self, preface: &Preface, sender: SendRequest<ProxyBody>, socket: Option<OwnedFd>) -> Arc<Worker> {
        // The app runs one thread, so CPU past one core doesn't speed up its requests.
        let cores_milli = (preface.limit_kb * 1000 / (MB_PER_VCPU * 1024)).min(1000);
        let cpu_budget = cores_milli * u64::from(self.cfg.cpu_target_pct) / 100;
        let deployment = if self.cfg.name.is_empty() { "default" } else { &self.cfg.name };
        let (start, early_used) = {
            let mut state = self.state.lock().unwrap();
            let life = state.lives.get(&preface.worker_id);
            let connect_ms = life.and_then(|l| l.connect_ms);
            let start = start_kind(preface.env_invocations, connect_ms);
            let early_used = life.and_then(|l| l.early.clone());
            if let (Some(ms), "thaw") = (connect_ms, start) {
                state.thaw_connect = Some(learn_thaw(state.thaw_connect, Duration::from_millis(ms)));
            }
            (start, early_used)
        };
        let background = Arc::new(logs::Background::new(self.changed.clone()));
        let net_streamed = self.state.lock().unwrap().lives.get(&preface.worker_id).map(|l| l.net_streamed.clone()).unwrap_or_default();
        let follow = preface.logs.then(|| {
            let net = Some((self.spend_owner(), net_streamed.clone()));
            logs::follow(&sender, &preface.token, &self.cfg.project, deployment, &preface.worker_id, self.log_limiter.clone(), background.clone(), net)
        });
        // A Next.js worker's caches are this router's: its stream is opened with its tunnel.
        let cache = preface.cache.then(crate::cache::next::get).flatten().map(|next| {
            let ident = crate::cache::next::Ident { worker: preface.worker_id.clone(), project: self.cfg.project.clone(), environment: self.cfg.environment.clone(), deployment: self.cfg.name.clone() };
            next.follow(&sender, &preface.token, ident)
        });
        let worker = Arc::new(Worker {
            id: preface.worker_id.clone(),
            sender,
            inflight: Arc::new(Inflight::new(self.changed.clone())),
            connected_at: Instant::now(),
            idle_since: Mutex::new(None),
            limit_kb: preface.limit_kb,
            base_kb: AtomicU64::new(preface.rss_kb),
            cpu_budget,
            load: Mutex::default(),
            died: AtomicBool::new(false),
            running_alone: AtomicBool::new(false),
            start,
            _logs: follow,
            _cache: cache,
            background,
            early_used,
            token: preface.token.clone(),
            gone: watch::channel(()).0,
            stop: Arc::default(),
            relay: None,
        });
        {
            let mut connected = self.connected.lock().unwrap();
            connected.retain(|_, w| w.strong_count() > 0);
            connected.insert(preface.worker_id.clone(), Arc::downgrade(&worker));
        }
        let mut state = self.state.lock().unwrap();
        // Starting until now, so the requests waiting for it started no other.
        state.starting.remove(&preface.worker_id);
        if let Some(life) = state.lives.get_mut(&preface.worker_id) {
            life.spend = socket.as_ref().and_then(|s| s.try_clone().ok()).map(|fd| crate::spend::ledger().tunnel(Some(fd), self.spend_owner()));
            life.socket = socket;
        }
        state.workers.push(worker.clone());
        state.cpu_budget = cpu_budget;
        drop(state);
        self.changed.notify_waiters();
        worker
    }

    /// For an egress request: the receiver that ends when worker `worker_id` goes, if it's still
    /// here and `token` is the one it connected with.
    pub fn live(&self, worker_id: &str, token: &str) -> Option<watch::Receiver<()>> {
        let worker = self.connected.lock().unwrap().get(worker_id)?.upgrade()?;
        (constant_time_eq(worker.token.as_bytes(), token.as_bytes()) && !worker.sender.is_closed()).then(|| worker.gone.subscribe())
    }

    /// Stop routing to a worker. Its tunnel closes once in-flight requests finish.
    pub fn remove(&self, worker_id: &str) {
        self.state.lock().unwrap().workers.retain(|w| w.id != worker_id);
        self.changed.notify_waiters();
    }

    /// The worker's tunnel ended: read what crossed it, once, and let go of its socket.
    pub fn tunnel_closed(&self, worker_id: &str) {
        let socket = self.state.lock().unwrap().lives.get_mut(worker_id).and_then(Life::count_tunnel);
        // Closed outside the lock: if hyper has dropped the stream, this is what closes it.
        drop(socket);
    }

    /// One of the worker's egress connections ended, having carried this much between the
    /// worker and the router. One that outlasts the invocation isn't counted.
    pub fn egress_closed(&self, worker_id: &str, bytes: Option<wire::Bytes>) {
        if let Some(b) = bytes {
            crate::spend::ledger().tunnel_bytes(&self.spend_owner(), b.sent, b.received);
        }
        if let Some(life) = self.state.lock().unwrap().lives.get_mut(worker_id) {
            life.crossed(bytes);
        }
    }

    /// Let every worker go now, requests in flight with them: the project is paused. The pool
    /// stays, for when it's resumed.
    pub fn stop_all(&self, reason: &'static str) {
        let mut state = self.state.lock().unwrap();
        let workers = std::mem::take(&mut state.workers);
        for w in &workers {
            if let Some(life) = state.lives.get_mut(&w.id) {
                life.retired = Some(reason);
            }
            w.stop.notify_one();
        }
        drop(state);
        if !workers.is_empty() {
            warn!(pool = %self.cfg.name, workers = workers.len(), reason, "workers stopped");
        }
        self.changed.notify_waiters();
    }

    /// Whose this pool's spending is: its project's (the install's for the default pool), in the
    /// region its functions run in.
    pub fn spend_owner(&self) -> crate::spend::Owner {
        let region = if self.cfg.region.is_empty() { crate::spend::ledger().region() } else { &self.cfg.region };
        crate::spend::Owner::new(&self.cfg.project, &self.cfg.name, region)
    }

    /// The deployment's project, for the access log; empty for the default pool.
    pub fn project(&self) -> &str {
        &self.cfg.project
    }

    /// What the access log says about the deployment.
    pub fn config(&self) -> &PoolConfig {
        &self.cfg
    }

    /// The deployment's log budgets, which also count what each request's log is missing.
    pub fn logs(&self) -> &Arc<LogLimiter> {
        &self.log_limiter
    }

    /// The region this pool's workers run in, for the access log.
    pub fn region(&self) -> &str {
        &self.cfg.region
    }

    /// The worker's invocation returned; drop anything still pointing at it.
    fn ended(&self, worker_id: &str, secs: f64, outcome: Result<Option<WorkerReport>, InvokeFailure>) {
        let mut state = self.state.lock().unwrap();
        let mut life = state.lives.remove(worker_id).unwrap_or_default();
        // As many as `live` were open at once until now.
        let live = state.live;
        if let Some(at) = state.at_least.get_mut(live - 1) {
            *at = Instant::now();
        }
        state.live -= 1;
        self.cfg.budget.give_back();
        if state.starting.remove(worker_id).is_some() {
            let reason = match &outcome {
                Err(e) if e.throttled => "throttled",
                Err(_) => "invoke failed",
                Ok(_) => "ended before connecting",
            };
            self.spawn_failed(&mut state, worker_id, reason);
        }
        state.workers.retain(|w| w.id != worker_id);
        drop(state);
        self.changed.notify_waiters();
        // The tunnel outlived the invocation, as when Lambda kills the environment and nothing
        // closes the connection: its counts as they stand.
        drop(life.count_tunnel());
        // What the app sent the internet itself in its last moments, which its log stream didn't.
        if let Ok(Some(report)) = &outcome
            && let Some(total) = report.net_out
        {
            let tail = total.saturating_sub(life.net_streamed.load(Ordering::Relaxed));
            crate::spend::ledger().add(&self.spend_owner(), forkrail_common::prices::Meter::FnEgressBytes, None, tail);
        }
        self.log_invocation(worker_id, secs, &outcome, life);
    }

    /// One line per invocation, with what the router and the worker know about it.
    fn log_invocation(&self, worker: &str, secs: f64, outcome: &Result<Option<WorkerReport>, InvokeFailure>, life: Life) {
        let report = outcome.as_ref().ok().and_then(Option::as_ref);
        let (result, error) = match outcome {
            Ok(_) => ("ok", None),
            Err(e) if e.throttled => ("throttled", Some(e.message.as_str())),
            Err(e) => ("failed", Some(e.message.as_str())),
        };
        let some = |s: &str| Some(s.to_owned()).filter(|s| !s.is_empty());
        info!(
            worker,
            project = some(&self.cfg.project),
            deployment = some(&self.cfg.name),
            region = some(&self.cfg.region),
            memory_mb = self.cfg.memory_mb,
            max_duration_s = self.cfg.max_duration.as_secs(),
            environment = some(&self.cfg.environment),
            runtime = some(&self.cfg.runtime),
            entrypoint = some(&self.cfg.entry),
            duration_ms = (secs * 1000.0).round() as u64,
            result,
            hedge = life.hedge,
            connect_ms = life.connect_ms,
            retired = life.retired,
            idle_ms = life.idle_ms,
            died = life.died,
            background_ms = life.background_ms,
            early = life.early.as_ref().map(|used| if used.load(Ordering::Relaxed) { "used" } else { "unused" }),
            env = report.map(|r| r.env_id.as_str()),
            env_invocations = report.map(|r| r.env_invocations),
            requests = report.map(|r| r.requests),
            app_start_ms = report.map(|r| r.app_start_ms),
            active_ms = report.map(|r| r.active_ms),
            end_reason = report.map(|r| r.end_reason.as_str()),
            rss_peak_kb = report.map(|r| r.rss_peak_kb),
            tunnel_bytes_in = life.wire.map(|b| b.received),
            tunnel_bytes_out = life.wire.map(|b| b.sent),
            error,
            "invocation"
        );
    }

    /// A worker failed with requests in flight, as an out-of-memory kill or a crash does.
    /// The routes it was serving then run alone until they show what they really cost.
    pub fn worker_died(&self, worker: &Worker) {
        if worker.died.swap(true, Ordering::Relaxed) {
            return;
        }
        let routes = worker.load.lock().unwrap().routes.clone();
        warn!(worker = %worker.id, ?routes, "worker died with requests in flight");
        if let Some(life) = self.state.lock().unwrap().lives.get_mut(&worker.id) {
            life.died = true;
        }
        self.remove(&worker.id);
        if worker.limit_kb == 0 {
            return;
        }
        let budget = worker.budget_kb();
        let mut state = self.state.lock().unwrap();
        for route in routes {
            if state.routes.len() < MAX_ROUTES || state.routes.contains_key(&route) {
                let cost = state.routes.entry(route).or_insert(budget);
                *cost = (*cost).max(budget);
            }
        }
    }

    /// Record the memory a worker reported with a response: its RSS when the request arrived
    /// and when the response started, and for a request that arrived alone, the peak in
    /// between, in KiB.
    /// Returns the request's own memory cost, if it ran alone.
    pub fn observe(&self, lease: &Lease, rss_start_kb: Option<u64>, rss_kb: Option<u64>, peak_kb: Option<u64>) -> Option<u64> {
        let worker = &lease.worker;
        let rss_kb = rss_kb?;
        let reserved = worker.load.lock().unwrap().reserved_kb;
        worker.base_kb.store(rss_kb.saturating_sub(reserved), Ordering::Relaxed);
        // Only a request that ran alone shows its own cost.
        let start_kb = rss_start_kb.filter(|_| lease.admitted_alone && worker.inflight.count() == 1)?;
        let sample = peak_kb.unwrap_or(rss_kb).max(rss_kb).saturating_sub(start_kb).max(MIN_ROUTE_KB);
        let mut state = self.state.lock().unwrap();
        let full = state.routes.len() >= MAX_ROUTES;
        let (before, after) = match state.routes.get_mut(&lease.route) {
            // Rise at once and fall slowly, so a few light requests don't hide a heavy route.
            Some(cost) => {
                let before = *cost;
                *cost = if sample >= before { sample } else { before - (before - sample) / 20 };
                (Some(before), *cost)
            }
            None if !full => {
                state.routes.insert(lease.route.clone(), sample);
                (None, sample)
            }
            None => return Some(sample),
        };
        if before.is_none_or(|b| b.abs_diff(after) * 10 > b) {
            info!(route = %lease.route, sample_mb = sample / 1024, cost_mb = after / 1024, "route memory cost");
        }
        Some(sample)
    }

    /// Record the CPU a request used and how long it took, as the worker measured them for one
    /// that had the worker to itself, in microseconds.
    pub fn observe_cpu(&self, lease: &Lease, cpu_us: Option<u64>, busy_us: Option<u64>) {
        let (Some(cpu_us), Some(busy_us)) = (cpu_us, busy_us.filter(|&b| b > 0)) else { return };
        let sample = (cpu_us * 1000 / busy_us).clamp(MIN_ROUTE_CPU, 1000);
        let mut state = self.state.lock().unwrap();
        let full = state.route_cpu.len() >= MAX_ROUTES;
        let (before, after) = match state.route_cpu.get_mut(&lease.route) {
            // Rise halfway at once and fall slowly: an underestimate costs latency for every
            // request packed with it, an overestimate only a little packing.
            Some(share) => {
                let before = *share;
                *share = if sample >= before { before + (sample - before).div_ceil(2) } else { before - (before - sample) / 20 };
                (Some(before), *share)
            }
            None if !full => {
                state.route_cpu.insert(lease.route.clone(), sample);
                (None, sample)
            }
            None => return,
        };
        if before.is_none_or(|b| b.abs_diff(after) * 5 > b.max(10)) {
            info!(route = %lease.route, sample_pct = sample as f64 / 10.0, share_pct = after as f64 / 10.0, "route cpu share");
        }
    }

    /// Whether to sample this request's CPU use over time: `profile_pct` of those admitted alone.
    pub fn profile(&self, lease: &Lease) -> bool {
        let pct = u64::from(self.cfg.profile_pct);
        if pct == 0 || !lease.admitted_alone {
            return false;
        }
        let n = self.alone_admitted.fetch_add(1, Ordering::Relaxed);
        n * pct / 100 != (n + 1) * pct / 100
    }

    /// Each request earns part of a retry. Retrying a request the worker may have
    /// acted on spends a whole one, so a crashing app can't multiply its own load.
    pub fn earn_retry(&self) {
        let mut state = self.state.lock().unwrap();
        state.retry_tokens = (state.retry_tokens + RETRY_EARN).min(RETRY_MAX);
    }

    pub fn take_retry(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.retry_tokens < 1.0 {
            return false;
        }
        state.retry_tokens -= 1.0;
        true
    }

    /// Retire workers that are older than `max_age`, or idle for `idle` and not
    /// needed as a spare. Also starts spawns that waited out a back-off.
    pub async fn reap(self: Arc<Self>) {
        let mut tick = tokio::time::interval(REAP_INTERVAL);
        let spare_max_idle = self.cfg.idle + WORKER_IDLE_GRACE - RETIRE_BEFORE_WORKER;
        // With a short `idle`, a worker retires when its `idle` runs out, not at the next tick:
        // every slot that frees up wakes the reaper, and so does the next worker's deadline.
        // Otherwise each invocation would be billed up to a tick past it, a few percent of the
        // cost when workers run one request each.
        let precise = self.cfg.idle < PRECISE_IDLE;
        let changed = self.changed.clone();
        let mut notified = Box::pin(changed.notified());
        notified.as_mut().enable();
        let deadline = tokio::time::sleep(Duration::ZERO);
        tokio::pin!(deadline);
        let mut next_deadline: Option<Instant> = None;
        loop {
            // Rounds that a change woke only retire workers. Spawning is left to the requests,
            // which scale after each try for a slot, and to the tick (back-offs, headroom):
            // the change that wakes this round, a worker connecting or a slot freeing, also wakes
            // the request waiting for it, and a round that got the lock first would see that
            // request still waiting and start another worker for it.
            let by_tick = if precise {
                if let Some(at) = next_deadline {
                    deadline.as_mut().reset(at.into());
                }
                tokio::select! {
                    _ = tick.tick() => true,
                    _ = &mut notified => {
                        // Registered before this round looks at the workers, so a slot that frees
                        // up meanwhile wakes the next one.
                        notified.set(changed.notified());
                        notified.as_mut().enable();
                        false
                    }
                    _ = &mut deadline, if next_deadline.is_some() => false,
                }
            } else {
                tick.tick().await;
                true
            };
            let now = Instant::now();
            let closed = self.closed.load(Ordering::Relaxed);
            if by_tick {
                // Reports what the deployment's logs dropped, and forgets requests that have ended.
                self.log_limiter.tick();
            }
            let mut state = self.state.lock().unwrap();
            // Spares ride out dips in demand, but once there has been none for `idle`,
            // the pool drops to zero: a frozen environment costs nothing and thaws in
            // tens of milliseconds, while a hot spare is billed every second. Demand now
            // counts too: this runs every 50 ms, so with a shorter `idle` the time since
            // the last demand would never be within it, and spares (and a new worker whose
            // waiting request hasn't taken it yet) would be retired as soon as they connect.
            let peak = if demand(&state) > 0 || now - state.last_demand < self.cfg.idle {
                state.peak.get()
            } else {
                0
            };
            // A worker with nothing in flight is kept while requests wait: one of them is about to
            // take it, since its slot just freed or it just connected, and retiring it would send
            // that request to a new invocation. So it is while a request is expected that a TLS
            // handshake started a worker for (`start_early`).
            state.expecting.retain(|_, until| *until > now);
            let wanted = state.waiting > 0 || !state.expecting.is_empty();
            // Retiring an idle worker doesn't change it, so it's worked out once per round.
            let per_worker = self.per_worker(&state);
            next_deadline = None;
            let mut i = 0;
            while i < state.workers.len() {
                let w = state.workers[i].clone();
                // None while requests are in flight: a busy worker is never idle, even with
                // `idle` at zero.
                let idle_for = {
                    let mut idle_since = w.idle_since.lock().unwrap();
                    if w.inflight.count() == 0 {
                        Some(now - *idle_since.get_or_insert(now))
                    } else {
                        *idle_since = None;
                        None
                    }
                };
                let age = now - w.connected_at;
                // A spare that has been idle this long is swapped for a fresh one.
                let spare_needed = idle_for.is_some_and(|d| d < spare_max_idle)
                    && !self.aging(&w, now)
                    && self.short_of_headroom(&state, now, 1, peak, per_worker);
                // Work the app does behind an answer (`waitUntil`, Next's `after()`) keeps its
                // worker: let go, the work would lose its log stream, and the next request would
                // find the environment busy and start another, cold. It gets the deployment's
                // maxDuration from the last answer's end, as a request gets from its start.
                let working = w.background.running() > 0 && idle_for.is_some_and(|d| d < self.cfg.max_duration);
                let idle = idle_for.is_some_and(|d| d >= self.cfg.idle) && !wanted && !working;
                let unneeded = closed && w.inflight.count() == 0 && !wanted;
                if age >= self.cfg.max_age || (idle && !spare_needed) || unneeded {
                    let idle_ms = idle_for.unwrap_or_default().as_millis() as u64;
                    let background_ms = w.background.lasted(now).map(|d| d.as_millis() as u64);
                    info!(worker = %w.id, idle_ms, background_ms, age_s = age.as_secs(), "retiring worker");
                    let reason = if age >= self.cfg.max_age {
                        "max_age"
                    } else if unneeded {
                        "closing"
                    } else {
                        "idle"
                    };
                    if let Some(life) = state.lives.get_mut(&w.id) {
                        (life.retired, life.idle_ms, life.background_ms) = (Some(reason), idle_ms, background_ms);
                    }
                    state.workers.swap_remove(i);
                } else {
                    // When this one's `idle` runs out, if nothing arrives before.
                    if let Some(d) = idle_for.filter(|d| *d < self.cfg.idle) {
                        let at = now + (self.cfg.idle - d);
                        next_deadline = Some(next_deadline.map_or(at, |n| n.min(at)));
                    }
                    i += 1;
                }
            }
            if by_tick {
                self.scale(&mut state, now);
            }
            if closed && state.workers.is_empty() && state.starting.is_empty() && state.waiting == 0 {
                info!(pool = %self.cfg.name, draining = state.live, "pool closed");
                return;
            }
        }
    }
}

/// When a spawn that hasn't connected is taken for a cold start, given how long the pool's thaws
/// take to connect lately.
fn hedge_after(thaw_connect: Option<Duration>) -> Duration {
    thaw_connect.map_or(HEDGE_AFTER, |thaw| (thaw * HEDGE_THAWS).max(HEDGE_AFTER))
}

/// The moving average of thaws' connect times, with one more: an eighth of the way to it, so one
/// slow thaw doesn't move it far. The first is taken as it is.
fn learn_thaw(average: Option<Duration>, connect: Duration) -> Duration {
    match average {
        None => connect,
        Some(average) => (average * 7 + connect) / 8,
    }
}

/// Requests in flight plus those waiting for a slot.
fn demand(state: &State) -> usize {
    state.workers.iter().map(|w| w.inflight.count()).sum::<usize>() + state.waiting
}

/// A ready environment (a quick start, kept as `thaw` in the logs) connects within tens of
/// milliseconds (all 2,117 of one API's in an hour within 53 ms, 175 ms across regions); a cold
/// one takes 250 ms or more. Used only for runtimes that don't say how many invocations their
/// environment has served.
const COLD_GUESS_MS: u64 = 250;

fn start_kind(env_invocations: Option<u64>, connect_ms: Option<u64>) -> &'static str {
    let cold = match (env_invocations, connect_ms) {
        (Some(n), _) => n <= 1,
        (None, Some(ms)) => ms >= COLD_GUESS_MS,
        (None, None) => false,
    };
    if cold { "cold" } else { "thaw" }
}

fn lease(worker: Arc<Worker>, route: &str, cost_kb: u64, cpu: u64, exclusive: bool) -> Lease {
    let admitted_alone = worker.inflight.count() == 0;
    if let Some(used) = &worker.early_used {
        used.store(true, Ordering::Relaxed);
    }
    let guard = worker.inflight.enter();
    if exclusive {
        worker.running_alone.store(true, Ordering::Relaxed);
    }
    // The reaper may never see a request that starts and ends between two
    // ticks, so restart the idle clock here.
    *worker.idle_since.lock().unwrap() = None;
    let mut load = worker.load.lock().unwrap();
    load.reserved_kb += cost_kb;
    load.reserved_cpu += cpu;
    load.routes.push(route.to_owned());
    drop(load);
    Lease {
        worker,
        leased_at: Instant::now(),
        start: "warm",
        route: route.to_owned(),
        cost_kb,
        cpu,
        admitted_alone,
        exclusive,
        _guard: guard,
    }
}

/// Seconds to suggest in `Retry-After`: until the spawn back-off ends, at least one.
fn retry_after(state: &State) -> Duration {
    let backoff = state.next_spawn.map_or(Duration::ZERO, |at| at.saturating_duration_since(Instant::now()));
    backoff.max(Duration::from_secs(1))
}

/// A request's place among those waiting for a slot.
struct Waiting<'a> {
    pool: &'a Pool,
    counted: bool,
    alone: bool,
    /// A retry that avoids workers it failed on.
    avoiding: bool,
    /// The CPU share it added to `State::waiting_cpu`.
    cpu: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.counted {
            let mut state = self.pool.state.lock().unwrap();
            state.waiting -= 1;
            state.waiting_alone -= usize::from(self.alone);
            state.waiting_avoiding -= usize::from(self.avoiding);
            state.waiting_cpu -= self.cpu;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::convert::Infallible;
    use std::path::PathBuf;

    use bytes::Bytes;
    use http_body_util::Empty;
    use hyper::Response;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    use super::*;

    /// One request per worker, retired as soon as it ends, no spares: concurrent compute off (`"fluid": false`).
    pub(crate) fn single() -> PoolConfig {
        PoolConfig {
            name: "single01".into(),
            max_inflight: 1,
            idle: Duration::ZERO,
            idle_auto: false,
            max_age: Duration::from_secs(780),
            headroom_pct: 0,
            tunnel_addr: "127.0.0.1:1".into(),
            cert_sha256: String::new(),
            acquire_timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(10),
            cpu_target_pct: 0,
            profile_pct: 0,
            max_workers: 20,
            budget: WorkerBudget::new(100),
            max_duration: Duration::from_secs(300),
            project: "p".into(),
            region: String::new(),
            memory_mb: 0,
            relay: None,
            environment: String::new(),
            runtime: String::new(),
            entry: String::new(),
            egress: Vec::new(),
        }
    }

    /// Workers that start and never connect, so the test connects its own in their place.
    pub(crate) fn hanging_spawner() -> (Spawner, PathBuf) {
        let dir = std::env::temp_dir().join(format!("forkrail-pool-{}", random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("hang.sh");
        std::fs::write(&bin, "#!/bin/sh\nexec sleep 5\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (Spawner::Local { bin, env: Vec::new(), isolated: false }, dir)
    }

    /// A tunnel to a fake worker that answers every request at once.
    async fn tunnel() -> SendRequest<ProxyBody> {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let service = hyper::service::service_fn(|_| async { Ok::<_, Infallible>(Response::new(Empty::<Bytes>::new())) });
            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(server_io), service)
                .await;
        });
        let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(client_io))
            .await
            .unwrap();
        tokio::spawn(conn);
        sender
    }

    /// The spawned workers' ids and tokens, as the tunnel would see them in their prefaces.
    pub(crate) fn spawned(pool: &Pool) -> Vec<Preface> {
        let state = pool.state.lock().unwrap();
        let mut prefaces: Vec<Preface> = state
            .starting
            .iter()
            .map(|(id, s)| Preface { worker_id: id.clone(), token: s.token.clone(), limit_kb: 0, rss_kb: 0, logs: false, env_invocations: None, cache: false })
            .collect();
        prefaces.sort_by(|a, b| a.worker_id.cmp(&b.worker_id));
        prefaces
    }

    pub(crate) fn counts(pool: &Pool) -> (usize, usize, usize, usize) {
        let state = pool.state.lock().unwrap();
        (state.live, state.starting.len(), state.workers.len(), state.waiting)
    }

    pub(crate) async fn settle() {
        // Lets the woken tasks run; well under the reaper's tick.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    /// A connecting worker must present its own slot's token: not another slot's, not one of the
    /// right length that's wrong, not one already used.
    #[tokio::test]
    async fn a_worker_joins_only_with_its_own_slots_token() {
        let (spawner, _dir) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        let (a, b) = (pool.clone(), pool.clone());
        let _first = tokio::spawn(async move { a.acquire("/a", &[], false).await });
        let _second = tokio::spawn(async move { b.acquire("/a", &[], false).await });
        settle().await;
        let prefaces = spawned(&pool);
        assert_eq!(prefaces.len(), 2);
        let (p, q) = (&prefaces[0], &prefaces[1]);
        assert!(pool.claim(&p.worker_id, &q.token).is_none(), "another slot's token");
        let mut wrong = p.token.clone().into_bytes();
        wrong[0] = if wrong[0] == b'0' { b'1' } else { b'0' };
        assert!(pool.claim(&p.worker_id, std::str::from_utf8(&wrong).unwrap()).is_none(), "a wrong token of the right length");
        assert!(pool.claim(&p.worker_id, "").is_none());
        assert!(pool.claim("someone-else.0123456789abcdef", &p.token).is_none(), "a slot that doesn't exist");
        assert!(pool.claim(&p.worker_id, &p.token).is_some());
        assert!(pool.claim(&p.worker_id, &p.token).is_none(), "once only");
    }

    /// Work an app does behind an answer keeps its worker: for the next request, and until the
    /// work ends or has had the deployment's maxDuration.
    #[tokio::test]
    async fn a_worker_with_background_work_is_kept_until_it_ends_or_max_duration() {
        let (spawner, dir) = hanging_spawner();
        let pool = Pool::new(PoolConfig { max_duration: Duration::from_millis(400), ..single() }, spawner);
        tokio::spawn(pool.clone().reap());
        let connect = |pool: Arc<Pool>| async move {
            let waiter = pool.clone();
            let lease = tokio::spawn(async move { waiter.acquire("/a", &[], false).await });
            settle().await;
            let preface = spawned(&pool).pop().expect("a worker was started");
            assert!(pool.claim(&preface.worker_id, &preface.token).is_some());
            pool.register(&preface, tunnel().await, None);
            lease.await.unwrap().unwrap()
        };

        // Its app says it has work running (what the log stream does), and the request ends.
        let lease = connect(pool.clone()).await;
        let worker = lease.worker.clone();
        worker.background.set(1);
        drop(lease);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(counts(&pool), (1, 0, 1, 0), "idle is zero, yet the worker is kept for its work");
        // The next request takes that worker: no second invocation.
        let again = pool.acquire("/a", &[], false).await.unwrap();
        assert_eq!((again.worker.id.as_str(), again.start), (worker.id.as_str(), "warm"));
        assert_eq!(counts(&pool), (1, 0, 1, 0));
        drop(again);
        // The work ends: retired at once, as an idle worker is.
        worker.background.set(0);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(counts(&pool).2, 0, "let go once its work ended");
        assert!(worker.background.lasted(Instant::now()).is_some_and(|d| d >= Duration::from_millis(150)));

        // Work that never ends is given maxDuration from its request's end, no more.
        let lease = connect(pool.clone()).await;
        let stuck = lease.worker.clone();
        stuck.background.set(3);
        drop(lease);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(counts(&pool).2, 1, "still within maxDuration");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(counts(&pool).2, 0, "let go at maxDuration");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn single_mode_starts_one_worker_per_request_and_retires_it_at_once() {
        let (spawner, dir) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        tokio::spawn(pool.clone().reap());

        // Two requests and no workers: each waits and starts one.
        let (a, b) = (pool.clone(), pool.clone());
        let first = tokio::spawn(async move { a.acquire("/a", &[], false).await });
        let second = tokio::spawn(async move { b.acquire("/a", &[], false).await });
        settle().await;
        assert_eq!(counts(&pool), (2, 2, 0, 2));

        // Both connect. A claimed worker still counts as starting until it registers, and one
        // that registered still covers its waiting request until that request takes it: neither
        // starts a third worker.
        let prefaces = spawned(&pool);
        let senders = [tunnel().await, tunnel().await];
        for p in &prefaces {
            assert!(pool.claim(&p.worker_id, &p.token).is_some());
            assert!(pool.claim(&p.worker_id, &p.token).is_none(), "a token works once");
        }
        assert_eq!(counts(&pool), (2, 2, 0, 2));
        for (p, sender) in prefaces.iter().zip(senders) {
            pool.register(p, sender, None);
        }
        let leases = [first.await.unwrap().unwrap(), second.await.unwrap().unwrap()];
        assert_ne!(leases[0].worker.id, leases[1].worker.id);
        settle().await;
        assert_eq!(counts(&pool), (2, 0, 2, 0), "no third worker");

        // A worker retires as soon as its request ends, not at the next tick.
        let [one, two] = leases;
        drop(one);
        settle().await;
        assert_eq!(counts(&pool).2, 1);
        drop(two);
        settle().await;
        assert_eq!(counts(&pool).2, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// What crossed a worker's tunnel is read from the kernel once, when the tunnel ends, and its
    /// egress connections' bytes are added to it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn a_worker_records_what_crossed_its_tunnel() {
        use crate::wire::tests::{DOWN, UP, close_to, pair};
        let (spawner, dir) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        let p = pool.clone();
        let waiting = tokio::spawn(async move { p.acquire("/a", &[], false).await });
        settle().await;
        let preface = spawned(&pool).remove(0);
        pool.claim(&preface.worker_id, &preface.token).unwrap();
        // A connection that has carried UP bytes out and DOWN bytes in stands for the tunnel's.
        let (ours, theirs) = pair();
        pool.register(&preface, tunnel().await, wire::hold(&ours));
        let lease = waiting.await.unwrap().unwrap();
        let wire = |pool: &Pool| pool.state.lock().unwrap().lives[&preface.worker_id].wire;
        assert_eq!(wire(&pool), None, "nothing is read while the tunnel is up");

        // The tunnel ends: hyper drops the stream, and the handle still has the counts.
        drop(ours);
        pool.tunnel_closed(&preface.worker_id);
        let read = wire(&pool).expect("read when the tunnel ended");
        assert!(close_to(read, DOWN, UP), "{read:?}");
        assert!(pool.state.lock().unwrap().lives[&preface.worker_id].socket.is_none(), "the handle is closed");
        pool.tunnel_closed(&preface.worker_id);
        assert_eq!(wire(&pool), Some(read), "read once");

        pool.egress_closed(&preface.worker_id, Some(wire::Bytes { received: 10, sent: 20 }));
        pool.egress_closed(&preface.worker_id, None);
        assert_eq!(wire(&pool), Some(wire::Bytes { received: read.received + 10, sent: read.sent + 20 }));

        // After the invocation's line is written nothing is kept, and a late end is ignored.
        drop(lease);
        pool.ended(&preface.worker_id, 0.1, Ok(None));
        pool.egress_closed(&preface.worker_id, Some(wire::Bytes { received: 1, sent: 1 }));
        pool.tunnel_closed(&preface.worker_id);
        assert!(pool.state.lock().unwrap().lives.is_empty());
        drop(theirs);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A worker that vanished: its invocation ends with the tunnel still up, and the counts are
    /// read then.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_tunnel_still_up_is_counted_when_the_invocation_ends() {
        use crate::wire::tests::{DOWN, UP, close_to, pair};
        let (ours, _theirs) = pair();
        let mut life = Life { socket: wire::hold(&ours), ..Life::default() };
        assert!(life.count_tunnel().is_some());
        assert!(close_to(life.wire.unwrap(), DOWN, UP), "{:?}", life.wire);
        assert!(life.count_tunnel().is_none());
    }

    fn hedges(pool: &Pool) -> usize {
        pool.state.lock().unwrap().starting.values().filter(|s| s.hedged).count()
    }

    #[test]
    fn second_invokes_wait_for_twice_a_thaw() {
        let ms = Duration::from_millis;
        // Nothing learned, or thaws in this region: the usual 100 ms.
        assert_eq!(hedge_after(None), ms(100));
        assert_eq!(hedge_after(Some(ms(19))), ms(100));
        // Workers across the country connect in about 194 ms: twice that before taking one for cold.
        assert_eq!(hedge_after(Some(ms(194))), ms(388));
        // The first thaw is taken as it is, then each moves it an eighth of the way.
        let mut average = learn_thaw(None, ms(194));
        assert_eq!(average, ms(194));
        average = learn_thaw(Some(average), ms(1994));
        assert_eq!(average, ms(419));
        for _ in 0..40 {
            average = learn_thaw(Some(average), ms(20));
        }
        assert!(average < ms(25), "{average:?}");
    }

    #[test]
    fn a_relay_lease_is_the_link_unless_it_failed_there() {
        let pool = Pool::new(PoolConfig { relay: Some(crate::relay::slot("xx-none-1")), ..single() }, Spawner::Local { bin: PathBuf::new(), env: Vec::new(), isolated: false });
        // No link up to that region: the pool's own workers.
        assert!(pool.relay_lease("/", &[]).is_none());
        assert!(!pool.relayed());
    }

    #[tokio::test]
    async fn a_request_stuck_behind_a_cold_start_gets_one_second_invoke() {
        let (spawner, dir) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        // Three invocations were open a moment ago, so Lambda probably has frozen environments.
        pool.state.lock().unwrap().envs.record(3, Instant::now());
        tokio::spawn(pool.clone().reap());
        let p = pool.clone();
        let waiting = tokio::spawn(async move { p.acquire("/a", &[], false).await });
        // A thaw would have connected by now; this one hasn't, but it isn't stuck yet.
        tokio::time::sleep(HEDGE_AFTER / 2).await;
        assert_eq!(counts(&pool), (1, 1, 0, 1));
        // Past HEDGE_AFTER and a reaper tick: one more invoke, and both count from now on.
        tokio::time::sleep(HEDGE_AFTER + REAP_INTERVAL * 2).await;
        assert_eq!(counts(&pool), (2, 2, 0, 1));
        assert_eq!(hedges(&pool), 2);
        // The second invoke is slow too: no third.
        tokio::time::sleep(HEDGE_AFTER * 3).await;
        assert_eq!(counts(&pool).0, 2);
        // The second connects first, and the request takes it.
        let second = spawned(&pool).into_iter().max_by_key(|p| pool.state.lock().unwrap().starting[&p.worker_id].since).unwrap();
        pool.claim(&second.worker_id, &second.token).unwrap();
        pool.register(&second, tunnel().await, None);
        assert_eq!(waiting.await.unwrap().unwrap().worker.id, second.worker_id);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_short_idle_keeps_a_worker_for_the_next_request_then_retires_it_on_time() {
        let (spawner, dir) = hanging_spawner();
        let idle = Duration::from_millis(30);
        let pool = Pool::new(PoolConfig { idle, ..single() }, spawner);
        tokio::spawn(pool.clone().reap());
        let p = pool.clone();
        let first = tokio::spawn(async move { p.acquire("/a", &[], false).await });
        settle().await;
        let preface = spawned(&pool).remove(0);
        pool.claim(&preface.worker_id, &preface.token).unwrap();
        pool.register(&preface, tunnel().await, None);
        drop(first.await.unwrap().unwrap());
        // Kept: the next request within `idle` takes it, with no new invocation.
        settle().await;
        assert_eq!(counts(&pool), (1, 0, 1, 0));
        let again = pool.acquire("/a", &[], false).await.unwrap();
        assert_eq!(again.worker.id, preface.worker_id);
        assert_eq!(counts(&pool).0, 1);
        drop(again);
        // Retired once `idle` runs out, well before a tick would come round again.
        tokio::time::sleep(idle / 2).await;
        assert_eq!(counts(&pool).2, 1);
        tokio::time::sleep(idle / 2 + Duration::from_millis(8)).await;
        assert_eq!(counts(&pool).2, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn no_second_invoke_when_no_environment_is_frozen() {
        let (spawner, dir) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        tokio::spawn(pool.clone().reap());
        let p = pool.clone();
        let _waiting = tokio::spawn(async move { p.acquire("/a", &[], false).await });
        // Nothing was open before, so another invoke would be a cold start too.
        tokio::time::sleep(HEDGE_AFTER * 3).await;
        assert_eq!(counts(&pool), (1, 1, 0, 1));
        assert_eq!(hedges(&pool), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn peaks_age_out() {
        let start = Instant::now();
        let mut peak = Peak::new(Duration::from_secs(10));
        peak.window_start = start;
        peak.record(5, start);
        peak.record(1, start + Duration::from_secs(12));
        assert_eq!(peak.get(), 5, "the previous window still counts");
        peak.record(1, start + Duration::from_secs(25));
        assert_eq!(peak.get(), 1);
        // A long quiet spell forgets the old peak altogether.
        peak.record(9, start + Duration::from_secs(30));
        peak.record(2, start + Duration::from_secs(60));
        assert_eq!(peak.get(), 2);
    }

    fn early_life(pool: &Pool, id: &str) -> Option<(bool, Option<&'static str>)> {
        let state = pool.state.lock().unwrap();
        let life = state.lives.get(id)?;
        Some((life.early.as_ref()?.load(Ordering::Relaxed), life.retired))
    }

    /// A TLS handshake starts a worker only where the request after it would wait for a cold start,
    /// one at a time, and the worker is kept for that request.
    #[tokio::test]
    async fn a_handshake_starts_a_worker_only_for_a_request_that_would_start_cold() {
        let (spawner, dir) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        tokio::spawn(pool.clone().reap());
        let hold = Duration::from_millis(500);
        let expected = pool.start_early(0, hold).expect("nothing open, nothing frozen: a cold start ahead");
        assert_eq!(counts(&pool), (1, 1, 0, 0));
        // Another handshake meanwhile: that start will serve whichever request comes first.
        assert!(pool.start_early(0, hold).is_none());
        // It connects before the request arrives (a far visitor), and is kept for it although
        // `idle` is zero.
        let preface = spawned(&pool).remove(0);
        pool.claim(&preface.worker_id, &preface.token).unwrap();
        pool.register(&preface, tunnel().await, None);
        tokio::time::sleep(REAP_INTERVAL * 3).await;
        assert_eq!(counts(&pool), (1, 0, 1, 0));
        // The request takes it: no second invocation.
        let lease = pool.acquire("/", &[], false).await.unwrap();
        assert_eq!((lease.worker.id.as_str(), counts(&pool).0), (preface.worker_id.as_str(), 1));
        // Its answer is out and the connection's first request done: retired as usual.
        drop(lease);
        drop(expected);
        settle().await;
        assert_eq!(counts(&pool).2, 0);
        assert_eq!(early_life(&pool, &preface.worker_id), Some((true, Some("idle"))));
        // Its invocation returns (the tunnel closed). Its environment is probably frozen now: the
        // next request would thaw it, so no start.
        pool.ended(&preface.worker_id, 1.0, Ok(None));
        assert!(pool.start_early(0, hold).is_none());

        // Nor where keep-warm keeps one, or a worker has a free slot, or a request is already
        // waiting for a start (the next request would need one more, cold: that one may start).
        let (spawner, dir2) = hanging_spawner();
        let kept = Pool::new(single(), spawner);
        assert!(kept.start_early(1, hold).is_none());
        let (spawner, dir3) = hanging_spawner();
        let roomy = Pool::new(PoolConfig { max_inflight: 4, ..single() }, spawner);
        let r = roomy.clone();
        let first = tokio::spawn(async move { r.acquire("/", &[], false).await });
        settle().await;
        let preface = spawned(&roomy).remove(0);
        roomy.claim(&preface.worker_id, &preface.token).unwrap();
        roomy.register(&preface, tunnel().await, None);
        let lease = first.await.unwrap().unwrap();
        assert!(roomy.start_early(0, hold).is_none(), "3 free slots on the worker");
        drop(lease);
        let (spawner, dir4) = hanging_spawner();
        let busy = Pool::new(single(), spawner);
        let b = busy.clone();
        let _waiting = tokio::spawn(async move { b.acquire("/", &[], false).await });
        settle().await;
        assert_eq!(counts(&busy), (1, 1, 0, 1));
        assert!(busy.start_early(0, hold).is_some(), "the start coming is the waiting request's");
        assert_eq!(counts(&busy), (2, 2, 0, 1));
        for d in [dir, dir2, dir3, dir4] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    /// A start nobody used: kept for its hold, then retired by the idle rule, and said so.
    #[tokio::test]
    async fn an_unused_handshake_start_is_let_go_after_its_hold() {
        let (spawner, dir) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        tokio::spawn(pool.clone().reap());
        let hold = Duration::from_millis(200);
        let _expected = pool.start_early(0, hold).unwrap();
        let preface = spawned(&pool).remove(0);
        pool.claim(&preface.worker_id, &preface.token).unwrap();
        pool.register(&preface, tunnel().await, None);
        tokio::time::sleep(hold / 2).await;
        assert_eq!(counts(&pool).2, 1, "held for the request");
        tokio::time::sleep(hold / 2 + REAP_INTERVAL * 2).await;
        assert_eq!(counts(&pool).2, 0, "let go when the hold ran out");
        assert_eq!(early_life(&pool, &preface.worker_id), Some((false, Some("idle"))));
        // A connection that ends first (its first request needed no worker, or it closed) lets
        // go at once.
        let (spawner, dir2) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        tokio::spawn(pool.clone().reap());
        let expected = pool.start_early(0, Duration::from_secs(5)).unwrap();
        let preface = spawned(&pool).remove(0);
        pool.claim(&preface.worker_id, &preface.token).unwrap();
        pool.register(&preface, tunnel().await, None);
        settle().await;
        assert_eq!(counts(&pool).2, 1);
        drop(expected);
        settle().await;
        assert_eq!(counts(&pool).2, 0);
        for d in [dir, dir2] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    /// What keep-warm reads: the busiest moment since it last asked, and how long ago k
    /// invocations were last open at once.
    #[tokio::test]
    async fn activity_says_how_many_were_open_at_once_and_when() {
        let (spawner, dir) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        let a = pool.activity();
        assert_eq!((a.busiest, a.at_least.len(), a.since_request), (0, 0, None));
        // A burst of three at once.
        let waiting: Vec<_> = (0..3)
            .map(|_| {
                let p = pool.clone();
                tokio::spawn(async move { p.acquire("/", &[], false).await })
            })
            .collect();
        settle().await;
        let a = pool.activity();
        assert_eq!((a.busiest, a.at_least.clone()), (3, vec![Duration::ZERO; 3]));
        assert!(a.since_request.is_some());
        // One invocation ends: three were open until now, two still are.
        let ids: Vec<String> = spawned(&pool).into_iter().map(|p| p.worker_id).collect();
        pool.ended(&ids[0], 1.0, Ok(None));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let a = pool.activity();
        assert_eq!(a.busiest, 3, "the busiest since the last time it was asked: three, until one ended");
        assert_eq!(&a.at_least[..2], [Duration::ZERO; 2]);
        assert!(a.at_least[2] >= Duration::from_millis(20));
        assert_eq!(pool.activity().busiest, 2, "and since then, two");
        // The requests give up, and the other two invocations end: none open now.
        for w in &waiting {
            w.abort();
        }
        settle().await;
        for id in &ids[1..] {
            pool.ended(id, 1.0, Ok(None));
        }
        let a = pool.activity();
        assert_eq!(a.busiest, 2, "two were open until a moment ago");
        assert!(a.at_least.len() == 3 && a.at_least.iter().all(|ago| *ago > Duration::ZERO));
        assert_eq!(pool.activity().busiest, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_idle_worker_is_kept_while_a_request_waits_for_it() {
        let (spawner, dir) = hanging_spawner();
        let pool = Pool::new(single(), spawner);
        tokio::spawn(pool.clone().reap());
        let p = pool.clone();
        let first = tokio::spawn(async move { p.acquire("/a", &[], false).await });
        settle().await;
        let preface = spawned(&pool).remove(0);
        let lease = {
            pool.claim(&preface.worker_id, &preface.token).unwrap();
            pool.register(&preface, tunnel().await, None);
            first.await.unwrap().unwrap()
        };
        // A second request waits behind the first and starts a worker of its own.
        let p = pool.clone();
        let second = tokio::spawn(async move { p.acquire("/b", &[], false).await });
        settle().await;
        assert_eq!(counts(&pool), (2, 1, 1, 1));
        // The first ends before the new worker connects: the waiting request takes its worker,
        // which the reaper, woken by the same change, mustn't retire first.
        let id = lease.worker.id.clone();
        drop(lease);
        let next = second.await.unwrap().unwrap();
        assert_eq!(next.worker.id, id);
        drop(next);
        settle().await;
        assert_eq!(counts(&pool).2, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
