//! Warming a deployment before production is pointed at it.
//!
//! A deployment is a new Lambda version, and a new version has no environments: the first
//! requests after production moves would each wait for one to be created and for the app to start
//! in it, a second or two for a Python API. So whatever moves production (a production deploy, a
//! promote, a rollback) first sends the version a few warm invocations at once, in every region
//! it runs in (`forkrail_common::WarmRequest`). Each lands in an environment of its own, where the
//! runtime starts the app and returns; no request reaches the app. The environments then wait
//! frozen, for five to ten minutes, and the first workers the routers start on that version thaw
//! in 15-25 ms.
//!
//! A warm-up never stops production from moving: what it couldn't do is said in a warning, and
//! the deployment's first requests then start cold, as they did before there were warm-ups.
//!
//! After that the routers keep environments alive for the project's production deployment
//! (crates/router/src/keepwarm.rs), as many per region as the project asks for (`keep_get`,
//! `keep_set`: `projects.<name>.keep_warm` in the routing table, in force within seconds and
//! without a redeploy). The warm-up here stays what it was: enough for the first burst.
//!
//! A deployment built before runtimes knew the warm payload answers it with an error, after its
//! environment was created and its app started in it (the app starts while the environment does),
//! so it's warmed all the same; only whether its app came up isn't known.

use std::time::{Duration, Instant};

use aws_sdk_lambda::error::DisplayErrorContext;
use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::InvocationType;
use forkrail_common::routes::KeepWarm;
use forkrail_common::{WarmReport, WarmRequest};
use serde::Serialize;

use crate::deploy::{Ctx, log, warn};
use crate::state::{DeploymentRecord, ROUTES_KEY};

/// Environments warmed in each region for a deployment that packs requests onto its workers:
/// three workers take thirty requests at once.
const CONCURRENT_ENVIRONMENTS: usize = 3;
/// And for one that gives every request a worker of its own: five requests at once.
const SINGLE_ENVIRONMENTS: usize = 5;
/// The most, whatever is asked: they all run at once, out of the account's Lambda concurrency.
const MAX_ENVIRONMENTS: usize = 50;
/// `FORKRAIL_WARM=N` warms N environments in each region instead; 0 warms none.
pub const COUNT_ENV: &str = "FORKRAIL_WARM";
/// How long each warm invocation stays once ready, so the ones sent together overlap.
const HOLD_MS: u64 = 150;
/// How long a warm-up may take before production moves without it: an environment's start (10 s
/// at most) and its app's (9 s) nearly never come near it.
const LIMIT: Duration = Duration::from_secs(15);

/// What became of one warm invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// The app is up. `new` if the environment was created for this.
    Ready { new: bool, app_start_ms: u64 },
    /// The environment is there, and its app didn't start.
    AppFailed(String),
    /// An older runtime: the environment was created and its app started, with no word on how.
    Older,
    /// Lambda had no room: the function's or the account's concurrency.
    Throttled,
    Failed(String),
}

/// How a deployment's warm-up went, over all its regions.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Warmed {
    /// Environments asked for, and those that are there now.
    pub asked: usize,
    pub ready: usize,
    /// Why the app didn't start, where it didn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_error: Option<String>,
    pub ms: u64,
}

/// How many environments to warm in each region.
fn count(concurrent: bool, asked: Option<&str>) -> usize {
    match asked.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) => n.min(MAX_ENVIRONMENTS),
        None if concurrent => CONCURRENT_ENVIRONMENTS,
        None => SINGLE_ENVIRONMENTS,
    }
}

/// Read what one warm invocation answered.
fn read(function_error: Option<&str>, body: &str) -> Outcome {
    if function_error.is_some() {
        // lambda_runtime's words for a payload the handler's type can't hold: a runtime from
        // before warm-ups. The handler never ran, and the environment is there.
        if body.contains("deserialize") {
            return Outcome::Older;
        }
        return Outcome::Failed(body.chars().take(300).collect());
    }
    match serde_json::from_str::<WarmReport>(body) {
        Ok(r) if r.warm && r.ok => Outcome::Ready { new: r.env_invocations <= 1, app_start_ms: r.app_start_ms },
        Ok(r) if r.warm => Outcome::AppFailed(r.error.unwrap_or_else(|| "no reason given".to_owned())),
        _ => Outcome::Failed(format!("an answer that isn't a warm-up's: {}", body.chars().take(200).collect::<String>())),
    }
}

async fn invoke(client: aws_sdk_lambda::Client, function: String, qualifier: String) -> Outcome {
    let payload = serde_json::to_vec(&WarmRequest { warm: true, hold_ms: HOLD_MS }).expect("serializes");
    let sent = client.invoke().function_name(function).qualifier(qualifier).invocation_type(InvocationType::RequestResponse).payload(Blob::new(payload)).send();
    match tokio::time::timeout(LIMIT, sent).await {
        Err(_) => Outcome::Failed(format!("no answer in {} s", LIMIT.as_secs())),
        Ok(Err(e)) if e.as_service_error().is_some_and(|e| e.is_too_many_requests_exception()) => Outcome::Throttled,
        Ok(Err(e)) => Outcome::Failed(DisplayErrorContext(&e).to_string().chars().take(300).collect()),
        Ok(Ok(out)) => {
            let body = out.payload().map(|b| String::from_utf8_lossy(b.as_ref()).into_owned()).unwrap_or_default();
            read(out.function_error(), &body)
        }
    }
}

/// One region's outcomes in a line, and what to warn of.
fn describe(region: &str, outcomes: &[Outcome], secs: f64) -> (String, Vec<String>) {
    let ready = outcomes.iter().filter(|o| matches!(o, Outcome::Ready { .. })).count();
    let new = outcomes.iter().filter(|o| matches!(o, Outcome::Ready { new: true, .. })).count();
    let older = outcomes.iter().filter(|o| matches!(o, Outcome::Older)).count();
    let slowest = outcomes.iter().filter_map(|o| if let Outcome::Ready { app_start_ms, .. } = o { Some(*app_start_ms) } else { None }).max().unwrap_or(0);
    let mut line = format!("warm-up in {region}: {} of {} environments ready in {secs:.1} s", ready + older, outcomes.len());
    if ready > 0 {
        line.push_str(&match (new, slowest) {
            (0, _) => " (they were there already)".to_owned(),
            (n, 0) => format!(" ({n} new)"),
            (n, ms) => format!(" ({n} new, the app started in {ms} ms)"),
        });
    }
    if older > 0 {
        line.push_str(" (its runtime predates warm-ups: the app was started, not checked)");
    }
    let mut warnings = Vec::new();
    if let Some(error) = outcomes.iter().find_map(|o| if let Outcome::AppFailed(e) = o { Some(e) } else { None }) {
        warnings.push(format!("in {region} the app didn't start in a warm-up: {}", error.trim()));
    }
    let throttled = outcomes.iter().filter(|o| matches!(o, Outcome::Throttled)).count();
    if throttled > 0 {
        warnings.push(format!("in {region} Lambda had no room for {throttled} of the warm-up's environments (the account's concurrency); those requests will start cold"));
    }
    if let Some(error) = outcomes.iter().find_map(|o| if let Outcome::Failed(e) = o { Some(e) } else { None }) {
        warnings.push(format!("in {region} the warm-up didn't go through, so its first requests start cold: {}", error.trim()));
    }
    (line, warnings)
}

/// Warm `function`'s versions, one per region (`region`, `qualifier`), all at once. Never an
/// error: what didn't work is logged as a warning.
pub async fn versions(ctx: &Ctx, function: &str, versions: &[(String, String)], concurrent: bool) -> Warmed {
    let each = count(concurrent, std::env::var(COUNT_ENV).ok().as_deref());
    if each == 0 || versions.is_empty() {
        return Warmed::default();
    }
    let started = Instant::now();
    let regions = versions.iter().map(|(region, qualifier)| {
        let client = ctx.lambda_in(region).client;
        let tasks: Vec<_> = (0..each).map(|_| tokio::spawn(invoke(client.clone(), function.to_owned(), qualifier.clone()))).collect();
        async move {
            let mut outcomes = Vec::with_capacity(tasks.len());
            for task in tasks {
                outcomes.push(task.await.unwrap_or_else(|e| Outcome::Failed(format!("the warm-up stopped: {e}"))));
            }
            (region.as_str(), outcomes, started.elapsed().as_secs_f64())
        }
    });
    // Every region's invocations are in flight already (spawned above); they're only read in turn.
    let mut warmed = Warmed::default();
    for region in regions {
        let (region, outcomes, secs) = region.await;
        let (line, warnings) = describe(region, &outcomes, secs);
        log(&line);
        for warning in &warnings {
            warn(warning);
        }
        warmed.asked += outcomes.len();
        warmed.ready += outcomes.iter().filter(|o| matches!(o, Outcome::Ready { .. } | Outcome::Older)).count();
        if warmed.app_error.is_none() {
            warmed.app_error = outcomes.iter().find_map(|o| if let Outcome::AppFailed(e) = o { Some(e.clone()) } else { None });
        }
    }
    warmed.ms = started.elapsed().as_millis() as u64;
    warmed
}

/// Warm a published deployment, from its record: its home region's version and its replicas'.
pub async fn deployment(ctx: &Ctx, record: &DeploymentRecord) -> Warmed {
    if record.version.is_empty() {
        return Warmed::default();
    }
    let home = record.region.clone().unwrap_or_else(|| ctx.install.home_region.clone());
    let mut all = vec![(home, record.version.clone())];
    all.extend(record.replicas.iter().map(|(region, version)| (region.clone(), version.clone())));
    versions(ctx, &record.function, &all, record.concurrent).await
}

/// How many environments the routers keep alive per region for `project`'s production
/// deployment: its own setting, or None for the routers' default (`FORKRAIL_KEEP_WARM`, 1).
pub async fn keep_get(ctx: &Ctx, project: &str) -> anyhow::Result<Option<KeepWarm>> {
    let routes = ctx.store.get_value::<forkrail_common::routes::Routes>(ROUTES_KEY).await?.unwrap_or_default();
    Ok(routes.projects.get(project).and_then(|p| p.keep_warm))
}

/// Set it: a number (0 keeps none) or `auto`, or None back to the routers' default. The project
/// must exist.
pub async fn keep_set(ctx: &Ctx, project: &str, count: Option<KeepWarm>) -> anyhow::Result<Option<KeepWarm>> {
    crate::deploy::project_record(ctx, project).await?;
    let routes = ctx
        .store
        .update::<forkrail_common::routes::Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            let entry = routes.projects.entry(project.to_owned()).or_default();
            entry.keep_warm = count;
            // An entry that says nothing is left out of the table, as it was before.
            if entry.is_empty() {
                routes.projects.remove(project);
            }
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    Ok(routes.projects.get(project).and_then(|p| p.keep_warm))
}

/// A project's keep-warm setting as the dashboard, the CLI and the API show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct KeepView {
    /// Its own setting: environments per region (0 for none), or `"auto"` (as many as it had in
    /// use at once at its busiest in the last day, at least 1). None: the routers' default.
    pub environments: Option<KeepWarm>,
    /// What applies when it has none of its own (the routers' `FORKRAIL_KEEP_WARM`, 0 unless
    /// the install changed it).
    pub default: u32,
    /// Whether any are kept.
    pub on: bool,
}

/// What the CLI says after a keep-warm change.
pub const KEEP_NOTE: &str = "keep-warm applies within seconds, without a redeploy: the routers keep that many environments ready per region for production";

/// The routers' default, as shown: an install that changes `FORKRAIL_KEEP_WARM` isn't known here.
/// None (the author, 2026-11-18): keep-warm is off unless a project turns it on.
pub const KEEP_DEFAULT: u32 = 0;

pub fn keep_view(environments: Option<KeepWarm>) -> KeepView {
    KeepView { environments, default: KEEP_DEFAULT, on: environments.unwrap_or(KeepWarm::Count(KEEP_DEFAULT)).on() }
}

/// A project's keep-warm setting in words.
pub fn keep_words(count: Option<KeepWarm>) -> String {
    match count {
        None => "default (off: production starts cold after a few idle minutes)".to_owned(),
        Some(KeepWarm::Count(0)) => "off (production starts cold after a few idle minutes)".to_owned(),
        Some(KeepWarm::Count(1)) => "1 environment per region kept ready for production".to_owned(),
        Some(KeepWarm::Count(n)) => format!("{n} environments per region kept ready for production"),
        Some(KeepWarm::Auto) => format!(
            "auto (per region, as many environments kept ready for production as it had in use at once at its busiest in the last day: at least 1, at most {})",
            forkrail_common::routes::KEEP_WARM_MAX
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_warm_as_shown() {
        let view = |k| serde_json::to_value(keep_view(k)).unwrap();
        assert_eq!(view(None), serde_json::json!({"environments": null, "default": 0, "on": false}));
        assert_eq!(view(Some(KeepWarm::Count(0))), serde_json::json!({"environments": 0, "default": 0, "on": false}));
        assert_eq!(view(Some(KeepWarm::Count(1))), serde_json::json!({"environments": 1, "default": 0, "on": true}));
        assert_eq!(view(Some(KeepWarm::Auto)), serde_json::json!({"environments": "auto", "default": 0, "on": true}));
        let back: KeepView = serde_json::from_value(serde_json::json!({"environments": "auto", "default": 0, "on": true})).unwrap();
        assert_eq!(back.environments, Some(KeepWarm::Auto));
        assert!(keep_words(Some(KeepWarm::Auto)).starts_with("auto"));
        assert!(keep_words(Some(KeepWarm::Count(0))).starts_with("off"));
    }

    #[test]
    fn how_many() {
        assert_eq!(count(true, None), 3);
        assert_eq!(count(false, None), 5);
        assert_eq!(count(true, Some("8")), 8);
        assert_eq!(count(true, Some("0")), 0, "0 turns warm-ups off");
        assert_eq!(count(true, Some("5000")), 50);
        assert_eq!(count(false, Some("lots")), 5, "not a number: the default");
    }

    #[test]
    fn answers() {
        let ready = r#"{"warm":true,"ok":true,"env_id":"ab12","env_invocations":1,"app_start_ms":640}"#;
        assert_eq!(read(None, ready), Outcome::Ready { new: true, app_start_ms: 640 });
        let thawed = r#"{"warm":true,"ok":true,"env_id":"ab12","env_invocations":4,"app_start_ms":0}"#;
        assert_eq!(read(None, thawed), Outcome::Ready { new: false, app_start_ms: 0 });
        let failed = r#"{"warm":true,"ok":false,"env_id":"ab12","env_invocations":1,"error":"app exited before listening for HTTP"}"#;
        assert_eq!(read(None, failed), Outcome::AppFailed("app exited before listening for HTTP".into()));
        // What a runtime from before warm-ups says: its handler takes a worker's payload only.
        let older = r#"{"errorType":"&lambda_runtime::deserializer::DeserializeError","errorMessage":"failed to deserialize the incoming data into the function's payload type: missing field `worker_id` at line 1 column 29"}"#;
        assert_eq!(read(Some("Unhandled"), older), Outcome::Older);
        assert!(matches!(read(Some("Unhandled"), r#"{"errorType":"Runtime.ExitError","errorMessage":"exit status 1"}"#), Outcome::Failed(_)));
        // A worker's report, from something that took the payload for a worker's.
        assert!(matches!(read(None, r#"{"worker_id":"w","requests":0}"#), Outcome::Failed(_)));
    }

    #[test]
    fn a_region_in_a_line() {
        let ready = Outcome::Ready { new: true, app_start_ms: 640 };
        let (line, warnings) = describe("us-east-1", &[ready.clone(), ready.clone(), Outcome::Ready { new: true, app_start_ms: 700 }], 1.42);
        assert_eq!(line, "warm-up in us-east-1: 3 of 3 environments ready in 1.4 s (3 new, the app started in 700 ms)");
        assert!(warnings.is_empty());
        let (line, _) = describe("us-west-1", &[Outcome::Ready { new: false, app_start_ms: 0 }, Outcome::Ready { new: false, app_start_ms: 0 }], 0.21);
        assert_eq!(line, "warm-up in us-west-1: 2 of 2 environments ready in 0.2 s (they were there already)");
        let (line, warnings) = describe("us-east-1", &[Outcome::Older, Outcome::Older, Outcome::Throttled], 1.9);
        assert!(line.starts_with("warm-up in us-east-1: 2 of 3 environments ready in 1.9 s") && line.contains("predates warm-ups"), "{line}");
        assert_eq!(warnings.len(), 1);
        let (line, warnings) = describe("us-east-1", &[Outcome::AppFailed("ModuleNotFoundError: No module named 'x'".into()), Outcome::Failed("AccessDenied".into())], 0.4);
        assert!(line.starts_with("warm-up in us-east-1: 0 of 2 environments ready"));
        assert!(warnings[0].contains("didn't start") && warnings[0].contains("ModuleNotFoundError") && warnings[1].contains("AccessDenied"), "{warnings:?}");
    }
}
