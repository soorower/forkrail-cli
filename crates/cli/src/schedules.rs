//! Schedules (`vercel.json`'s `crons` and `schedules`): keeping EventBridge Scheduler in step with
//! each project's production deployment, turning them off and on, running one now, and listing them
//! with their next runs. The router runs them (see `forkrail_common::schedules`).
//!
//! - `sync` after every change of production (publish, promote, rollback) and of a state: one
//!   Scheduler schedule per entry, in the group `forkrail-<project>`, each sending a message to the
//!   queue `forkrail-schedules`. It writes only what changed since the last sync (the project file
//!   keeps what it wrote, `ScheduleStates::synced`) and deletes what production no longer has.
//! - States live in the project file, apart from deployments, so they survive rollbacks: "Disable
//!   all" (`disabled`) and keys turned off one by one (`inactive`). Off is Scheduler's `DISABLED`.
//! - Run now sends the same message straight to the queue, marked manual (no lateness limit).

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use forkrail_common::schedules::{self as sched, Cron, Message, Schedule};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::deploy::{Ctx, check_project, project_record, record};
use crate::state::{self, Project, ScheduleStates, now_rfc3339};

/// What the dashboard, the CLI and the API ask of a project's schedules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SchedulesOp {
    /// The production deployment's schedules, with their states and next runs.
    List { project: String },
    /// Run one now, as Scheduler would, whatever its state.
    Run { project: String, key: String },
    /// Turn one on (`key`), or every one back on ("Enable all": no key).
    Enable {
        project: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Turn one off, or every one ("Disable all").
    Disable {
        project: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Write Scheduler again from production and the states (after a failed sync).
    Sync { project: String },
}

impl SchedulesOp {
    pub fn name(&self) -> &'static str {
        match self {
            SchedulesOp::List { .. } => "list",
            SchedulesOp::Run { .. } => "run",
            SchedulesOp::Enable { .. } => "enable",
            SchedulesOp::Disable { .. } => "disable",
            SchedulesOp::Sync { .. } => "sync",
        }
    }

    pub fn changes(&self) -> bool {
        !matches!(self, SchedulesOp::List { .. })
    }

    pub fn project(&self) -> &str {
        match self {
            SchedulesOp::List { project }
            | SchedulesOp::Run { project, .. }
            | SchedulesOp::Enable { project, .. }
            | SchedulesOp::Disable { project, .. }
            | SchedulesOp::Sync { project } => project,
        }
    }

    pub fn key(&self) -> Option<&str> {
        match self {
            SchedulesOp::Run { key, .. } => Some(key),
            SchedulesOp::Enable { key, .. } | SchedulesOp::Disable { key, .. } => key.as_deref(),
            _ => None,
        }
    }
}

/// A project's schedules, for the dashboard and `schedules ls`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub project: String,
    /// The production deployment they come from; none without one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production: Option<String>,
    /// "Disable all".
    pub disabled: bool,
    pub schedules: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub key: String,
    /// `cron` (from `crons`) or `schedule` (from `schedules`).
    pub kind: String,
    /// A `schedules` entry's name; a cron's path.
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub path: String,
    pub method: String,
    pub expression: String,
    /// The expression in words: "Every day at 05:00".
    pub description: String,
    /// IANA; `UTC` when the entry names none.
    pub timezone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jitter: Option<u32>,
    /// Runs on its own: neither it nor the project is turned off.
    pub active: bool,
    /// Its own switch, whatever "Disable all" says: off when it was turned off by itself.
    pub on: bool,
    /// Its next runs (RFC 3339, UTC), when active.
    pub next: Vec<String>,
}

pub async fn run(ctx: &Ctx, op: SchedulesOp, actor: Option<&str>) -> Result<Value> {
    check_project(op.project())?;
    Ok(match op {
        SchedulesOp::List { project } => json!(list(ctx, &project).await?),
        SchedulesOp::Run { project, key } => json!(run_now(ctx, &project, &key, actor).await?),
        SchedulesOp::Enable { project, key } => json!(set_state(ctx, &project, key.as_deref(), true).await?),
        SchedulesOp::Disable { project, key } => json!(set_state(ctx, &project, key.as_deref(), false).await?),
        SchedulesOp::Sync { project } => {
            let synced = sync(ctx, &project).await?;
            json!({"project": project, "written": synced.written, "deleted": synced.deleted, "schedules": list(ctx, &project).await?})
        }
    })
}

/// The production deployment's schedules, and the project.
async fn production(ctx: &Ctx, project: &str) -> Result<(Project, Option<String>, Vec<Schedule>)> {
    let p = project_record(ctx, project).await?;
    let schedules = match &p.production {
        Some(id) => record(ctx, id).await?.schedules,
        None => Vec::new(),
    };
    let production = p.production.clone();
    Ok((p, production, schedules))
}

pub async fn list(ctx: &Ctx, project: &str) -> Result<View> {
    let (p, production, schedules) = production(ctx, project).await?;
    Ok(view(&p, production, &schedules, Utc::now()))
}

fn view(p: &Project, production: Option<String>, schedules: &[Schedule], now: DateTime<Utc>) -> View {
    let schedules = schedules
        .iter()
        .map(|s| {
            let active = p.schedules.active(&s.key);
            Item {
                key: s.key.clone(),
                kind: if s.name.is_some() { "schedule" } else { "cron" }.to_owned(),
                label: s.label().to_owned(),
                name: s.name.clone(),
                path: s.path.clone(),
                method: s.method.clone(),
                expression: s.expression.clone(),
                description: describe(&s.expression),
                timezone: s.timezone.clone().unwrap_or_else(|| "UTC".to_owned()),
                jitter: s.jitter,
                active,
                on: !p.schedules.inactive.iter().any(|k| k == &s.key),
                next: if active { next_runs(s, now, 3).iter().map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).collect() } else { Vec::new() },
            }
        })
        .collect();
    View { project: p.name.clone(), production, disabled: p.schedules.disabled, schedules }
}

/// What a sync wrote and deleted.
#[derive(Debug, Default, Serialize)]
pub struct Synced {
    pub written: Vec<String>,
    pub deleted: Vec<String>,
}

fn scheduler(ctx: &Ctx) -> aws_sdk_scheduler::Client {
    let config = aws_sdk_scheduler::config::Builder::from(&ctx.config).region(aws_sdk_scheduler::config::Region::new(ctx.install.home_region.clone())).build();
    aws_sdk_scheduler::Client::from_conf(config)
}

fn sqs(ctx: &Ctx) -> aws_sdk_sqs::Client {
    let config = aws_sdk_sqs::config::Builder::from(&ctx.config).region(aws_sdk_sqs::config::Region::new(ctx.install.home_region.clone())).build();
    aws_sdk_sqs::Client::from_conf(config)
}

/// One schedule as Scheduler is given it, and as a fingerprint of that, to tell what changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub name: String,
    pub expression: String,
    pub timezone: String,
    /// Minutes; 0 is off (runs within its minute).
    pub window: u32,
    pub enabled: bool,
    /// The message, with Scheduler's placeholders for the time and the run.
    pub input: String,
}

impl Desired {
    pub fn of(project: &str, s: &Schedule, enabled: bool) -> Result<Self> {
        let cron = Cron::parse(&s.expression).map_err(anyhow::Error::msg)?;
        // Scheduler fills these in for each run; the router reads them back.
        let input = json!({
            "v": 1,
            "project": project,
            "key": s.key,
            "scheduled": "<aws.scheduler.scheduled-time>",
            "run": "<aws.scheduler.execution-id>",
            "late_s": s.late_s(),
        })
        .to_string();
        Ok(Desired {
            name: s.key.clone(),
            expression: cron.aws(),
            timezone: s.timezone.clone().unwrap_or_else(|| "UTC".to_owned()),
            window: s.jitter.unwrap_or(0),
            enabled,
            input,
        })
    }

    pub fn fingerprint(&self) -> String {
        format!("{}|{}|{}|{}|{}", self.expression, self.timezone, self.window, self.enabled, self.input)
    }
}

/// What production and the states ask Scheduler to have.
fn desired(project: &str, states: &ScheduleStates, schedules: &[Schedule]) -> Result<Vec<Desired>> {
    schedules.iter().map(|s| Desired::of(project, s, states.active(&s.key))).collect()
}

/// What to write and delete, given what Scheduler has (`existing`, names) and what was last written.
fn plan<'a>(want: &'a [Desired], existing: &[String], synced: &BTreeMap<String, String>) -> (Vec<&'a Desired>, Vec<String>) {
    let write = want.iter().filter(|d| !existing.contains(&d.name) || synced.get(&d.name) != Some(&d.fingerprint())).collect();
    let delete = existing.iter().filter(|name| !want.iter().any(|d| &d.name == *name)).cloned().collect();
    (write, delete)
}

/// Make the project's Scheduler group match its production deployment and its states.
pub async fn sync(ctx: &Ctx, project: &str) -> Result<Synced> {
    let (p, _, schedules) = production(ctx, project).await?;
    let want = desired(project, &p.schedules, &schedules)?;
    let client = scheduler(ctx);
    let group = sched::group(project);
    let existing = list_names(&client, &group).await?;
    if want.is_empty() && existing.as_ref().is_none_or(Vec::is_empty) {
        if !p.schedules.synced.is_empty() {
            save_synced(ctx, project, BTreeMap::new()).await?;
        }
        return Ok(Synced::default());
    }
    if existing.is_none() {
        create_group(ctx, &client, &group, project).await?;
    }
    let existing = existing.unwrap_or_default();
    let (write, delete) = plan(&want, &existing, &p.schedules.synced);
    let mut synced: BTreeMap<String, String> = p.schedules.synced.iter().filter(|(k, _)| want.iter().any(|d| &d.name == *k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut out = Synced::default();
    for d in write {
        put(ctx, &client, &group, d, existing.contains(&d.name)).await.with_context(|| format!("schedule {}", d.name))?;
        synced.insert(d.name.clone(), d.fingerprint());
        out.written.push(d.name.clone());
    }
    for name in delete {
        match client.delete_schedule().group_name(&group).name(&name).send().await {
            Ok(_) => {}
            Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_not_found_exception()) => {}
            Err(e) => bail!("deleting schedule {name}: {}", aws_sdk_scheduler::error::DisplayErrorContext(&e)),
        }
        synced.remove(&name);
        out.deleted.push(name);
    }
    if synced != p.schedules.synced {
        save_synced(ctx, project, synced).await?;
    }
    Ok(out)
}

/// After production moved: sync, and say so; a failure is a warning (production has moved already),
/// with how to try again.
pub async fn after_production_change(ctx: &Ctx, project: &str) {
    match sync(ctx, project).await {
        Ok(s) if s.written.is_empty() && s.deleted.is_empty() => {}
        Ok(s) => crate::deploy::log(&format!("schedules: {} written, {} removed", s.written.len(), s.deleted.len())),
        Err(e) => crate::deploy::warn(&format!("schedules weren't updated: {e:#}. `forkrail schedules sync {project}` tries again")),
    }
}

async fn save_synced(ctx: &Ctx, project: &str, synced: BTreeMap<String, String>) -> Result<()> {
    ctx.store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = current.context("no such project")?;
            p.schedules.synced = synced.clone();
            Ok(p)
        })
        .await?;
    Ok(())
}

/// Delete the project's Scheduler group, and with it every schedule in it (Scheduler deletes them
/// soon after). True if there was one.
pub async fn delete_group(ctx: &Ctx, project: &str) -> Result<bool> {
    let group = sched::group(project);
    match scheduler(ctx).delete_schedule_group().name(&group).send().await {
        Ok(_) => Ok(true),
        Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_not_found_exception()) => Ok(false),
        // Being deleted already (a delete tried again).
        Err(e) if e.as_service_error().is_some_and(|e| e.is_conflict_exception()) => Ok(true),
        Err(e) => Err(scheduler_error(e, "deleting the schedule group")),
    }
}

/// The names of the group's schedules; none if the group doesn't exist.
async fn list_names(client: &aws_sdk_scheduler::Client, group: &str) -> Result<Option<Vec<String>>> {
    let mut names = Vec::new();
    let mut token = None;
    loop {
        let page = match client.list_schedules().group_name(group).set_next_token(token).send().await {
            Ok(page) => page,
            Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_not_found_exception()) => return Ok(None),
            Err(e) => return Err(scheduler_error(e, "listing schedules")),
        };
        names.extend(page.schedules().iter().filter_map(|s| s.name().map(str::to_owned)));
        token = page.next_token().map(str::to_owned);
        if token.is_none() {
            // An empty group answers the same as one with nothing: tell them apart.
            if names.is_empty() {
                match client.get_schedule_group().name(group).send().await {
                    Ok(_) => {}
                    Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_not_found_exception()) => return Ok(None),
                    Err(e) => return Err(scheduler_error(e, "reading the schedule group")),
                }
            }
            return Ok(Some(names));
        }
    }
}

async fn create_group(ctx: &Ctx, client: &aws_sdk_scheduler::Client, group: &str, project: &str) -> Result<()> {
    use aws_sdk_scheduler::types::Tag;
    let tag = |k: &str, v: &str| Tag::builder().key(k).value(v).build().expect("key and value set");
    let tags = vec![tag(forkrail_common::install::TAG_KEY, forkrail_common::install::TAG_VALUE), tag(forkrail_common::install::PROJECT_TAG, project)];
    match client.create_schedule_group().name(group).set_tags(Some(tags)).send().await {
        Ok(_) => Ok(()),
        Err(e) if e.as_service_error().is_some_and(|e| e.is_conflict_exception()) => Ok(()),
        Err(e) => Err(scheduler_error(e, "creating the schedule group")).with_context(|| format!("is the forkrail-schedules stack installed in {}?", ctx.install.home_region)),
    }
}

/// Create or update one schedule.
async fn put(ctx: &Ctx, client: &aws_sdk_scheduler::Client, group: &str, d: &Desired, exists: bool) -> Result<()> {
    use aws_sdk_scheduler::types::{ActionAfterCompletion, FlexibleTimeWindow, FlexibleTimeWindowMode, RetryPolicy, ScheduleState, Target};
    let window = match d.window {
        0 => FlexibleTimeWindow::builder().mode(FlexibleTimeWindowMode::Off).build()?,
        m => FlexibleTimeWindow::builder().mode(FlexibleTimeWindowMode::Flexible).maximum_window_in_minutes(m as i32).build()?,
    };
    // No retries: a run that can't be delivered is skipped, as on Vercel.
    let retry = RetryPolicy::builder().maximum_retry_attempts(0).maximum_event_age_in_seconds(60).build();
    let target = Target::builder().arn(ctx.install.schedule_queue_arn()).role_arn(ctx.install.scheduler_role_arn()).input(&d.input).retry_policy(retry).build()?;
    let state = if d.enabled { ScheduleState::Enabled } else { ScheduleState::Disabled };
    let description = "forkrail schedule (written by forkrail; changes here are overwritten)";
    macro_rules! send {
        ($call:expr) => {
            $call
                .group_name(group)
                .name(&d.name)
                .schedule_expression(&d.expression)
                .schedule_expression_timezone(&d.timezone)
                .flexible_time_window(window.clone())
                .target(target.clone())
                .state(state.clone())
                .action_after_completion(ActionAfterCompletion::None)
                .description(description)
                .send()
                .await
        };
    }
    if exists {
        match send!(client.update_schedule()) {
            Ok(_) => return Ok(()),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_not_found_exception()) => {}
            Err(e) => return Err(scheduler_error(e, "updating")),
        }
    }
    match send!(client.create_schedule()) {
        Ok(_) => Ok(()),
        Err(e) if e.as_service_error().is_some_and(|e| e.is_conflict_exception()) => {
            send!(client.update_schedule()).map(|_| ()).map_err(|e| scheduler_error(e, "updating"))
        }
        Err(e) => Err(scheduler_error(e, "creating")),
    }
}

/// The service's own one-line message when there is one ("The execution role you provide must
/// allow…"), else the whole chain.
fn scheduler_error<E, R>(e: aws_sdk_scheduler::error::SdkError<E, R>, doing: &str) -> anyhow::Error
where
    E: std::error::Error + aws_sdk_scheduler::error::ProvideErrorMetadata + Send + Sync + 'static,
    R: std::fmt::Debug,
{
    match e.as_service_error().and_then(|s| Some(format!("{}: {}", s.code()?, s.message()?))) {
        Some(message) => anyhow::anyhow!("{doing}: {message}"),
        None => anyhow::anyhow!("{doing}: {}", aws_sdk_scheduler::error::DisplayErrorContext(&e)),
    }
}

/// Turn one schedule (`key`) or the project's (no key) on or off, then sync.
pub async fn set_state(ctx: &Ctx, project: &str, key: Option<&str>, on: bool) -> Result<View> {
    if let Some(key) = key {
        let (_, _, schedules) = production(ctx, project).await?;
        if !schedules.iter().any(|s| s.key == key) {
            bail!("{project}'s production deployment has no schedule {key:?} (`forkrail schedules ls {project}` lists them)");
        }
    }
    ctx.store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = current.context("no such project")?;
            match key {
                None => {
                    p.schedules.disabled = !on;
                    // "Enable all" means all: each one's own switch too.
                    if on {
                        p.schedules.inactive.clear();
                    }
                }
                Some(key) => {
                    p.schedules.inactive.retain(|k| k != key);
                    if !on {
                        p.schedules.inactive.push(key.to_owned());
                    }
                }
            }
            Ok(p)
        })
        .await?;
    sync(ctx, project).await?;
    list(ctx, project).await
}

/// What Run now answers.
#[derive(Debug, Serialize, Deserialize)]
pub struct Started {
    pub project: String,
    pub key: String,
    pub run: String,
}

/// Run one of production's schedules now: its message, straight to the queue.
pub async fn run_now(ctx: &Ctx, project: &str, key: &str, actor: Option<&str>) -> Result<Started> {
    let (_, production, schedules) = production(ctx, project).await?;
    if production.is_none() {
        bail!("{project} has no production deployment to run a schedule on");
    }
    let Some(s) = schedules.iter().find(|s| s.key == key || s.name.as_deref() == Some(key)) else {
        bail!("{project}'s production deployment has no schedule {key:?} (`forkrail schedules ls {project}` lists them)");
    };
    let run = format!("manual-{}", forkrail_common::random_hex(8));
    let message = Message { v: 1, project: project.to_owned(), key: s.key.clone(), scheduled: now_rfc3339(), run: run.clone(), late_s: 0, manual: true };
    sqs(ctx)
        .send_message()
        .queue_url(ctx.install.schedule_queue_url())
        .message_body(serde_json::to_string(&message)?)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("sending the run: {}", aws_sdk_sqs::error::DisplayErrorContext(&e)))
        .with_context(|| format!("is the forkrail-schedules stack installed in {}?", ctx.install.home_region))?;
    if let Some(actor) = actor {
        eprintln!("schedule {key} of {project} run by {actor} ({run})");
    }
    Ok(Started { project: project.to_owned(), key: s.key.clone(), run })
}

/// The next `n` runs after `after`, in UTC: days in the schedule's zone, each of its times that
/// exists that day (a time skipped by a daylight-saving change doesn't run; one that happens twice
/// runs at the first).
pub fn next_runs(s: &Schedule, after: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
    let Ok(cron) = Cron::parse(&s.expression) else { return Vec::new() };
    let tz: chrono_tz::Tz = s.timezone.as_deref().and_then(|t| t.parse().ok()).unwrap_or(chrono_tz::UTC);
    let times = cron.times();
    let mut out = Vec::with_capacity(n);
    let mut day: NaiveDate = after.with_timezone(&tz).date_naive();
    for _ in 0..(4 * 366) {
        let weekday = day.weekday().num_days_from_sunday() as u8;
        if cron.runs_on(day.month() as u8, day.day() as u8, weekday) {
            for &t in &times {
                let Some(local) = day.and_hms_opt(t / 60, t % 60, 0) else { continue };
                let Some(at) = tz.from_local_datetime(&local).earliest() else { continue };
                let at = at.with_timezone(&Utc);
                if at > after {
                    out.push(at);
                    if out.len() == n {
                        return out;
                    }
                }
            }
        }
        match day.succ_opt() {
            Some(next) => day = next,
            None => break,
        }
    }
    out
}

/// A cron expression in words, for the common shapes; the expression itself otherwise.
pub fn describe(expression: &str) -> String {
    let Ok(cron) = Cron::parse(expression) else { return expression.to_owned() };
    let f: Vec<&str> = expression.split(' ').collect();
    let all_days = f[2] == "*" && f[3] == "*" && f[4] == "*";
    let single = |s: &std::collections::BTreeSet<u8>| (s.len() == 1).then(|| *s.iter().next().expect("one"));
    let days_words = || -> Option<String> {
        if all_days {
            return Some("every day".to_owned());
        }
        if f[3] != "*" {
            return None;
        }
        if f[4] == "1-5" {
            return Some("on weekdays".to_owned());
        }
        if f[4] == "0,6" || f[4] == "6,0" {
            return Some("on weekends".to_owned());
        }
        const NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
        if cron.dow_set {
            let names: Vec<&str> = cron.weekdays.iter().map(|&d| NAMES[d as usize]).collect();
            return Some(format!("on {}", join_words(&names)));
        }
        if let Some(d) = single(&cron.days) {
            return Some(format!("on day {d} of the month"));
        }
        None
    };
    match (f[0], f[1]) {
        ("*", "*") if all_days => return "Every minute".to_owned(),
        (m, "*") if all_days && m.starts_with("*/") => return format!("Every {} minutes", &m[2..]),
        (_, "*") if all_days && single(&cron.minutes).is_some() => {
            return format!("Every hour at minute {}", single(&cron.minutes).expect("one"));
        }
        _ => {}
    }
    if let (Some(m), Some(days)) = (single(&cron.minutes), days_words()) {
        if let Some(h) = single(&cron.hours) {
            let mut s = format!("{days} at {h:02}:{m:02}");
            s[..1].make_ascii_uppercase();
            return s;
        }
        if f[1].starts_with("*/") && all_days {
            return format!("Every {} hours at minute {m}", &f[1][2..]);
        }
    }
    expression.to_owned()
}

fn join_words(words: &[&str]) -> String {
    match words {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Print `schedules ls`.
pub fn print_view(view: &View) {
    match &view.production {
        None => println!("{}: no production deployment, so no schedules run", view.project),
        Some(id) if view.schedules.is_empty() => println!("{}: production ({id}) has no schedules", view.project),
        Some(id) => {
            println!("{}: {} schedules from production ({id}){}", view.project, view.schedules.len(), if view.disabled { ", all turned off" } else { "" });
            for s in &view.schedules {
                let state = if s.active { "on " } else { "off" };
                let next = s.next.first().map(String::as_str).unwrap_or("-");
                let zone = if s.timezone == "UTC" { String::new() } else { format!(" ({})", s.timezone) };
                println!("  {state} {:<24} {} {}  {:<16} {}{zone}  next {next}", s.label, s.method, s.path, s.expression, s.description);
                if s.label != s.key {
                    println!("      key {}", s.key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(expression: &str, timezone: Option<&str>, jitter: Option<u32>) -> Schedule {
        Schedule { key: "k".into(), name: None, path: "/p".into(), method: "GET".into(), expression: expression.into(), timezone: timezone.map(Into::into), jitter }
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn next_runs_in_zones() {
        let after = at("2026-11-14T10:00:30Z"); // a Saturday
        let runs = next_runs(&s("* * * * *", None, None), after, 2);
        assert_eq!(runs, vec![at("2026-11-14T10:01:00Z"), at("2026-11-14T10:02:00Z")]);
        let runs = next_runs(&s("0 9 * * 1-5", Some("America/New_York"), None), after, 2);
        assert_eq!(runs, vec![at("2026-11-16T14:00:00Z"), at("2026-11-17T14:00:00Z")]); // EST, UTC-5
        let runs = next_runs(&s("0 9 * * *", Some("America/New_York"), None), at("2026-07-01T00:00:00Z"), 1);
        assert_eq!(runs, vec![at("2026-07-01T13:00:00Z")]); // EDT, UTC-4
        let runs = next_runs(&s("0 0 29 2 *", None, None), after, 1);
        assert_eq!(runs, vec![at("2028-02-29T00:00:00Z")]);
        // 02:30 doesn't exist in New York on 2027-03-14: that day is skipped.
        let runs = next_runs(&s("30 2 * * *", Some("America/New_York"), None), at("2027-03-13T12:00:00Z"), 2);
        assert_eq!(runs, vec![at("2027-03-15T06:30:00Z"), at("2027-03-16T06:30:00Z")]);
    }

    #[test]
    fn words() {
        assert_eq!(describe("* * * * *"), "Every minute");
        assert_eq!(describe("*/5 * * * *"), "Every 5 minutes");
        assert_eq!(describe("15 * * * *"), "Every hour at minute 15");
        assert_eq!(describe("0 5 * * *"), "Every day at 05:00");
        assert_eq!(describe("30 9 * * 1-5"), "On weekdays at 09:30");
        assert_eq!(describe("0 8 * * 1,3"), "On Monday and Wednesday at 08:00");
        assert_eq!(describe("0 0 1 * *"), "On day 1 of the month at 00:00");
        assert_eq!(describe("0 */6 * * *"), "Every 6 hours at minute 0");
        assert_eq!(describe("0 0 1 6 *"), "0 0 1 6 *");
        assert_eq!(describe("0,30 9 * * *"), "0,30 9 * * *");
    }

    #[test]
    fn what_a_sync_writes() {
        let mut states = ScheduleStates::default();
        let a = Schedule { key: "a".into(), name: Some("a".into()), path: "/a".into(), method: "POST".into(), expression: "0 9 * * 1-5".into(), timezone: Some("Europe/Berlin".into()), jitter: Some(5) };
        let c = s("*/10 * * * *", None, None);
        let want = desired("shop", &states, &[a.clone(), c.clone()]).unwrap();
        assert_eq!(want[0].expression, "cron(0 9 ? * 2-6 *)");
        assert_eq!((want[0].timezone.as_str(), want[0].window, want[0].enabled), ("Europe/Berlin", 5, true));
        assert_eq!((want[1].timezone.as_str(), want[1].window), ("UTC", 0));
        let input: Value = serde_json::from_str(&want[1].input).unwrap();
        assert_eq!(input["scheduled"], "<aws.scheduler.scheduled-time>");
        assert_eq!(input["run"], "<aws.scheduler.execution-id>");
        assert_eq!(input["late_s"], 600);
        assert_eq!(input["project"], "shop");

        // Nothing there yet: write both. Written and unchanged: nothing. One gone from production: delete it.
        let (write, delete) = plan(&want, &[], &BTreeMap::new());
        assert_eq!((write.len(), delete.len()), (2, 0));
        let synced: BTreeMap<String, String> = want.iter().map(|d| (d.name.clone(), d.fingerprint())).collect();
        let names = vec!["a".to_owned(), "k".to_owned()];
        let (write, delete) = plan(&want, &names, &synced);
        assert!(write.is_empty() && delete.is_empty());
        let (write, delete) = plan(&want[..1], &names, &synced);
        assert_eq!((write.len(), delete), (0, vec!["k".to_owned()]));
        // Deleted by hand in Scheduler: written again.
        let (write, _) = plan(&want, &names[..1], &synced);
        assert_eq!(write.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), vec!["k"]);

        // Turned off: that one is rewritten as disabled; "Disable all" turns both off.
        states.inactive.push("a".into());
        let off = desired("shop", &states, &[a.clone(), c.clone()]).unwrap();
        assert!(!off[0].enabled && off[1].enabled);
        let (write, _) = plan(&off, &names, &synced);
        assert_eq!(write.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), vec!["a"]);
        states.disabled = true;
        assert!(desired("shop", &states, &[a, c]).unwrap().iter().all(|d| !d.enabled));
    }

    #[test]
    fn cron_secret_must_fit_a_header() {
        use crate::publish::check_cron_secret;
        assert!(check_cron_secret(None).is_ok());
        assert!(check_cron_secret(Some("s3cret-Value_123")).is_ok());
        for bad in ["s3cret\n", " s3cret", "s3cret ", "a\u{7f}b", "line\r\nbreak"] {
            assert!(check_cron_secret(Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn views() {
        let mut p = Project { name: "shop".into(), ..Default::default() };
        p.schedules.inactive.push("daily".into());
        let schedules = vec![
            s("0 5 * * *", None, None),
            Schedule { key: "daily".into(), name: Some("daily".into()), path: "/d".into(), method: "POST".into(), expression: "0 9 * * *".into(), timezone: Some("Europe/Berlin".into()), jitter: Some(1) },
        ];
        let v = view(&p, Some("abc".into()), &schedules, at("2026-11-14T10:00:00Z"));
        assert_eq!((v.schedules[0].kind.as_str(), v.schedules[0].label.as_str(), v.schedules[0].active), ("cron", "/p", true));
        assert_eq!(v.schedules[0].next[0], "2026-11-15T05:00:00Z");
        assert_eq!(v.schedules[0].description, "Every day at 05:00");
        assert_eq!((v.schedules[1].kind.as_str(), v.schedules[1].active, v.schedules[1].on, v.schedules[1].next.len()), ("schedule", false, false, 0));
        p.schedules.disabled = true;
        let v = view(&p, Some("abc".into()), &schedules, at("2026-11-14T10:00:00Z"));
        assert!(!v.schedules[0].active && v.schedules[0].on && v.schedules[0].next.is_empty());
        let op: SchedulesOp = serde_json::from_value(json!({"op": "disable", "project": "shop"})).unwrap();
        assert!(matches!(op, SchedulesOp::Disable { key: None, .. }) && op.changes());
        let op: SchedulesOp = serde_json::from_value(json!({"op": "run", "project": "shop", "key": "daily"})).unwrap();
        assert_eq!((op.name(), op.key()), ("run", Some("daily")));
    }
}
