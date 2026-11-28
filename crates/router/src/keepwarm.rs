//! Keeping environments alive.
//!
//! Lambda drops an environment that has had no invocation for a few minutes (see `EVERY`), and
//! the next request then waits for a new one and for the app to start in it: a second or more
//! for a framework's server. So for every project's production deployment the router sends a
//! warm invocation (`forkrail_common::WarmRequest`) every `every`, which Lambda runs in an idle
//! environment: its idle clock starts again, the runtime answers at once (the app is up already),
//! no request reaches the app, nothing is logged as a visitor's. When Lambda has replaced the
//! environment meanwhile, the same invocation starts the new one, app and priming included, and
//! no visitor waits for that either.
//!
//! - How many: `projects.<name>.keep_warm` in the routing table, else `FORKRAIL_KEEP_WARM`
//!   (default 0, none: off unless a project turns it on), per region the deployment runs in. `auto` keeps as many as the
//!   deployment had invocations open at once at its busiest in the last day (at least 1, at most
//!   `KEEP_WARM_MAX`), so a burst that comes back finds its environments ready.
//! - All at once, every round: Lambda keeps only the environments it runs invocations in, and
//!   gives an invocation the environment it used last, so keep-alives one after another would
//!   keep one environment however many are sent. A round sends all of them together, each held
//!   open a moment, so each lands in an environment of its own. Real traffic counts in a round's
//!   place only when it had as many open at once within the interval (those were as many
//!   environments, each fresh); less than that, and the round goes out whole. A deployment with
//!   steady traffic gets none.
//! - Not for projects nobody uses: a deployment with no request through this router for
//!   `FORKRAIL_KEEP_WARM_UNUSED_S` (default 7 days; 0 = never) is paused until its next request
//!   or the next production deployment. A router's restart counts as a use (it can't know).
//! - Which router: the one in the version's region. A version in a region without a router is kept
//!   by every router that invokes it there (`routers.rs` says which regions have one).
//! - During a restart two routers run on one machine. Only the one holding the keeper's lock sends
//!   (a loopback port, `FORKRAIL_KEEP_WARM_LOCK`, default `127.0.0.1:7446`; `none` for no lock):
//!   the old one until it exits, then the new one, which starts with a round for every deployment,
//!   since it can't know when the old one's last was. The busiest hours of the last day go from
//!   one to the other through a file (`FORKRAIL_KEEP_WARM_STATE`), so `auto` survives restarts.
//! - What it costs: a warm invocation of a ready environment alone is billed as one request and
//!   3-5 ms (measured): at a round every 2 minutes about $0.011 a month per environment at 1 GB.
//!   Several together are billed `HOLD_MS` each.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use forkrail_common::WarmReport;
use forkrail_common::routes::{Deployment, KEEP_WARM_MAX, KeepWarm};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::pool::Activity;
use crate::registry::Registry;
use crate::spawner::{InvokeFailure, Spawner};

/// Time between a deployment's rounds unless `FORKRAIL_KEEP_WARM_EVERY_S` says. How long Lambda
/// keeps an idle environment isn't one number (measured 2026-11-17 over 2 h 20 min, both regions,
/// 640 to 1,769 MB, Node and Python; docs/data/n4): a version invoked now and then lost its
/// environments after anything from under 3 minutes to 5 min 30 s (never later), while
/// environments invoked every 1 to 2.5 minutes all stayed (158 of 158), 57 of 59 at 3 to 3.5
/// minutes, and rounds every 4 minutes lost 3 environments in 232. Two minutes is inside what
/// always held, and leaves a failed round its retry (`RETRY_AFTER`) and a restart its takeover
/// (`FIRST_WITHIN`).
pub const EVERY: Duration = Duration::from_secs(120);
/// How long without a request before a deployment is no longer kept, unless
/// `FORKRAIL_KEEP_WARM_UNUSED_S` says: a week, so a site with a visitor now and then stays ready
/// and a project made once and never opened stops costing. The author's number to set.
pub const UNUSED_AFTER: Duration = Duration::from_secs(7 * 86_400);
/// The keeper's lock unless `FORKRAIL_KEEP_WARM_LOCK` names another.
pub const DEFAULT_LOCK: &str = "127.0.0.1:7446";
/// Where the busiest hours are kept unless `FORKRAIL_KEEP_WARM_STATE` names another file.
pub const DEFAULT_STATE: &str = "/var/lib/forkrail/keepwarm/busiest.json";
/// How often the keeper looks at what's due.
const TICK: Duration = Duration::from_secs(1);
/// A deployment the keeper sees for the first time gets its first round within this long: at
/// start-up and after taking over from another router that's all of them, spread out.
const FIRST_WITHIN: Duration = Duration::from_secs(10);
/// How long warm invocations sent together stay open, so that they overlap (the CLI's warm-up
/// uses the same).
const HOLD_MS: u64 = 150;
/// A round that failed (Lambda had no room, the call didn't go through) is tried again this soon,
/// not a whole interval later, which would be past what Lambda keeps an idle environment.
const RETRY_AFTER: Duration = Duration::from_secs(20);
/// How often the keeper says what it did.
const SUMMARY_EVERY: Duration = Duration::from_secs(900);
/// How far back `auto` looks, in hours.
const DAY_HOURS: u64 = 24;
/// How often the busiest hours are written down.
const SAVE_EVERY: Duration = Duration::from_secs(60);

pub struct Settings {
    /// What applies to a project that doesn't say.
    pub keep: KeepWarm,
    /// Time between a deployment's rounds.
    pub every: Duration,
    /// The loopback address whose listener is the keeper's lock; None for no lock.
    pub lock: Option<String>,
    /// A deployment that has had no request through this router for this long is no longer kept,
    /// until its next request; zero keeps every one whatever its use.
    pub unused_after: Duration,
    /// The file the busiest hours are kept in across restarts; None keeps them in memory alone.
    pub state: Option<PathBuf>,
}

/// A deployment to keep environments alive for, as this router runs it.
pub struct Target {
    pub deployment: String,
    pub region: String,
    /// What its project asks for, or the router's default.
    pub keep: KeepWarm,
    /// What this router's invocations of it did lately (`Pool::activity`); nothing when it has
    /// had none since this router started, or since its entry changed.
    pub activity: Activity,
    pub entry: Deployment,
}

/// The most invocations open at once in each of the last `DAY_HOURS` hours that had any, by Unix
/// hour, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Busiest(Vec<(u64, u32)>);

impl Busiest {
    fn record(&mut self, hour: u64, busiest: usize) {
        if busiest > 0 {
            let busiest = busiest.min(u32::MAX as usize) as u32;
            match self.0.last_mut() {
                Some((h, b)) if *h == hour => *b = (*b).max(busiest),
                _ => self.0.push((hour, busiest)),
            }
        }
        self.0.retain(|(h, _)| h + DAY_HOURS > hour);
    }

    /// The busiest in the last day.
    fn day(&self, hour: u64) -> usize {
        self.0.iter().filter(|(h, _)| h + DAY_HOURS > hour).map(|(_, b)| *b as usize).max().unwrap_or(0)
    }
}

/// What the keeper remembers of a deployment it keeps.
struct Kept {
    /// When its next round is due.
    next: Instant,
    /// When it was last used by a request, as far as this router knows: at the latest when the
    /// keeper first saw it (a new production deployment, or this router's start).
    used: Instant,
    /// Not kept for now: nobody has asked for it in `Settings::unused_after`.
    paused: bool,
    busiest: Busiest,
    /// How many its last round kept, to say when `auto` changes it.
    count: usize,
}

type Key = (String, String);

/// What a round did, for the summary.
#[derive(Default)]
struct Tally {
    rounds: u64,
    invocations: u64,
    /// Environments a round found gone and started again.
    started: u64,
    failed: u64,
}

/// A round's end, for the keeper: how many environments it found ready (each a different one),
/// and whether to try again soon.
struct Done {
    key: Key,
    ready: usize,
    retry: bool,
}

/// When a deployment first seen at `now` gets its first round: spread by its name over `within`.
fn first_due(now: Instant, deployment: &str, region: &str, within: Duration) -> Instant {
    let mut hasher = DefaultHasher::new();
    (deployment, region).hash(&mut hasher);
    now + Duration::from_millis(hasher.finish() % (within.as_millis() as u64).max(1))
}

/// How many environments `keep` asks for, with `busiest` the deployment's last day at `hour`.
fn count(keep: KeepWarm, busiest: &Busiest, hour: u64) -> usize {
    match keep {
        KeepWarm::Count(n) => n as usize,
        KeepWarm::Auto => busiest.day(hour).clamp(1, KEEP_WARM_MAX as usize),
    }
}

/// What's due at `now` (`hour` is the Unix hour): each as (target, invocations to send, all at
/// once). Deployments no longer kept are forgotten and new ones get their first round soon,
/// starting from what `remembered` has of their busiest hours.
///
/// A deployment whose turn it is needs a round of as many invocations as it keeps environments,
/// unless real traffic had that many open at once within `every`: each was an environment of its
/// own whose idle clock started no earlier than then, so the next round is due `every` after.
/// Fewer than that, and the round goes out whole: Lambda gives an invocation the environment it
/// used last, so making up only the difference would keep the same few and lose the rest.
///
/// A deployment nobody has asked for in `unused_after` is paused: no rounds until its next
/// request (which this router sees in its pool) or until production moves to a new deployment.
fn due(kept: &mut HashMap<Key, Kept>, remembered: &HashMap<Key, Busiest>, targets: Vec<Target>, now: Instant, hour: u64, every: Duration, unused_after: Duration) -> Vec<(Target, usize)> {
    kept.retain(|(deployment, region), _| targets.iter().any(|t| t.deployment == *deployment && t.region == *region));
    let mut rounds = Vec::new();
    for target in targets {
        let key = (target.deployment.clone(), target.region.clone());
        let state = kept.entry(key.clone()).or_insert_with(|| Kept {
            next: first_due(now, &target.deployment, &target.region, FIRST_WITHIN.min(every)),
            used: now,
            paused: false,
            busiest: remembered.get(&key).cloned().unwrap_or_default(),
            count: 0,
        });
        state.busiest.record(hour, target.activity.busiest);
        if let Some(at) = target.activity.since_request.and_then(|ago| now.checked_sub(ago)) {
            state.used = state.used.max(at);
        }
        let unused = now.saturating_duration_since(state.used);
        if !unused_after.is_zero() && unused >= unused_after {
            if !state.paused {
                state.paused = true;
                info!(deployment = %target.deployment, project = %target.entry.project, region = %target.region, unused_h = unused.as_secs() / 3600, "keep-warm paused: no request for a long time; it resumes with the next one");
            }
            continue;
        }
        if state.paused {
            state.paused = false;
            state.next = now;
            info!(deployment = %target.deployment, project = %target.entry.project, region = %target.region, "keep-warm resumed");
        }
        if now < state.next {
            continue;
        }
        let count = count(target.keep, &state.busiest, hour);
        if target.keep == KeepWarm::Auto && count != state.count {
            info!(deployment = %target.deployment, project = %target.entry.project, region = %target.region, environments = count, before = state.count, "keep-warm auto: as many as the busiest moment of the last day");
        }
        state.count = count;
        if count == 0 {
            continue;
        }
        // An open invocation's environment starts its idle clock when it ends, some time from
        // now; one that has ended started its clock then.
        match target.activity.at_least.get(count - 1).copied().filter(|ago| *ago < every) {
            Some(ago) => state.next = now + (every - ago),
            None => {
                state.next = now + every;
                rounds.push((target, count));
            }
        }
    }
    rounds
}

/// A deployment's round failed: its next one comes after `RETRY_AFTER`, unless one is due sooner.
fn retry_soon(kept: &mut HashMap<Key, Kept>, key: Key, now: Instant, every: Duration) {
    if let Some(state) = kept.get_mut(&key) {
        state.next = state.next.min(now + RETRY_AFTER.min(every));
    }
}

/// What sends a warm invocation: Lambda (`Spawner::warm`), or a stand-in in tests.
pub trait Warm: Send + Sync + 'static {
    fn warm(&self, hold_ms: u64) -> impl Future<Output = Result<Option<WarmReport>, InvokeFailure>> + Send;
}

impl Warm for Spawner {
    fn warm(&self, hold_ms: u64) -> impl Future<Output = Result<Option<WarmReport>, InvokeFailure>> + Send {
        Spawner::warm(self, hold_ms)
    }
}

/// One round for one deployment: `need` warm invocations at once, each held open `HOLD_MS` when
/// there are several, so each lands in an environment of its own. Says on `done` how many
/// environments were found ready, and whether to try again soon.
async fn round<W: Warm>(warm: Arc<W>, target: Target, need: usize, tally: Arc<std::sync::Mutex<Tally>>, done: tokio::sync::mpsc::UnboundedSender<Done>) {
    let started = Instant::now();
    let hold_ms = if need > 1 { HOLD_MS } else { 0 };
    // Each is an invocation, billed as one (`spend.rs`).
    let owner = crate::spend::Owner::new(&target.entry.project, &target.deployment, &target.region);
    let sent: Vec<_> = (0..need)
        .map(|_| {
            let warm = warm.clone();
            let spent = crate::spend::ledger().invocation(owner.clone(), target.entry.memory_mb, false);
            tokio::spawn(async move {
                let report = warm.warm(hold_ms).await;
                drop(spent);
                report
            })
        })
        .collect();
    let (mut ready, mut new, mut throttled, mut error) = (0, 0, 0, None);
    let mut environments = HashSet::new();
    // The longest app start among the environments that are new.
    let mut app_start_ms = 0;
    for task in sent {
        match task.await {
            Ok(Ok(Some(report))) if report.ok => {
                ready += 1;
                environments.insert(report.env_id);
                if report.env_invocations <= 1 {
                    new += 1;
                    app_start_ms = app_start_ms.max(report.app_start_ms);
                }
            }
            Ok(Ok(Some(report))) => error = Some(format!("the app didn't start: {}", report.error.unwrap_or_default())),
            // A runtime from before warm invocations: its environment was used, which keeps it.
            Ok(Ok(None)) => {
                ready += 1;
                environments.insert(format!("unknown-{ready}"));
            }
            Ok(Err(e)) if e.throttled => throttled += 1,
            Ok(Err(e)) => error = Some(e.message),
            Err(e) => error = Some(e.to_string()),
        }
    }
    let ms = started.elapsed().as_millis() as u64;
    {
        let mut tally = tally.lock().unwrap();
        tally.rounds += 1;
        tally.invocations += need as u64;
        tally.started += new as u64;
        tally.failed += (need - ready) as u64;
    }
    let (deployment, project, region) = (&target.deployment, &target.entry.project, &target.region);
    // An app that doesn't start isn't helped by asking again sooner; a call that failed is.
    let retry = ready < need && (throttled > 0 || error.as_deref().is_some_and(|e| !e.starts_with("the app didn't start")));
    let _ = done.send(Done { key: (deployment.clone(), region.clone()), ready: environments.len(), retry });
    if let Some(error) = error {
        warn!(deployment, project, region, asked = need, ready, throttled, ms, error, "keep-warm round failed");
    } else if throttled > 0 {
        warn!(deployment, project, region, asked = need, ready, throttled, ms, "keep-warm: Lambda had no room (the account's concurrency)");
    } else if environments.len() < ready {
        // Two invocations of the round ran in one environment, one after the other: the other
        // environment wasn't touched, and may be lost.
        warn!(deployment, project, region, asked = need, environments = environments.len(), hold_ms, ms, "keep-warm: a round's invocations shared environments");
    } else if new > 0 {
        // Lambda had dropped or replaced it. Either this invocation took the cold start no visitor
        // will, or Lambda had started the new one itself beforehand (it does, for functions
        // invoked regularly): then the round took less time than the app's start did.
        let prepared_by_lambda = ms < app_start_ms + hold_ms;
        info!(deployment, project, region, asked = need, ready, new, ms, app_start_ms, prepared_by_lambda, "keep-warm found a new environment");
    } else {
        debug!(deployment, project, region, asked = need, ready, ms, "keep-warm round");
    }
}

/// Take the keeper's lock: a listener nothing connects to, held until this process exits.
fn lock(address: &str) -> std::io::Result<TcpListener> {
    let address: std::net::SocketAddr = address.parse().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{address:?}: {e}")))?;
    if !address.ip().is_loopback() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{address} isn't a loopback address")));
    }
    TcpListener::bind(address)
}

/// The busiest hours as kept in the state file.
#[derive(Serialize, Deserialize)]
struct Saved {
    deployment: String,
    region: String,
    busiest: Busiest,
}

fn load(path: &Path) -> HashMap<Key, Busiest> {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "keep-warm: the busiest hours can't be read; auto starts from 1");
            return HashMap::new();
        }
    };
    match serde_json::from_slice::<Vec<Saved>>(&text) {
        Ok(saved) => saved.into_iter().map(|s| ((s.deployment, s.region), s.busiest)).collect(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "keep-warm: the busiest hours file isn't readable; auto starts from 1");
            HashMap::new()
        }
    }
}

/// Write the busiest hours down, through a file beside it renamed into place.
fn save(path: &Path, kept: &HashMap<Key, Kept>, hour: u64) -> std::io::Result<()> {
    let saved: Vec<Saved> = kept
        .iter()
        .filter(|(_, k)| k.busiest.day(hour) > 0)
        .map(|((deployment, region), k)| Saved { deployment: deployment.clone(), region: region.clone(), busiest: k.busiest.clone() })
        .collect();
    let next = path.with_extension("json.new");
    std::fs::write(&next, serde_json::to_vec(&saved)?)?;
    std::fs::rename(next, path)
}

fn unix_hour() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / 3600
}

/// Keep environments alive until the router exits.
pub async fn run(registry: Arc<Registry>, settings: Settings) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The lock, held while this router runs once it has it; `active` from then on (from the
    // start without one).
    let mut _held: Option<TcpListener> = None;
    let mut active = false;
    let mut waiting_said = false;
    let mut kept = HashMap::new();
    // What the router before this one wrote down, read when this one starts keeping.
    let mut remembered = HashMap::new();
    let tally = Arc::new(std::sync::Mutex::new(Tally::default()));
    let (done_tx, mut done) = tokio::sync::mpsc::unbounded_channel::<Done>();
    let mut summary_at = Instant::now() + SUMMARY_EVERY;
    let (mut save_at, mut save_warned) = (Instant::now() + SAVE_EVERY, false);
    info!(keep = %settings.keep, every_s = settings.every.as_secs(), unused_after_h = settings.unused_after.as_secs() / 3600, lock = settings.lock.as_deref(), state = ?settings.state, "keep-warm on");
    loop {
        tick.tick().await;
        if !active {
            if let Some(address) = &settings.lock {
                match lock(address) {
                    Ok(listener) => {
                        info!(lock = %address, took_over = waiting_said, "this router keeps environments warm");
                        _held = Some(listener);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                        if !waiting_said {
                            info!(lock = %address, "another router on this machine keeps environments warm; taking over when it exits");
                            waiting_said = true;
                        }
                        continue;
                    }
                    Err(e) => {
                        if !waiting_said {
                            warn!(lock = %address, error = %e, "the keep-warm lock can't be taken; no environments kept warm by this router");
                            waiting_said = true;
                        }
                        continue;
                    }
                }
            }
            active = true;
            // What the router before this one wrote down, up to a minute before it exited.
            if let Some(path) = &settings.state {
                remembered = load(path);
                if !remembered.is_empty() {
                    info!(deployments = remembered.len(), path = %path.display(), "keep-warm: busiest hours read");
                }
            }
        }
        let now = Instant::now();
        let hour = unix_hour();
        while let Ok(round) = done.try_recv() {
            if round.ready > 0 {
                registry.kept_warm(&round.key.0, round.ready);
            }
            if round.retry {
                retry_soon(&mut kept, round.key, now, settings.every);
            }
        }
        for (target, need) in due(&mut kept, &remembered, registry.keep_warm(settings.keep), now, hour, settings.every, settings.unused_after) {
            let spawner = Arc::new(registry.spawner_for(&target.entry));
            tokio::spawn(round(spawner, target, need, tally.clone(), done_tx.clone()));
        }
        if let Some(path) = settings.state.as_deref().filter(|_| now >= save_at) {
            save_at = now + SAVE_EVERY;
            match save(path, &kept, hour) {
                Ok(()) => save_warned = false,
                Err(e) if !save_warned => {
                    save_warned = true;
                    warn!(path = %path.display(), error = %e, "keep-warm: the busiest hours can't be written; after a restart auto starts from 1");
                }
                Err(_) => {}
            }
        }
        if now >= summary_at {
            summary_at = now + SUMMARY_EVERY;
            let done = std::mem::take(&mut *tally.lock().unwrap());
            if done.rounds > 0 {
                info!(deployments = kept.len(), paused = kept.values().filter(|k| k.paused).count(), rounds = done.rounds, invocations = done.invocations, started = done.started, failed = done.failed, minutes = SUMMARY_EVERY.as_secs() / 60, "keep-warm summary");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(deployment: &str, count: u32, open: usize) -> Target {
        let entry: Deployment = serde_json::from_value(serde_json::json!({ "project": "p", "function": "f", "qualifier": "1" })).unwrap();
        let activity = Activity { busiest: open, at_least: vec![Duration::ZERO; open], since_request: None };
        Target { deployment: deployment.to_owned(), region: "us-east-1".to_owned(), keep: KeepWarm::Count(count), activity, entry }
    }

    /// One whose last invocation ended, and last request began, `ago` ago; it had `at_once` open
    /// at once then.
    fn used_by(deployment: &str, count: u32, ago: Duration, at_once: usize) -> Target {
        let mut t = target(deployment, count, 0);
        t.activity = Activity { busiest: at_once, at_least: vec![ago; at_once], since_request: Some(ago) };
        t
    }

    fn used(deployment: &str, count: u32, ago: Duration) -> Target {
        used_by(deployment, count, ago, 1)
    }

    /// With no pausing of unused deployments, nothing remembered, at hour 0.
    fn due(kept: &mut HashMap<Key, Kept>, targets: Vec<Target>, now: Instant, every: Duration) -> Vec<(Target, usize)> {
        super::due(kept, &HashMap::new(), targets, now, 0, every, Duration::ZERO)
    }

    fn names(rounds: &[(Target, usize)]) -> Vec<(&str, usize)> {
        rounds.iter().map(|(t, need)| (t.deployment.as_str(), *need)).collect()
    }

    #[test]
    fn rounds_come_every_interval_after_a_first_one_soon() {
        let every = Duration::from_secs(240);
        let start = Instant::now();
        let mut next = HashMap::new();
        // First sight: nothing at once (two routers starting together would both send), all
        // within FIRST_WITHIN.
        assert!(due(&mut next, vec![target("a", 1, 0), target("b", 2, 0)], start, every).is_empty());
        let rounds = due(&mut next, vec![target("a", 1, 0), target("b", 2, 0)], start + FIRST_WITHIN, every);
        assert_eq!(names(&rounds), [("a", 1), ("b", 2)]);
        // Not again until `every` has passed.
        assert!(due(&mut next, vec![target("a", 1, 0), target("b", 2, 0)], start + FIRST_WITHIN + every - Duration::from_secs(1), every).is_empty());
        let rounds = due(&mut next, vec![target("a", 1, 0), target("b", 2, 0)], start + FIRST_WITHIN + every, every);
        assert_eq!(names(&rounds), [("a", 1), ("b", 2)]);
    }

    #[test]
    fn a_round_is_whole_unless_traffic_had_as_many_open_at_once() {
        let every = Duration::from_secs(240);
        let start = Instant::now();
        let mut next = HashMap::new();
        due(&mut next, vec![target("a", 1, 0), target("b", 3, 0), target("c", 3, 0)], start, every);
        // `a` has a worker open: its environment is in use, so it needs nothing this turn, and
        // its next turn is a whole interval away all the same. `b` has one of its three open:
        // that's not three environments in use, so the round sends all three together (only the
        // difference would land in the environment Lambda used last, and lose the others). `c`
        // has three open at once: all three are in use.
        let rounds = due(&mut next, vec![target("a", 1, 1), target("b", 3, 1), target("c", 3, 3)], start + FIRST_WITHIN, every);
        assert_eq!(names(&rounds), [("b", 3)]);
        assert!(due(&mut next, vec![target("a", 1, 0), target("b", 3, 0), target("c", 3, 0)], start + FIRST_WITHIN + every / 2, every).is_empty());
        // `b` left production: forgotten. Back later, it's new again.
        let later = start + FIRST_WITHIN + every;
        assert_eq!(names(&due(&mut next, vec![target("a", 1, 0)], later, every)), [("a", 1)]);
        assert_eq!(next.len(), 1);
        assert!(due(&mut next, vec![target("a", 1, 0), target("b", 3, 0)], later + Duration::from_secs(1), every).is_empty());
        assert_eq!(names(&due(&mut next, vec![target("a", 1, 0), target("b", 3, 0)], later + Duration::from_secs(1) + FIRST_WITHIN, every)), [("b", 3)]);
    }

    #[test]
    fn real_traffic_takes_the_place_of_keep_alives() {
        let every = Duration::from_secs(120);
        let start = Instant::now();
        let mut kept = HashMap::new();
        due(&mut kept, vec![target("a", 1, 0), target("b", 3, 0), target("c", 3, 0)], start, every);
        let at = start + FIRST_WITHIN;
        // A visitor's request ended on `a` 30 s ago: its environment is fresh, so no round now,
        // and the next is due 120 s after that end, not after now. `b` keeps three, and traffic
        // had one open: the round goes out whole. `c` had three at once 30 s ago: covered too.
        let rounds = due(&mut kept, vec![used("a", 1, Duration::from_secs(30)), used("b", 3, Duration::from_secs(30)), used_by("c", 3, Duration::from_secs(30), 3)], at, every);
        assert_eq!(names(&rounds), [("b", 3)]);
        assert!(due(&mut kept, vec![used("a", 1, Duration::from_secs(119)), target("b", 3, 0), used_by("c", 3, Duration::from_secs(119), 3)], at + Duration::from_secs(89), every).is_empty());
        // Nothing since: the round comes when that request's 120 s are up.
        assert_eq!(
            names(&due(&mut kept, vec![used("a", 1, Duration::from_secs(120)), target("b", 3, 0), used_by("c", 3, Duration::from_secs(120), 3)], at + Duration::from_secs(90), every)),
            [("a", 1), ("c", 3)]
        );
        // Steady traffic (a request every minute): never a round.
        let mut now = at + Duration::from_secs(90);
        for _ in 0..20 {
            now += Duration::from_secs(60);
            let rounds = due(&mut kept, vec![used("a", 1, Duration::from_secs(40))], now, every);
            assert!(rounds.is_empty(), "{:?}", names(&rounds));
        }
    }

    #[test]
    fn auto_keeps_as_many_as_the_busiest_moment_of_the_last_day() {
        let every = Duration::from_secs(120);
        let start = Instant::now();
        let mut kept = HashMap::new();
        let auto = |busiest: usize, ago: Option<Duration>| {
            let mut t = target("a", 0, 0);
            t.keep = KeepWarm::Auto;
            t.activity = Activity { busiest, at_least: ago.map_or(Vec::new(), |ago| vec![ago; busiest]), since_request: ago };
            t
        };
        let at = |kept: &mut HashMap<Key, Kept>, t: Target, now: Instant, hour: u64| names(&super::due(kept, &HashMap::new(), vec![t], now, hour, every, Duration::ZERO)).first().map(|r| r.1);
        // Nothing yet: one, as the default keeps.
        at(&mut kept, auto(0, None), start, 1000);
        assert_eq!(at(&mut kept, auto(0, None), start + FIRST_WITHIN, 1000), Some(1));
        // A burst of 16 at once at hour 1000 (seen at the next look: it has ended since).
        let mut now = start + FIRST_WITHIN + every;
        assert_eq!(at(&mut kept, auto(16, Some(Duration::from_secs(200))), now, 1000), Some(16));
        // An hour later, and every round until the day has passed: 16, all at once.
        let mut rounds = Vec::new();
        for hour in 1001..1024 {
            for _ in 0..30 {
                now += every;
                rounds.extend(at(&mut kept, auto(0, Some(Duration::from_secs(3600))), now, hour));
            }
        }
        assert!(rounds.len() >= 23 * 29 && rounds.iter().all(|n| *n == 16), "{} rounds, {:?}", rounds.len(), rounds.iter().collect::<HashSet<_>>());
        // A day after the burst it's back to one.
        now += every;
        assert_eq!(at(&mut kept, auto(0, Some(Duration::from_secs(3600))), now, 1024), Some(1));
        // At most KEEP_WARM_MAX.
        now += every;
        assert_eq!(at(&mut kept, auto(300, Some(Duration::from_secs(200))), now, 1025), Some(KEEP_WARM_MAX as usize));
    }

    #[test]
    fn the_busiest_hours_survive_a_restart_through_the_file() {
        let dir = std::env::temp_dir().join(format!("forkrail-keepwarm-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("busiest.json");
        let every = Duration::from_secs(120);
        let start = Instant::now();
        let mut old = HashMap::new();
        let mut t = target("a", 0, 0);
        (t.keep, t.activity.busiest) = (KeepWarm::Auto, 9);
        super::due(&mut old, &HashMap::new(), vec![t], start, 500, every, Duration::ZERO);
        save(&path, &old, 500).unwrap();
        // The new router reads it, and keeps 9 from its first round.
        let remembered = load(&path);
        let mut new = HashMap::new();
        let fresh = || {
            let mut t = target("a", 0, 0);
            t.keep = KeepWarm::Auto;
            t
        };
        super::due(&mut new, &remembered, vec![fresh()], start, 503, every, Duration::ZERO);
        let rounds = super::due(&mut new, &remembered, vec![fresh()], start + FIRST_WITHIN, 503, every, Duration::ZERO);
        assert_eq!(names(&rounds), [("a", 9)]);
        // A file that isn't one: nothing remembered, no panic.
        std::fs::write(&path, b"nonsense").unwrap();
        assert!(load(&path).is_empty());
        assert!(load(&dir.join("missing.json")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
        // A day back, no further.
        let mut a = Busiest::default();
        for (hour, busiest) in [(1, 3), (2, 7), (2, 5), (3, 1)] {
            a.record(hour, busiest);
        }
        assert_eq!(a, Busiest(vec![(1, 3), (2, 7), (3, 1)]));
        assert_eq!((a.day(24), a.day(25), a.day(26)), (7, 7, 1));
        // A quiet hour forgets what's more than a day old.
        a.record(27, 0);
        assert_eq!(a, Busiest::default());
    }

    /// A stand-in for Lambda: counts invocations open at the same moment.
    #[derive(Default)]
    struct Counting {
        open: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
        holds: std::sync::Mutex<Vec<u64>>,
        n: std::sync::atomic::AtomicUsize,
    }

    impl Warm for Counting {
        fn warm(&self, hold_ms: u64) -> impl Future<Output = Result<Option<WarmReport>, InvokeFailure>> + Send {
            use std::sync::atomic::Ordering::SeqCst;
            let open = self.open.fetch_add(1, SeqCst) + 1;
            self.most.fetch_max(open, SeqCst);
            self.holds.lock().unwrap().push(hold_ms);
            let n = self.n.fetch_add(1, SeqCst);
            async move {
                // As the runtime does: answers after `hold_ms`.
                tokio::time::sleep(Duration::from_millis(hold_ms)).await;
                self.open.fetch_sub(1, SeqCst);
                Ok(Some(WarmReport { warm: true, ok: true, env_id: format!("env{n}"), env_invocations: 5, app_start_ms: 0, error: None }))
            }
        }
    }

    #[tokio::test]
    async fn a_round_touches_every_kept_environment_at_the_same_moment() {
        for need in [1, 5, 16, 20] {
            let lambda = Arc::new(Counting::default());
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            round(lambda.clone(), target("a", need as u32, 0), need, Arc::default(), tx).await;
            // All of them were open together, so each had an environment of its own.
            assert_eq!(lambda.most.load(std::sync::atomic::Ordering::SeqCst), need, "{need}");
            let holds = lambda.holds.lock().unwrap().clone();
            assert_eq!(holds, vec![if need > 1 { HOLD_MS } else { 0 }; need]);
            let done = rx.recv().await.unwrap();
            assert_eq!((done.ready, done.retry), (need, false));
        }
    }

    #[test]
    fn a_deployment_nobody_asks_for_is_paused_until_its_next_request() {
        let every = Duration::from_secs(120);
        let week = Duration::from_secs(7 * 86_400);
        let start = Instant::now();
        let mut kept = HashMap::new();
        let rounds_in = |kept: &mut HashMap<Key, Kept>, from: Instant, span: Duration, target_at: &dyn Fn(Instant) -> Target| {
            let (mut now, mut rounds) = (from, 0);
            while now < from + span {
                rounds += super::due(kept, &HashMap::new(), vec![target_at(now)], now, 0, every, week).len();
                now += Duration::from_secs(60);
            }
            rounds
        };
        // Kept for its first week though nobody asks: one round every two minutes.
        let first_week = rounds_in(&mut kept, start, week, &|_| target("a", 1, 0));
        assert!((5030..=5040).contains(&first_week), "{first_week}");
        // Then paused: nothing for a month.
        let after = start + week;
        assert_eq!(rounds_in(&mut kept, after, Duration::from_secs(30 * 86_400), &|_| target("a", 1, 0)), 0);
        assert!(kept[&("a".to_owned(), "us-east-1".to_owned())].paused);
        // A visitor comes (and waits for a cold start): kept again from then, for a week more.
        let visit = after + Duration::from_secs(30 * 86_400);
        let resumed = rounds_in(&mut kept, visit, week + Duration::from_secs(86_400), &|now| used("a", 1, now - visit));
        assert!(!kept.is_empty() && (5030..=5040).contains(&resumed), "{resumed}");
        // A project visited once a day is never paused.
        let mut kept = HashMap::new();
        let daily = rounds_in(&mut kept, start, Duration::from_secs(30 * 86_400), &|now| used("a", 1, Duration::from_secs((now - start).as_secs() % 86_400)));
        assert!(daily > 21_000, "{daily}");
        // And with pausing off, nothing ever is.
        let mut kept = HashMap::new();
        let mut now = start;
        let mut rounds = 0;
        while now < start + Duration::from_secs(30 * 86_400) {
            rounds += due(&mut kept, vec![target("a", 1, 0)], now, every).len();
            now += Duration::from_secs(60);
        }
        assert!(rounds > 21_000, "{rounds}");
    }

    #[test]
    fn a_failed_round_is_tried_again_soon() {
        let every = Duration::from_secs(240);
        let start = Instant::now();
        let mut next = HashMap::new();
        due(&mut next, vec![target("a", 1, 0)], start, every);
        let at = start + FIRST_WITHIN;
        assert_eq!(names(&due(&mut next, vec![target("a", 1, 0)], at, every)), [("a", 1)]);
        // Its invocation failed a moment later.
        retry_soon(&mut next, ("a".into(), "us-east-1".into()), at + Duration::from_secs(1), every);
        assert!(due(&mut next, vec![target("a", 1, 0)], at + RETRY_AFTER, every).is_empty());
        assert_eq!(names(&due(&mut next, vec![target("a", 1, 0)], at + Duration::from_secs(1) + RETRY_AFTER, every)), [("a", 1)]);
        // A deployment that's gone meanwhile isn't brought back by its failure.
        retry_soon(&mut next, ("gone".into(), "us-east-1".into()), at, every);
        assert_eq!(next.len(), 1);
    }

    #[test]
    fn first_rounds_are_spread_out() {
        let now = Instant::now();
        let offsets: std::collections::BTreeSet<Duration> = (0..50).map(|i| first_due(now, &format!("dep{i}"), "us-east-1", FIRST_WITHIN) - now).collect();
        assert!(offsets.iter().all(|d| *d < FIRST_WITHIN));
        assert!(offsets.len() > 40, "{} distinct of 50", offsets.len());
    }

    #[test]
    fn the_lock_is_one_at_a_time_and_loopback_only() {
        let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
        let first = lock(&free).expect("free port");
        assert_eq!(lock(&free).unwrap_err().kind(), std::io::ErrorKind::AddrInUse);
        drop(first);
        lock(&free).expect("free again once its holder is gone");
        assert!(lock("0.0.0.0:7446").is_err());
        assert!(lock("nonsense").is_err());
    }
}
