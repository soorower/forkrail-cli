//! Spend budgets (C15, `docs/spend-protection.md`): each project's monthly spend against its own
//! budget and the install's, and the pause when one is reached.
//!
//! Every `READ_EVERY` the router stores its own counts and reads each project's month to date from
//! ClickHouse (`usage_1h`, every router's: `spend.rs`); every second in between it checks that
//! against what it has counted itself since, so its own spend is seen within a second (another
//! router's within `READ_EVERY`). A project is paused when:
//!
//! - the routing table says so (`projects.<name>.spend.paused`: control records every pause, and
//!   only a person resumes, so raising a budget never resumes anything by itself);
//! - its own budget is reached; or
//! - the install's is reached, unless it's `exempt` (the dashboard, so the install stays
//!   manageable).
//!
//! A paused project's hosts answer 503 `DEPLOYMENT_PAUSED` before anything of it runs or is
//! served (`proxy.rs`), and its workers are let go at once, requests in flight with them: their
//! invocations return and Lambda freezes their environments. The router tells control
//! (`spend.paused` on the events queue), which records the pause, sets the function's reserved
//! concurrency to 0 so AWS refuses any invocation, and tells the team.
//!
//! The check is the same every time from the same numbers, so a router that restarts pauses again
//! at once, and two routers decide alike. While ClickHouse can't be read, the month to date is the
//! last one read plus what this router counted since, `FORKRAIL_SPEND_BLIND_FACTOR` times (default
//! 2: as if the other region spent as much), so being blind never lets spend through unseen.
//!
//! The team hears at 50, 75 and 100% of a budget and each further 100% up to 1,000%
//! (`budget.reached`, with the payload webhooks written for the platform this copies expect), and
//! when a project spends over `FAST_SHARE` of its monthly budget in an hour (`spend.fast`: an
//! email only; a real user can be busy for an hour). Each event's id is made from what it says
//! (`events::id_of`), so the notifier tells the team once although every router sends it, and a
//! router that restarts mid-month doesn't say again what it said before. Without an install
//! budget in the table nothing is paused by it, and the router says so.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use forkrail_common::prices::Nanos;
use forkrail_common::routes::{ProjectSpend, Routes};
use serde_json::json;
use tracing::{info, warn};

/// How often this router's counts are stored and every router's month to date read.
pub const READ_EVERY: Duration = Duration::from_secs(10);
/// How often the budgets are checked against what's known.
const CHECK_EVERY: Duration = Duration::from_secs(1);
/// A read older than this means ClickHouse can't be read: what this router counts since is taken
/// `blind_factor` times.
const BLIND_AFTER: Duration = Duration::from_secs(30);
/// How often the last hour's spend is read, for the fast-spend warning.
const FAST_EVERY: Duration = Duration::from_secs(60);
/// A project spending more than this share of its monthly budget in an hour is said to the team.
pub const FAST_SHARE: f64 = 0.10;
/// Alerts' thresholds below 100%; past it, every further 100% up to 1,000% (Vercel's).
const THRESHOLDS: [u32; 3] = [50, 75, 100];
/// Blind this long, it's said (and again every so often).
const BLIND_WARN_AFTER: Duration = Duration::from_secs(120);
/// How often the month to date is published for the guard, and forkrail's own costs read from it
/// (`Files`).
const PUBLISH_EVERY: Duration = Duration::from_secs(300);

/// The spend files in the routing table's bucket.
///
/// - `publish`: where a public router publishes its month to date (`state/spend/<region>.json`),
///   for the guard (crates/guard) to hold AWS's own numbers against: a meter that misses
///   something shows as a gap. Only a router with a base domain publishes, so a bench or local
///   one never overwrites a real router's figures.
/// - What the guard counted of forkrail's own costs, which no router sees (`OWN_SPEND_KEY`:
///   storage, the tables, control and forkrail's other functions): every router adds it to the
///   team's spend. It only grows in a month, and the guard writes it hourly (every few minutes
///   near a budget), so a router that can't read it keeps the last it read.
pub struct Files {
    pub client: aws_sdk_s3::Client,
    pub bucket: String,
    pub publish: Option<String>,
}

/// What the routing table says about spend.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub install_n: Option<Nanos>,
    pub projects: HashMap<String, ProjectSpend>,
}

impl Table {
    pub fn of(routes: &Routes) -> Self {
        Self {
            install_n: routes.spend.as_ref().map(|s| s.budget_n),
            projects: routes.projects.iter().filter_map(|(name, p)| Some((name.clone(), p.spend.clone()?))).collect(),
        }
    }
}

/// Why a project is paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Why {
    /// The table says so: control recorded it, or a person paused it.
    Recorded,
    ProjectBudget,
    InstallBudget,
}

impl Why {
    pub fn name(self) -> &'static str {
        match self {
            Why::Recorded => "recorded",
            Why::ProjectBudget => "project_budget",
            Why::InstallBudget => "install_budget",
        }
    }
}

/// The outcome of a check, read on every request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Verdict {
    /// The install's budget is reached: every project not exempt is paused.
    pub install_over: bool,
    pub exempt: HashSet<String>,
    /// Paused for their own sake.
    pub projects: HashMap<String, Why>,
}

impl Verdict {
    pub fn paused(&self, project: &str) -> Option<Why> {
        if project.is_empty() {
            return None;
        }
        if let Some(why) = self.projects.get(project) {
            return Some(*why);
        }
        (self.install_over && !self.exempt.contains(project)).then_some(Why::InstallBudget)
    }
}

/// The check: what's paused, from the table and each project's month to date (the install's
/// own spend under `""`).
pub fn decide(table: &Table, spent: &HashMap<String, Nanos>) -> Verdict {
    let install_total: Nanos = spent.values().sum();
    let mut verdict = Verdict { install_over: table.install_n.is_some_and(|b| install_total >= b), ..Verdict::default() };
    for (name, p) in &table.projects {
        if p.exempt {
            verdict.exempt.insert(name.clone());
        }
        if p.paused.is_some() {
            verdict.projects.insert(name.clone(), Why::Recorded);
        } else if let Some(budget) = p.budget_n
            && spent.get(name).copied().unwrap_or(0) >= budget
        {
            verdict.projects.insert(name.clone(), Why::ProjectBudget);
        }
    }
    verdict
}

/// The threshold a share of a budget has reached: 50, 75, 100, then 200, 300 … 1,000; 0 below 50.
pub fn threshold(spent: Nanos, budget: Nanos) -> u32 {
    if budget <= 0 {
        return if spent > 0 { 1000 } else { 0 };
    }
    let pct = (i128::from(spent) * 100 / i128::from(budget)).clamp(0, 100_000) as u32;
    match pct {
        p if p >= 200 => (p / 100 * 100).min(1000),
        p => THRESHOLDS.iter().rev().copied().find(|t| p >= *t).unwrap_or(0),
    }
}

struct State {
    table: Table,
    /// The last month to date read from ClickHouse, when, and this router's own months then.
    read: Option<(Instant, HashMap<String, Nanos>, HashMap<Arc<str>, Nanos>)>,
    /// Of that, function time alone, and the first hour the ledger has this month (for the guard,
    /// which compares only a month the ledger has whole).
    read_fn: HashMap<String, Nanos>,
    since: Option<u64>,
    /// forkrail's own costs as the guard counted them (`Files`), per project, and their month.
    own: HashMap<String, Nanos>,
    own_month: u32,
    /// Thresholds said this month: scope (`""` the install) → the highest.
    said: HashMap<String, u32>,
    /// Fast spend said: project → when.
    fast_said: HashMap<String, Instant>,
    month: u32,
    blind_said: Option<Instant>,
    warned_no_budget: bool,
}

pub struct Budgets {
    verdict: RwLock<Arc<Verdict>>,
    state: Mutex<State>,
    blind_factor: i64,
    /// Projects paused by a check that had no registry (a new table, applied while the registry
    /// is locked): their workers are let go at the next check that has one.
    to_stop: Mutex<HashSet<String>>,
}

static BUDGETS: LazyLock<Budgets> = LazyLock::new(|| Budgets {
    verdict: RwLock::default(),
    state: Mutex::new(State {
        table: Table::default(),
        read: None,
        read_fn: HashMap::new(),
        since: None,
        own: HashMap::new(),
        own_month: 0,
        said: HashMap::new(),
        fast_said: HashMap::new(),
        month: 0,
        blind_said: None,
        warned_no_budget: false,
    }),
    blind_factor: std::env::var("FORKRAIL_SPEND_BLIND_FACTOR").ok().and_then(|v| v.parse().ok()).unwrap_or(2).max(1),
    to_stop: Mutex::default(),
});

pub fn budgets() -> &'static Budgets {
    &BUDGETS
}

/// Why `project` is paused, if it is: read on every request.
pub fn paused(project: &str) -> Option<Why> {
    BUDGETS.verdict.read().unwrap().paused(project)
}

impl Budgets {
    /// The routing table changed.
    pub fn set_table(&self, routes: &Routes) {
        let table = Table::of(routes);
        let mut state = self.state.lock().unwrap();
        if state.table == table {
            return;
        }
        state.table = table;
        // A recorded pause or resume applies at once, without waiting for the next check.
        let spent = self.spent(&mut state, None);
        drop(state);
        self.apply(&spent, None);
    }

    /// Each project's month to date: what ClickHouse said (`read`, when it said it), else the
    /// last it said plus this router's own since, `blind_factor` times; with what this router
    /// hasn't sent yet.
    fn spent(&self, state: &mut State, read: Option<HashMap<String, Nanos>>) -> HashMap<String, Nanos> {
        let ledger = crate::spend::ledger();
        let mine = ledger.months();
        let month = crate::spend::month_of(unix_now());
        if state.month != month {
            // A new month: last month's total goes to control (the default budget's step), and its
            // reads and alerts are over.
            if state.month != 0
                && let Some((_, read, _)) = &state.read
                && state.table.install_n.is_some()
            {
                let spent_n: Nanos = read.values().sum();
                info!(month = state.month, spent_n, "spend: month ended");
                crate::events::to_control("spend.month_end", json!({ "month": state.month, "spentN": spent_n, "budgetN": state.table.install_n }));
            }
            state.month = month;
            state.read = None;
            state.said.clear();
        }
        if let Some(read) = read {
            state.read = Some((Instant::now(), read, mine.clone()));
        }
        let mut spent: HashMap<String, Nanos> = match &state.read {
            Some((at, read, mine_then)) => {
                let factor = if at.elapsed() >= BLIND_AFTER { self.blind_factor } else { 1 };
                let mut spent = read.clone();
                for (project, now) in &mine {
                    let since = now - mine_then.get(project).copied().unwrap_or(0);
                    *spent.entry(project.to_string()).or_default() += since * factor;
                }
                spent
            }
            // Nothing read yet (no ClickHouse here, or not answered since start): this router's.
            None => mine.iter().map(|(p, n)| (p.to_string(), *n)).collect(),
        };
        // Counted here and not in what ClickHouse said: what the sink still has to send, and what
        // was counted since the last store.
        for (project, n) in ledger.unconfirmed().into_iter().chain(ledger.pending()) {
            *spent.entry(project.to_string()).or_default() += n;
        }
        // Builds, which control counts in the table (no router sees them).
        for (project, p) in &state.table.projects {
            let n = p.builds_n(month);
            if n > 0 {
                *spent.entry(project.clone()).or_default() += n;
            }
        }
        // forkrail's own costs, as the guard counted them (no router sees them either).
        add_own(&mut spent, &state.own, state.own_month, month);
        spent
    }

    /// What the guard counted of forkrail's own costs (`Files`): taken if it's this month's.
    fn set_own(&self, text: &[u8]) {
        let Some((month, own)) = parse_own(text) else { return };
        let mut state = self.state.lock().unwrap();
        if month == crate::spend::month_of(unix_now()) {
            state.own = own;
            state.own_month = month;
        }
    }

    /// Put a check's verdict in force: let go of newly paused projects' workers, and say what's
    /// new. `registry` None in tests.
    fn apply(&self, spent: &HashMap<String, Nanos>, registry: Option<&crate::registry::Registry>) {
        let (verdict, table) = {
            let state = self.state.lock().unwrap();
            (decide(&state.table, spent), state.table.clone())
        };
        let before = self.verdict.read().unwrap().clone();
        // Pauses put in force without a registry: their workers go now, if they're still paused.
        if let Some(registry) = registry {
            let waiting: Vec<String> = self.to_stop.lock().unwrap().drain().collect();
            for project in waiting.iter().filter(|p| verdict.paused(p).is_some()) {
                registry.stop_project(project);
            }
        }
        if *before == verdict {
            return;
        }
        let now_paused = |v: &Verdict, p: &str| v.paused(p).is_some();
        // Every project the router knows, and every one in either verdict.
        let mut projects: HashSet<String> = registry.map(|r| r.deployment_projects().into_iter().map(|(_, p)| p).collect()).unwrap_or_default();
        projects.extend(verdict.projects.keys().cloned());
        projects.extend(table.projects.keys().cloned());
        projects.extend(spent.keys().filter(|p| !p.is_empty()).cloned());
        let install_total: Nanos = spent.values().sum();
        // In force before any worker is let go, so a request retried off a stopped worker finds
        // the project paused.
        let verdict = Arc::new(verdict);
        *self.verdict.write().unwrap() = verdict.clone();
        let month = crate::spend::month_of(unix_now());
        // The team hears of each budget's pauses once: the install's reached pauses every project.
        let mut newly: std::collections::BTreeMap<(Why, Option<Nanos>), Vec<(String, Nanos)>> = Default::default();
        let mut projects: Vec<String> = projects.into_iter().collect();
        projects.sort();
        for project in &projects {
            match (now_paused(&before, project), verdict.paused(project)) {
                (false, Some(why)) => {
                    warn!(%project, why = why.name(), spent_n = spent.get(project).copied().unwrap_or(0), install_n = install_total, "spend: project paused");
                    match registry {
                        Some(registry) => registry.stop_project(project),
                        None => {
                            self.to_stop.lock().unwrap().insert(project.clone());
                        }
                    }
                    if why != Why::Recorded {
                        let budget_n = match why {
                            Why::ProjectBudget => table.projects.get(project).and_then(|p| p.budget_n),
                            _ => table.install_n,
                        };
                        let spent_n = spent.get(project).copied().unwrap_or(0);
                        crate::events::to_control(
                            "spend.paused",
                            json!({ "project": project, "reason": why.name(), "spentN": spent_n, "installSpentN": install_total, "budgetN": budget_n, "month": month }),
                        );
                        newly.entry((why, budget_n)).or_default().push((project.clone(), spent_n));
                    }
                }
                (true, None) => info!(%project, "spend: project serving again"),
                _ => {}
            }
        }
        for ((why, budget_n), paused) in newly {
            let names: Vec<String> = paused.iter().map(|(p, _)| p.clone()).collect();
            let what = format!("spend.paused|{}|{month}|{budget_n:?}|{}", why.name(), names.join(","));
            // What spent the most of it, named (a query, off this path).
            let top = paused.iter().max_by_key(|(_, n)| *n).map(|(p, _)| p.clone()).unwrap_or_default();
            let spent_n = if why == Why::ProjectBudget { paused[0].1 } else { install_total };
            let send = async move {
                let cause = cause(&top).await.map(|c| if names.len() > 1 { format!("{top}: {c}") } else { c });
                crate::events::send_as(
                    "spend.paused",
                    Some(crate::events::id_of(&what)),
                    json!({
                        "reason": why.name(),
                        "project": (names.len() == 1 && why == Why::ProjectBudget).then(|| names[0].clone()),
                        "projects": names,
                        "spentN": spent_n,
                        "budgetN": budget_n,
                        "month": month,
                        "cause": cause,
                    }),
                );
            };
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(send);
            }
        }
    }

    /// Say each budget's new thresholds.
    fn alerts(&self, spent: &HashMap<String, Nanos>) {
        let mut state = self.state.lock().unwrap();
        let month = state.month;
        let install_total: Nanos = spent.values().sum();
        let mut scopes: Vec<(String, Nanos, Nanos)> = Vec::new();
        if let Some(b) = state.table.install_n {
            scopes.push((String::new(), install_total, b));
        }
        for (name, p) in &state.table.projects {
            if let Some(b) = p.budget_n {
                scopes.push((name.clone(), spent.get(name).copied().unwrap_or(0), b));
            }
        }
        for (scope, spent_n, budget_n) in scopes {
            let reached = threshold(spent_n, budget_n);
            let said = state.said.get(&scope).copied().unwrap_or(0);
            if reached > said {
                state.said.insert(scope.clone(), reached);
                info!(scope = %scope, percent = reached, spent_n, budget_n, "spend: budget threshold");
                crate::events::send_as(
                    "budget.reached",
                    Some(crate::events::id_of(&format!("budget.reached|{scope}|{month}|{reached}|{budget_n}"))),
                    reached_payload(&scope, reached, spent_n, budget_n, month),
                );
            }
        }
    }

    /// Each project spending more than `FAST_SHARE` of its monthly budget in the last hour, not
    /// said in the last hour: (project, the hour's spend, its budget, the month).
    fn fast(&self, last_hour: &HashMap<String, Nanos>) -> Vec<(String, Nanos, Nanos, u32)> {
        let mut state = self.state.lock().unwrap();
        let install_n = state.table.install_n;
        let now = Instant::now();
        let mut out = Vec::new();
        for (project, hour_n) in last_hour {
            if project.is_empty() {
                continue;
            }
            let Some(budget_n) = state.table.projects.get(project).and_then(|p| p.budget_n).or(install_n) else { continue };
            if (*hour_n as f64) <= budget_n as f64 * FAST_SHARE {
                continue;
            }
            if state.fast_said.get(project).is_some_and(|at| now.duration_since(*at) < Duration::from_secs(3600)) {
                continue;
            }
            state.fast_said.insert(project.clone(), now);
            warn!(%project, hour_n, budget_n, "spend: fast spending");
            out.push((project.clone(), *hour_n, budget_n, state.month));
        }
        out
    }

    /// Check on the schedule, reading ClickHouse when there is one.
    pub async fn run(&'static self, registry: Arc<crate::registry::Registry>, files: Option<Files>) {
        let mut published_at: Option<Instant> = None;
        let mut tick = tokio::time::interval(CHECK_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut fast_at = Instant::now();
        let mut read_at: Option<Instant> = None;
        loop {
            tick.tick().await;
            if read_at.is_some_and(|at| at.elapsed() < READ_EVERY) {
                // Between reads: the last one and what this router counted since.
                let spent = {
                    let mut state = self.state.lock().unwrap();
                    self.spent(&mut state, None)
                };
                self.apply(&spent, Some(&registry));
                self.alerts(&spent);
                continue;
            }
            read_at = Some(Instant::now());
            if let Some(files) = &files
                && published_at.is_none_or(|at| at.elapsed() >= PUBLISH_EVERY)
            {
                crate::spend::ledger().s3(forkrail_common::prices::Meter::S3Get);
                match files.client.get_object().bucket(&files.bucket).key(forkrail_common::install::OWN_SPEND_KEY).send().await {
                    Ok(out) => match out.body.collect().await {
                        Ok(body) => self.set_own(&body.into_bytes()),
                        Err(e) => warn!(error = %e, "spend: reading forkrail's own costs failed"),
                    },
                    // None yet (no guard, or not run this month): nothing to add.
                    Err(e) if e.as_service_error().is_some_and(|s| s.is_no_such_key()) || e.raw_response().is_some_and(|r| matches!(r.status().as_u16(), 403 | 404)) => {}
                    Err(e) => warn!(error = %aws_sdk_s3::error::DisplayErrorContext(&e), "spend: reading forkrail's own costs failed"),
                }
            }
            // This router's counts first, so what's read next has them.
            crate::spend::ledger().store().await;
            let read = match crate::sink::query(MONTH_SQL).await {
                Some(Ok(text)) => {
                    let (total, of_fn, since) = parse_month(&text);
                    let mut state = self.state.lock().unwrap();
                    state.read_fn = of_fn;
                    state.since = since;
                    drop(state);
                    Some(total)
                }
                Some(Err(e)) => {
                    let mut state = self.state.lock().unwrap();
                    let since = state.read.as_ref().map(|(at, ..)| at.elapsed());
                    if since.is_none_or(|d| d >= BLIND_WARN_AFTER) && state.blind_said.is_none_or(|at| at.elapsed() >= Duration::from_secs(600)) {
                        state.blind_said = Some(Instant::now());
                        warn!(error = %e, blind_s = since.map(|d| d.as_secs()), factor = self.blind_factor, "spend: month to date can't be read; counting this router's own spend that many times over");
                        crate::events::send("spend.blind", json!({ "error": e, "blindS": since.map(|d| d.as_secs()) }));
                    }
                    None
                }
                None => None,
            };
            let spent = {
                let mut state = self.state.lock().unwrap();
                if state.table.install_n.is_none() && !state.warned_no_budget {
                    state.warned_no_budget = true;
                    warn!("spend: the routing table has no install budget; only projects' own budgets are enforced");
                }
                self.spent(&mut state, read)
            };
            self.apply(&spent, Some(&registry));
            self.alerts(&spent);
            if let Some(files) = &files
                && published_at.is_none_or(|at| at.elapsed() >= PUBLISH_EVERY)
            {
                published_at = Some(Instant::now());
                if let Some(key) = &files.publish {
                    let body = {
                        let state = self.state.lock().unwrap();
                        ledger_json(&spent, &state.read_fn, state.since, state.month, &registry.here())
                    };
                    crate::spend::ledger().s3(forkrail_common::prices::Meter::S3Put);
                    if let Err(e) = files.client.put_object().bucket(&files.bucket).key(key).content_type("application/json").body(body.into_bytes().into()).send().await {
                        warn!(%key, error = %aws_sdk_s3::error::DisplayErrorContext(&e), "spend: publishing the month to date for the guard failed");
                    }
                }
            }
            if fast_at.elapsed() >= FAST_EVERY {
                fast_at = Instant::now();
                if let Some(Ok(text)) = crate::sink::query(HOUR_SQL).await {
                    for (project, hour_n, budget_n, month) in self.fast(&parse(&text)) {
                        let cause = cause(&project).await;
                        crate::events::send_as(
                            "spend.fast",
                            Some(crate::events::id_of(&format!("spend.fast|{project}|{}", unix_now() / 3600))),
                            json!({ "project": project, "hourN": hour_n, "budgetN": budget_n, "month": month, "cause": cause }),
                        );
                    }
                }
            }
        }
    }
}

/// What spent most of `project`'s last hour, in a few words for the team ("86% of it data sent
/// to visitors: /files/big.zip 71%; from 203.0.113.0/24, AS14061 DigitalOcean, DE, 64%"), from
/// the ledger and the access log in ClickHouse. None without them.
async fn cause(project: &str) -> Option<String> {
    // Names are lowercase letters, digits and dashes (routes.rs `valid_project`): safe to quote.
    if project.is_empty() || !project.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return None;
    }
    let rows = |text: String| text.lines().map(|l| l.split('\t').map(str::to_owned).collect::<Vec<_>>()).collect::<Vec<_>>();
    let meters = rows(crate::sink::query(&format!(
        "SELECT meter, sum(cost_n) AS n, sum(sum(cost_n)) OVER () FROM usage WHERE project = '{project}' AND time > now('UTC') - 3600 \
         GROUP BY meter ORDER BY n DESC LIMIT 1 FORMAT TabSeparated"
    ))
    .await?
    .ok()?);
    let first = meters.first()?;
    let (meter, n, total) = (first.first()?.as_str(), first.get(1)?.parse::<f64>().ok()?, first.get(2)?.parse::<f64>().ok()?);
    if total <= 0.0 {
        return None;
    }
    let (words, weight) = match meter {
        "egress_bytes" => ("data sent to visitors", "bytes_out"),
        "ingress_bytes" => ("data received from visitors", "ifNull(bytes_in, 0)"),
        "tunnel_bytes" | "inter_region_bytes" => ("traffic between the routers and the functions", "bytes_out + ifNull(bytes_in, 0)"),
        "fn_egress_bytes" => ("the functions' own uploads", "total_ms * memory_mb"),
        _ => ("function time", "total_ms * memory_mb"),
    };
    let pct = |part: f64, of: f64| if of > 0.0 { (part * 100.0 / of).round() as u32 } else { 0 };
    let mut text = format!("{}% of it {words}", pct(n, total));
    let from = format!("FROM requests WHERE project = '{project}' AND time > now('UTC') - 3600");
    if let Some(Ok(top)) = crate::sink::query(&format!(
        "SELECT if(route != '', route, path) AS what, sum({weight}) AS w, sum(sum({weight})) OVER () {from} GROUP BY what ORDER BY w DESC LIMIT 1 FORMAT TabSeparated"
    ))
    .await
        && let Some(r) = rows(top).first()
        && let (Some(what), Some(w), Some(t)) = (r.first(), r.get(1).and_then(|v| v.parse::<f64>().ok()), r.get(2).and_then(|v| v.parse::<f64>().ok()))
    {
        let what: String = what.chars().take(80).collect();
        text.push_str(&format!(": {what} {}%", pct(w, t)));
    }
    // A client's network: its /24 (IPv4) or /48 (IPv6).
    let net = "if(position(client, ':') > 0, concat(arrayStringConcat(arraySlice(splitByChar(':', client), 1, 3), ':'), '::/48'), \
               concat(arrayStringConcat(arraySlice(splitByChar('.', client), 1, 3), '.'), '.0/24'))";
    if let Some(Ok(top)) = crate::sink::query(&format!(
        "SELECT {net} AS net, any(asn), any(as_org), any(country), sum({weight}) AS w, sum(sum({weight})) OVER () {from} \
         GROUP BY net ORDER BY w DESC LIMIT 1 FORMAT TabSeparated"
    ))
    .await
        && let Some(r) = rows(top).first()
        && r.len() >= 6
        && let (Ok(w), Ok(t)) = (r[4].parse::<f64>(), r[5].parse::<f64>())
    {
        let org: String = r[2].chars().take(40).collect();
        let asn = match r[1].as_str() {
            "0" | "" => String::new(),
            n => format!(", AS{n}{}", if org.is_empty() { String::new() } else { format!(" {org}") }),
        };
        let country = if r[3].is_empty() { String::new() } else { format!(", {}", r[3]) };
        text.push_str(&format!("; from {}{asn}{country}, {}%", r[0], pct(w, t)));
    }
    Some(text)
}

/// `budget.reached`'s payload: the fields webhooks written for the platform this copies read
/// (dollars; `scope` `team` for the install's budget), and forkrail's own (nano-dollars).
fn reached_payload(scope: &str, percent: u32, spent_n: Nanos, budget_n: Nanos, month: u32) -> serde_json::Value {
    let dollars = |n: Nanos| (n as f64 / 1e7).round() / 100.0;
    let install = scope.is_empty();
    json!({
        "budgetAmount": dollars(budget_n),
        "currentSpend": dollars(spent_n),
        "thresholdPercent": percent,
        "scope": if install { "team" } else { "project" },
        "scopeId": if install { "install" } else { scope },
        "scopeName": if install { "install" } else { scope },
        "project": (!install).then_some(scope),
        "spentN": spent_n,
        "budgetN": budget_n,
        "month": month,
    })
}

/// Each project's spend this month, every router's: in all, and of function time.
const MONTH_SQL: &str = "SELECT project, sum(cost_n), sumIf(cost_n, meter IN ('fn_mb_ms', 'fn_x86_mb_ms')), toUnixTimestamp(min(hour)) \
     FROM usage_1h WHERE hour >= toStartOfMonth(now('UTC')) GROUP BY project FORMAT TabSeparated";

/// MONTH_SQL's answer: each project's total, its function time, and the first hour counted.
fn parse_month(text: &str) -> (HashMap<String, Nanos>, HashMap<String, Nanos>, Option<u64>) {
    let (mut total, mut of_fn, mut since) = (HashMap::new(), HashMap::new(), None::<u64>);
    for line in text.lines() {
        let mut cols = line.split('\t');
        let (Some(project), Some(t)) = (cols.next(), cols.next().and_then(|n| n.trim().parse().ok())) else { continue };
        total.insert(project.to_owned(), t);
        if let Some(f) = cols.next().and_then(|n| n.trim().parse().ok()) {
            of_fn.insert(project.to_owned(), f);
        }
        if let Some(h) = cols.next().and_then(|n| n.trim().parse::<u64>().ok()) {
            since = Some(since.map_or(h, |s| s.min(h)));
        }
    }
    (total, of_fn, since)
}

/// forkrail's own costs (`Files`) added to each project's month to date, if they're `month`'s.
fn add_own(spent: &mut HashMap<String, Nanos>, own: &HashMap<String, Nanos>, own_month: u32, month: u32) {
    if own_month != month {
        return;
    }
    for (project, n) in own {
        *spent.entry(project.clone()).or_default() += n;
    }
}

/// `state/spend/own.json` as the guard writes it (crates/guard/src/own.rs `Own`): its month, and
/// each project's month to date. None if it isn't that.
fn parse_own(text: &[u8]) -> Option<(u32, HashMap<String, Nanos>)> {
    let v: serde_json::Value = serde_json::from_slice(text).ok()?;
    let month = u32::try_from(v.get("month")?.as_u64()?).ok()?;
    let projects = v.get("projects")?.as_object()?.iter().filter_map(|(p, n)| Some((p.clone(), n.as_i64()?.max(0)))).collect();
    Some((month, projects))
}

/// What the guard reads (crates/guard/src/check.rs `Ledger`).
fn ledger_json(spent: &HashMap<String, Nanos>, read_fn: &HashMap<String, Nanos>, since: Option<u64>, month: u32, region: &str) -> String {
    let projects: serde_json::Map<String, serde_json::Value> = spent
        .iter()
        .map(|(p, n)| (p.clone(), json!({ "total_n": n, "fn_n": read_fn.get(p).copied().unwrap_or(0) })))
        .collect();
    json!({ "at": unix_now(), "month": month, "since": since, "region": region, "projects": projects }).to_string()
}
/// And in the last hour.
const HOUR_SQL: &str = "SELECT project, sum(cost_n) FROM usage WHERE time > now('UTC') - 3600 GROUP BY project FORMAT TabSeparated";

fn parse(text: &str) -> HashMap<String, Nanos> {
    text.lines()
        .filter_map(|line| {
            let (project, n) = line.split_once('\t')?;
            Some((project.to_owned(), n.trim().parse().ok()?))
        })
        .collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use forkrail_common::routes::SpendPause;

    const D: Nanos = 1_000_000_000;

    fn table(install: Option<Nanos>, projects: &[(&str, Option<Nanos>, bool, bool)]) -> Table {
        Table {
            install_n: install,
            projects: projects
                .iter()
                .map(|(name, budget, paused, exempt)| {
                    let paused = paused.then(|| SpendPause { at: "2026-11-19T00:00:00Z".into(), reason: "manual".into(), reserved: None });
                    (name.to_string(), ProjectSpend { budget_n: *budget, paused, exempt: *exempt, builds: None })
                })
                .collect(),
        }
    }

    fn spent(of: &[(&str, Nanos)]) -> HashMap<String, Nanos> {
        of.iter().map(|(p, n)| (p.to_string(), *n)).collect()
    }

    #[test]
    fn the_month_and_what_the_guard_reads() {
        let (total, of_fn, since) = parse_month("shop\t5000\t3000\t1795050000\n\t40\t0\t1795046400\nbroken\n");
        assert_eq!((total["shop"], of_fn["shop"], total[""], since), (5000, 3000, 40, Some(1_795_046_400)));
        let j: serde_json::Value = serde_json::from_str(&ledger_json(&total, &of_fn, since, 202611, "us-east-1")).unwrap();
        assert_eq!(j["since"].as_u64(), Some(1_795_046_400));
        assert_eq!((j["month"].as_u64(), j["projects"]["shop"]["total_n"].as_i64(), j["projects"]["shop"]["fn_n"].as_i64()), (Some(202611), Some(5000), Some(3000)));
    }

    #[test]
    fn forkrail_s_own_costs_join_the_month() {
        let text = br#"{"at":1795132800,"month":202611,"parts":{"storage":7,"dashboard_table":2},"projects":{"":7,"forkrail-web":2}}"#;
        let (month, own) = parse_own(text).unwrap();
        assert_eq!((month, own[""], own["forkrail-web"]), (202611, 7, 2));
        assert_eq!(parse_own(b"not json"), None);
        assert_eq!(parse_own(br#"{"month":202611}"#), None);
        let mut s = spent(&[("", 10), ("shop", 5)]);
        add_own(&mut s, &own, 202611, 202611);
        assert_eq!((s[""], s["shop"], s["forkrail-web"]), (17, 5, 2));
        // Last month's figures are left out.
        let mut s = spent(&[("", 10)]);
        add_own(&mut s, &own, 202611, 202612);
        assert_eq!((s[""], s.get("forkrail-web")), (10, None));
        // They count toward the team's budget like the rest.
        let t = table(Some(15 * D), &[("shop", None, false, false)]);
        let mut s = spent(&[("shop", 14 * D)]);
        assert_eq!(decide(&t, &s).paused("shop"), None);
        add_own(&mut s, &HashMap::from([(String::new(), D)]), 202611, 202611);
        assert_eq!(decide(&t, &s).paused("shop"), Some(Why::InstallBudget));
    }

    #[test]
    fn budget_reached_reads_as_the_platforms_webhooks_expect() {
        let p = reached_payload("", 75, 15_004_000_000, 20 * D, 202611);
        assert_eq!((p["scope"].as_str(), p["budgetAmount"].as_f64(), p["currentSpend"].as_f64(), p["thresholdPercent"].as_u64()), (Some("team"), Some(20.0), Some(15.0), Some(75)));
        assert!(p["project"].is_null());
        let p = reached_payload("shop", 200, 10 * D, 5 * D, 202611);
        assert_eq!((p["scope"].as_str(), p["scopeName"].as_str(), p["project"].as_str(), p["spentN"].as_i64()), (Some("project"), Some("shop"), Some("shop"), Some(10 * D)));
    }

    #[test]
    fn a_project_pauses_at_its_own_budget() {
        let t = table(Some(100 * D), &[("a", Some(5 * D), false, false)]);
        assert_eq!(decide(&t, &spent(&[("a", 5 * D - 1)])).paused("a"), None);
        assert_eq!(decide(&t, &spent(&[("a", 5 * D)])).paused("a"), Some(Why::ProjectBudget));
        assert_eq!(decide(&t, &spent(&[("a", 5 * D)])).paused("b"), None, "others go on");
    }

    #[test]
    fn the_install_budget_pauses_every_project_but_the_exempt() {
        let t = table(Some(20 * D), &[("web", None, false, true), ("a", None, false, false)]);
        // The install's own spend (`""`) counts toward it too.
        let v = decide(&t, &spent(&[("a", 15 * D), ("", 5 * D)]));
        assert!(v.install_over);
        assert_eq!(v.paused("a"), Some(Why::InstallBudget));
        assert_eq!(v.paused("never-spent"), Some(Why::InstallBudget), "a project with no spend yet is paused too");
        assert_eq!(v.paused("web"), None, "the dashboard stays up");
        assert_eq!(v.paused(""), None);
    }

    #[test]
    fn an_exempt_project_still_has_its_own_budget() {
        let t = table(Some(20 * D), &[("web", Some(2 * D), false, true)]);
        assert_eq!(decide(&t, &spent(&[("web", 3 * D)])).paused("web"), Some(Why::ProjectBudget));
    }

    #[test]
    fn a_recorded_pause_holds_whatever_the_spend() {
        let t = table(Some(20 * D), &[("a", Some(50 * D), true, false)]);
        assert_eq!(decide(&t, &spent(&[])).paused("a"), Some(Why::Recorded), "raising a budget doesn't resume");
    }

    #[test]
    fn no_install_budget_pauses_nothing_by_it() {
        let t = table(None, &[]);
        let v = decide(&t, &spent(&[("a", 10_000 * D)]));
        assert!(!v.install_over);
        assert_eq!(v.paused("a"), None);
    }

    #[test]
    fn thresholds() {
        assert_eq!(threshold(0, 100), 0);
        assert_eq!(threshold(49, 100), 0);
        assert_eq!(threshold(50, 100), 50);
        assert_eq!(threshold(80, 100), 75);
        assert_eq!(threshold(100, 100), 100);
        assert_eq!(threshold(199, 100), 100);
        assert_eq!(threshold(250, 100), 200);
        assert_eq!(threshold(5000, 100), 1000);
        assert_eq!(threshold(1, 0), 1000, "any spend against a zero budget is over it");
    }

    #[test]
    fn month_to_date_parses() {
        assert_eq!(parse("a\t120\n\t5\nbad line\nb\tx\n"), spent(&[("a", 120), ("", 5)]));
    }
}
