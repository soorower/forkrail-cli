//! `forkrail-control`: the install's control function. The dashboard asks it to change things
//! (promote, roll back, redeploy, a project's function settings, its env vars) and to read a
//! deployment's files, and it does them with the same code as the CLI (the `forkrail_cli` library),
//! under its own narrow role (deploy/install/control.yaml).
//!
//! Nothing reaches it but an IAM-signed Lambda Invoke: it has no URL, and its resource policy lets
//! in only the dashboard's role (and the account's administrators, through IAM). It does a fixed
//! list of things, so a dashboard that went wrong can press the dashboard's buttons and nothing
//! else: it can't name a role for a function, read a production or preview value back, or reach
//! outside forkrail's own projects. Env var values come in, go to SSM, and are never logged; only
//! Development's, which no deployment uses, are returned (`env_pull`, for `forkrail env pull`).
//!
//! An invocation is one JSON object, `{"action": "...", "actor": "who asked", ...}`, answered with
//! `{"ok": true, "result": ...}` or `{"ok": false, "error": "...", "code": ...}`.
//!
//! It is also the one place that holds the GitHub App's private key (connect.rs): it lists the
//! repositories the app reaches, connects one to a project, and builds what's pushed to it. The
//! dashboard checks a push's signature (it has the webhook secret) and hands it over; the files
//! built are always the ones GitHub gives for that commit of the project's own repository. The
//! same goes for a branch deleted there (its previews are removed) and a pull request opened
//! (it gets the project's comment).
//!
//! People signed in from a terminal (`forkrail login`) reach it the same way, through the dashboard's
//! API: it signs the address their source is uploaded to (`upload_url`) and starts the build
//! (`deploy`), so a deploy needs no AWS credentials. Such a request says `"via": "cli"`.
//!
//! It also listens on the builds topic (`forkrail-builds`), where Lambda reports how each of the
//! builder's invocations ended: it publishes what was built, or records why not
//! (forkrail_cli::remote::finished). Those notices are checked against the deployment's record, since
//! the code a build runs can publish to the topic too.
//!
//! The dashboard says who is asking and with what role, and checks the role itself. A request that
//! names a role is checked against it here too (`permitted`): a developer's stays within previews
//! and a viewer's changes nothing, whatever a dashboard that went wrong lets through.
//!
//! `forkrail-control --once` answers one request from stdin and exits, with the credentials of
//! whoever runs it: for trying a change before it's released.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use aws_config::BehaviorVersion;
use forkrail_cli::deploy::{self, Ctx, PreviewEnv, PreviewEnvDiffers, Redeploy, Redeployed};
use forkrail_cli::project::{self, Change};
use forkrail_cli::connect::{self, Api, Connect, Create, PullRequest, Push, Pushed};
use forkrail_cli::remote::{PublishOptions, Start};
use forkrail_cli::source::Archive;
use forkrail_cli::domains::{self, DomainsOp};
use forkrail_cli::purge::{self, CacheOp};
use forkrail_cli::schedules::{self, SchedulesOp};
use forkrail_cli::protection::{self, ProtectionOp};
use forkrail_cli::spend::{self, SpendOp};
use forkrail_cli::skew;
use forkrail_cli::firewall::{self, FirewallOp};
use forkrail_cli::retention::{self, RetentionOp};
use forkrail_cli::project_routing::{self, RedirectsOp, RoutesOp};
use forkrail_cli::deploy_hooks::{self, DeployHooksOp};
use forkrail_cli::{remote, source, templates};
use forkrail_common::install::BUILDS_TOPIC;
use forkrail_cli::state::{EnvTarget, GitCommit, Target, Via};
use forkrail_cli::env as env_vars;
use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Invocation {
    /// Who asked: the dashboard's signed-in user. Recorded as a redeploy's deployer, and logged.
    #[serde(default)]
    actor: Option<String>,
    /// How it was asked: the dashboard's buttons, unless the dashboard says it came from the
    /// person's terminal (`cli`). Recorded on what's deployed.
    #[serde(default)]
    via: Option<Via>,
    /// The asker's role in the team, as the dashboard knows it: `owner`, `member`, `developer` or
    /// `viewer`. Without one nothing is checked here (a push has no person, and dashboards from
    /// before this sent none).
    #[serde(default)]
    role: Option<String>,
    #[serde(flatten)]
    request: Request,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    /// Make a deployment production; a preview whose env vars differ is handled as `preview_env`
    /// says (refused by default).
    Promote {
        id: String,
        #[serde(default = "refuse")]
        preview_env: PreviewEnv,
    },
    /// Point production back at `to`, or at the one before it.
    Rollback { project: String, to: Option<String> },
    /// Make a deployment again, with today's env vars and settings; as production if `production`,
    /// else as the deployment was. One built in the builder is built again from its source, as on
    /// Vercel; any other's build is published again.
    Redeploy {
        id: String,
        #[serde(default)]
        production: bool,
        /// When it's built again: without the project's build cache.
        #[serde(default)]
        no_cache: bool,
        /// A production deployment that leaves production where it is.
        #[serde(default)]
        skip_domain: bool,
    },
    /// A signed address for a project's source, zipped: `bytes` long, with that SHA-256. S3 takes
    /// that zip there and nothing else.
    UploadUrl { project: String, sha256: String, bytes: u64 },
    /// Build an uploaded source in the builder and publish it: `forkrail deploy` from someone
    /// signed in. The actor is recorded as the deployer.
    Deploy {
        project: String,
        #[serde(default)]
        production: bool,
        upload: Archive,
        /// The commit their folder was at, as their CLI read it.
        #[serde(default)]
        git: Option<GitCommit>,
        #[serde(default)]
        memory: Option<u32>,
        /// Concurrent compute, under the key the project's setting is kept under.
        #[serde(default, rename = "fluid")]
        concurrent: Option<bool>,
        #[serde(default)]
        skip_domain: bool,
        #[serde(default)]
        same_source: bool,
        #[serde(default)]
        no_cache: bool,
    },
    /// Stop a deployment building in the builder.
    BuildCancel { id: String },
    /// Change a project's function settings for its next deployments: absent leaves one as it is,
    /// null clears it.
    Settings {
        project: String,
        #[serde(default, deserialize_with = "present")]
        memory_mb: Option<Option<u32>>,
        #[serde(default, deserialize_with = "present")]
        max_duration_s: Option<Option<u64>>,
        /// Concurrent compute, under the key the project's setting is kept under.
        #[serde(default, deserialize_with = "present", rename = "fluid")]
        concurrent: Option<Option<bool>>,
        /// With concurrent compute on, the most requests one worker takes at once (2-50); null
        /// the router's default. Applied at once to its deployments that run concurrent compute.
        #[serde(default, deserialize_with = "present")]
        max_inflight: Option<Option<u32>>,
        /// The framework preset, a Vercel slug; null finds it from the project's files.
        #[serde(default, deserialize_with = "present")]
        framework: Option<Option<String>>,
        /// "Auto-assign custom domains": off, production deployments wait for Promote.
        #[serde(default, deserialize_with = "present")]
        auto_assign_domains: Option<Option<bool>>,
        /// Skew protection (the CLI's skew.rs), applied at once: on or off; the maximum age in
        /// seconds (null: the default, 12 hours); a threshold deployment (null: none); the sites
        /// allowed to send skew-protected requests from elsewhere.
        #[serde(default)]
        skew_protection: Option<bool>,
        #[serde(default, deserialize_with = "present")]
        skew_max_age_s: Option<Option<u64>>,
        #[serde(default, deserialize_with = "present")]
        skew_threshold: Option<Option<String>>,
        #[serde(default)]
        skew_allowed_domains: Option<Vec<String>>,
        /// Keep-warm (the CLI's warm.rs), applied within seconds: environments the routers keep
        /// ready per region for its production deployment; 0 none, "auto" as many as its busiest
        /// moment of the last day needed (at least 1), null the routers' default.
        #[serde(default, deserialize_with = "present")]
        keep_warm: Option<Option<forkrail_common::routes::KeepWarm>>,
        /// Web Analytics (the CLI's web_analytics.rs), applied within seconds: on or off.
        #[serde(default)]
        analytics: Option<bool>,
        /// The regions its functions run in from its next deployment, by AWS's or Vercel's names
        /// (the CLI's regions.rs); null for none: where production runs, the home region for a new
        /// project.
        #[serde(default, deserialize_with = "present")]
        regions: Option<Option<Vec<String>>>,
    },
    /// Set env vars (name → value) for each target, or with `branch` (and no targets), for that
    /// Git branch's previews, over preview's own. A forkrail-control from before branches finds no
    /// targets in such a request and refuses it, rather than change preview's values.
    EnvSet {
        project: String,
        values: BTreeMap<String, String>,
        #[serde(default)]
        targets: Vec<EnvTarget>,
        #[serde(default)]
        branch: Option<String>,
    },
    EnvRemove {
        project: String,
        key: String,
        #[serde(default)]
        targets: Vec<EnvTarget>,
        #[serde(default)]
        branch: Option<String>,
    },
    /// A project's variables for one target, for `forkrail env pull`: Development's values, and
    /// only the names of production's and previews' (env.rs, `pull`); with `branch` (and no
    /// target), the names that branch's previews get.
    EnvPull {
        project: String,
        #[serde(default)]
        target: Option<EnvTarget>,
        #[serde(default)]
        branch: Option<String>,
    },
    /// A deployment's source files, and one of them by its path in the project.
    SourceList { id: String },
    SourceFile { id: String, path: String },
    /// The repositories the install's GitHub App reaches, the most recently pushed first, and the
    /// accounts it's installed on. `query` narrows them by name; `fresh` asks GitHub again now.
    GitRepos {
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        fresh: bool,
    },
    /// Connect a project to a repository (`owner/name`): its pushes deploy from then on, `branch`
    /// (the repository's default if not given) to production.
    GitConnect {
        project: String,
        repo: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        root: Option<String>,
    },
    GitDisconnect { project: String },
    /// Make a project from a repository (`owner/name`): connected from the start, with `env` for
    /// production and previews, and its production branch deployed at once.
    ProjectCreate {
        project: String,
        repo: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        root: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        /// Its framework preset, a Vercel slug; absent or null finds it from its files.
        #[serde(default)]
        framework: Option<String>,
    },
    /// Make a project without a repository: empty, or from one of the templates
    /// (crates/cli/src/templates.rs), whose production deployment starts building at once.
    ProjectNew {
        project: String,
        #[serde(default)]
        template: Option<String>,
    },
    /// Delete a project and everything of it: its deployments, domains, schedules, env vars,
    /// functions in every region and files (the CLI's project.rs). Asked with its name again.
    ProjectDelete { project: String },
    /// Call a project something else: only the name shown changes, its id and everything under it
    /// stay (the CLI's project.rs `rename`). Its id as `name` puts the first name back.
    ProjectRename { project: String, name: String },
    /// Remove one deployment that isn't production (`forkrail rm`).
    Remove { id: String },
    /// A push to a project's connected repository, as GitHub's webhook told the dashboard: build
    /// that commit. Refused unless the project is connected to that repository.
    GitPush {
        project: String,
        repo_id: u64,
        branch: String,
        sha: String,
        /// The commit the branch was at before, when GitHub said.
        #[serde(default)]
        before: Option<String>,
        #[serde(default)]
        pusher: Option<String>,
        /// The pusher's role in the team, as the dashboard found it by their GitHub account:
        /// `owner`, `member`, `developer` or `viewer`, or `none` when they aren't in it. A project
        /// whose code acts with more than the shared worker role builds only a member's or an
        /// owner's push (connect::push_refused); without this, nobody's.
        #[serde(default)]
        pusher_role: Option<String>,
    },
    /// A branch of a project's connected repository was deleted: its previews go, and its
    /// `<project>-git-<branch>` host with them.
    GitBranchDeleted { project: String, repo_id: u64, branch: String },
    /// A pull request was opened in a project's connected repository: it gets the project's
    /// comment, saying how its branch's newest deployment stands.
    GitPullRequest { project: String, repo_id: u64, number: u64, branch: String },
    /// A project's own bulk redirects: list, change (staged), publish, restore, test.
    Redirects {
        project: String,
        #[serde(flatten)]
        op: RedirectsOp,
    },
    /// A project's own routing rules, likewise.
    Routes {
        project: String,
        #[serde(flatten)]
        op: RoutesOp,
    },
    /// The install's custom domains: list, inspect, add, remove, update, check (domains.rs).
    Domains {
        #[serde(flatten)]
        op: DomainsOp,
    },
    /// A project's schedules (schedules.rs): list, run now, enable, disable, sync.
    Schedules {
        #[serde(flatten)]
        op: SchedulesOp,
    },
    /// A project's response cache (purge.rs): its latest purges; invalidate, delete or purge by
    /// tag, applied by every router from the table.
    Cache {
        #[serde(flatten)]
        op: CacheOp,
    },
    /// A project's deployment protection (protection.rs): look, change settings, bypass secrets,
    /// share links taken back; the install's default scope.
    Protection {
        #[serde(flatten)]
        op: ProtectionOp,
    },
    /// Spend protection (spend.rs): the install's budget, projects' own, pauses and resumes.
    Spend {
        #[serde(flatten)]
        op: SpendOp,
    },
    /// A project's firewall, or the install's without `project` (firewall.rs): look, change
    /// (staged), diff, publish, restore, discard, test.
    Firewall {
        #[serde(default)]
        project: Option<String>,
        #[serde(flatten)]
        op: FirewallOp,
    },
    /// A project's deploy hooks (deploy_hooks.rs): list, create (the answer holds its secret, the
    /// only time it's seen), remove.
    DeployHooks {
        #[serde(flatten)]
        op: DeployHooksOp,
    },
    /// One of a project's deploy hooks was called, as the dashboard's endpoint for it heard: build
    /// its branch's newest commit, if `secret` is the hook's. Nobody's to ask for by hand.
    DeployHook {
        project: String,
        id: String,
        secret: String,
        #[serde(default)]
        no_cache: bool,
    },
    /// How long deployments are kept (retention.rs): look, a dry run, change a project's policy or
    /// the install's default, run it now, restore a deleted deployment. EventBridge Scheduler asks
    /// for `sweep` once a day (deploy/install/retention.yaml).
    Retention {
        #[serde(flatten)]
        op: RetentionOp,
    },
    /// Whether the function answers, and for which install.
    Ping,
    /// Every region functions may run in: where routers are, and whether functions can run there
    /// yet (the CLI's regions.rs `views`).
    Regions,
}

fn refuse() -> PreviewEnv {
    PreviewEnv::Refuse
}

/// A field that's there, even as null: `Some(None)`. Absent is `None`, by `default`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let region = std::env::var("AWS_REGION").map_err(|_| "AWS_REGION isn't set")?;
    let config = aws_config::defaults(BehaviorVersion::latest()).load().await;
    let bucket = std::env::var("FORKRAIL_BUCKET").ok().filter(|b| !b.is_empty());
    let ctx: &'static Ctx = Box::leak(Box::new(Ctx::connect(config, &region, bucket.as_deref()).await?));
    if std::env::args().nth(1).as_deref() == Some("--once") {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        // The audit line goes to stderr here, so stdout is the answer alone.
        ONCE.store(true, std::sync::atomic::Ordering::Relaxed);
        println!("{}", handle(ctx, serde_json::from_str(&text)?).await);
        return Ok(());
    }
    lambda_runtime::run(service_fn(move |event: LambdaEvent<Value>| async move { Ok::<Value, Error>(handle(ctx, event.payload).await) })).await
}

/// Answering one request from a terminal (`--once`), not Lambda.
static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// GitHub as the install's app, kept between invocations with the tokens it has: the app's record
/// and key are read again every ten minutes, so a replaced app is noticed.
async fn github(ctx: &Ctx) -> Result<Arc<Api>> {
    static KEPT: tokio::sync::Mutex<Option<(Instant, Arc<Api>)>> = tokio::sync::Mutex::const_new(None);
    let mut kept = KEPT.lock().await;
    if let Some((since, api)) = kept.as_ref()
        && since.elapsed() < Duration::from_secs(600)
    {
        return Ok(api.clone());
    }
    let api = Arc::new(Api::connect(ctx).await?);
    *kept = Some((Instant::now(), api.clone()));
    Ok(api)
}

/// A router found a project's budget reached (crates/router/src/budgets.rs): the pause is recorded,
/// so only a person resumes it, and the function's reserved concurrency set to 0 in every region,
/// so Lambda refuses its invocations whatever asks. Both routers say so; the second changes
/// nothing. A record that fails comes back (`batchItemFailures`) to be tried again.
async fn spend_paused(ctx: &Ctx, records: &[Value], started: Instant) -> Value {
    let mut failed = Vec::new();
    for record in records {
        let id = record.get("messageId").and_then(Value::as_str).unwrap_or_default().to_owned();
        let event: Value = record.get("body").and_then(Value::as_str).and_then(|b| serde_json::from_str(b).ok()).unwrap_or_default();
        let payload = &event["payload"];
        let op = match (event["type"].as_str(), payload["project"].as_str(), payload["reason"].as_str()) {
            (Some("spend.paused"), Some(project), Some(reason)) => SpendOp::Paused {
                project: project.to_owned(),
                reason: reason.to_owned(),
                reserved: payload["reserved"].as_u64().and_then(|n| u32::try_from(n).ok()),
            },
            (Some("spend.month_end"), ..) if payload["month"].is_u64() && payload["spentN"].is_i64() => SpendOp::MonthEnd {
                month: payload["month"].as_u64().unwrap_or_default() as u32,
                spent_n: payload["spentN"].as_i64().unwrap_or_default(),
            },
            _ => {
                log("spend_event", Some("router"), &json!({"id": id}), Err("not a spend event"), started);
                continue;
            }
        };
        let about = about(&Request::Spend { op: op.clone() });
        match spend::run(ctx, op).await {
            Ok(_) => log("spend_paused", Some("router"), &about, Ok(()), started),
            Err(e) => {
                log("spend_paused", Some("router"), &about, Err(&format!("{e:#}")), started);
                failed.push(json!({"itemIdentifier": id}));
            }
        }
    }
    json!({"batchItemFailures": failed})
}

async fn handle(ctx: &Ctx, payload: Value) -> Value {
    let started = Instant::now();
    if let Some(records) = payload.get("Records").and_then(Value::as_array) {
        // SQS: the routers' spend pauses (FORKRAIL_SPEND_QUEUE); SNS: builds' ends.
        if records.first().and_then(|r| r.get("eventSource")).and_then(Value::as_str) == Some("aws:sqs") {
            return spend_paused(ctx, records, started).await;
        }
        return builds_ended(ctx, records, started).await;
    }
    let action = payload.get("action").and_then(Value::as_str).unwrap_or("").to_owned();
    let invocation = match serde_json::from_value::<Invocation>(payload) {
        Ok(i) => i,
        Err(e) => {
            log(&action, None, &json!({}), Err(&format!("bad request: {e}")), started);
            return json!({"ok": false, "error": format!("bad request: {e}"), "code": "bad_request"});
        }
    };
    let actor = invocation.actor.as_deref().map(|a| a.chars().take(100).collect::<String>());
    let about = about(&invocation.request);
    if let Err(why) = permitted(ctx, invocation.role.as_deref(), &invocation.request).await {
        log(&action, actor.as_deref(), &about, Err(&format!("refused: {why}")), started);
        return json!({"ok": false, "error": why, "code": "role"});
    }
    // Only the two ways a person asks: a push is recorded as one by `git_push` itself.
    let via = match invocation.via {
        Some(Via::Cli) => Via::Cli,
        _ => Via::Dashboard,
    };
    match run(ctx, invocation.request, actor.as_deref(), via).await {
        Ok(result) => {
            log(&action, actor.as_deref(), &about, Ok(()), started);
            json!({"ok": true, "result": result})
        }
        Err(e) => {
            let message = format!("{e:#}");
            log(&action, actor.as_deref(), &about, Err(&message), started);
            match e.downcast_ref::<PreviewEnvDiffers>() {
                Some(d) => json!({"ok": false, "error": message, "code": "preview_env_differs", "keys": d.keys, "rebuild": d.rebuild}),
                None => json!({"ok": false, "error": message}),
            }
        }
    }
}

async fn run(ctx: &Ctx, request: Request, actor: Option<&str>, via: Via) -> Result<Value> {
    Ok(match request {
        Request::Promote { id, preview_env } => json!(deploy::promote(ctx, &id, preview_env, actor, via).await?),
        Request::Rollback { project, to } => {
            let production = deploy::rollback(ctx, &project, to.as_deref()).await?;
            json!({"project": project, "production": production})
        }
        Request::Redeploy { id, production, no_cache, skip_domain } => {
            let target = production.then_some(Target::Production);
            let args = Redeploy { id, target, skip_domain, actor: actor.map(str::to_owned), via, no_cache };
            match deploy::redeploy_any(ctx, args).await? {
                Redeployed::Published(p) => json!(p),
                Redeployed::Building(s) => json!({"id": s.id, "project": s.project, "target": s.target, "urls": [s.url], "building": true}),
            }
        }
        Request::UploadUrl { project, sha256, bytes } => json!(remote::upload_url(ctx, &project, &sha256, bytes).await?),
        Request::Deploy { project, production, upload, git, memory, concurrent, skip_domain, same_source, no_cache } => {
            let start = Start {
                project,
                target: if production { Target::Production } else { Target::Preview },
                upload,
                git,
                deployer: actor.map(str::to_owned),
                publish: PublishOptions { memory, concurrent, skip_domain, same_source, redeploy_of: None },
                via,
                no_cache,
                github_deployment: None,
                previous_sha: None,
            };
            let started = remote::start(ctx, start).await?;
            json!({"id": started.id, "project": started.project, "target": started.target, "urls": [started.url], "building": true})
        }
        Request::BuildCancel { id } => {
            let rec = remote::cancel(ctx, &id).await?;
            json!({"id": rec.id, "project": rec.project, "status": rec.status.as_str()})
        }
        Request::Settings { project, memory_mb, max_duration_s, concurrent, max_inflight, framework, auto_assign_domains, skew_protection, skew_max_age_s, skew_threshold, skew_allowed_domains, keep_warm, analytics, regions } => {
            // The function settings, the preset, auto-assign, skew protection and keep-warm: a project's role and egress stay the CLI's to change.
            let skew = skew::Change { enabled: skew_protection, max_age_s: skew_max_age_s, threshold: skew_threshold, allowed_domains: skew_allowed_domains };
            let change = Change { memory_mb, max_duration_s, concurrent, max_inflight, framework, auto_assign_domains, skew, keep_warm, analytics, regions, ..Default::default() };
            let p = project::update(ctx, &project, &change, false).await?;
            let skew = skew::get(ctx, &project).await?;
            let keep = forkrail_cli::warm::keep_view(forkrail_cli::warm::keep_get(ctx, &project).await?);
            let analytics = forkrail_cli::web_analytics::get(ctx, &project).await?;
            json!({
                "project": p.name, "memory_mb": p.memory_mb, "max_duration_s": p.max_duration_s, "concurrent": p.concurrent, "max_inflight": p.max_inflight, "framework": p.framework,
                "auto_assign_domains": p.auto_assigns_domains(), "skew": skew, "keep_warm": keep, "analytics": analytics,
                "regions": p.regions, "home_region": ctx.install.home_region,
            })
        }
        Request::EnvSet { project, values, targets, branch } => {
            env_vars::set(ctx, &project, &values, &targets, branch.as_deref(), false).await?;
            json!({"project": project, "set": values.keys().collect::<Vec<_>>(), "targets": targets, "branch": branch})
        }
        Request::EnvRemove { project, key, targets, branch } => {
            let removed = env_vars::remove(ctx, &project, &key, &targets, branch.as_deref()).await?;
            json!({"project": project, "key": key, "removed": removed, "branch": branch})
        }
        Request::EnvPull { project, target, branch } => {
            let target = env_vars::pull_target(target, branch.as_deref())?;
            json!(env_vars::pull(ctx, &project, target, branch.as_deref()).await?)
        }
        Request::Redirects { project, op } => project_routing::redirects(ctx, &project, op, actor).await?,
        Request::Routes { project, op } => project_routing::routes(ctx, &project, op, actor).await?,
        Request::Domains { op } => domains::run(ctx, op, actor).await?,
        Request::Schedules { op } => schedules::run(ctx, op, actor).await?,
        Request::Cache { op } => purge::run(ctx, op, actor).await?,
        Request::Protection { op } => protection::run(ctx, op).await?,
        Request::Spend { op } => json!(spend::run(ctx, op).await?),
        Request::Firewall { project, op } => firewall::run(ctx, project.as_deref(), op, actor).await?,
        Request::Retention { op } => retention::run(ctx, op, actor).await?,
        Request::SourceList { id } => json!(source::list(ctx, &id).await?),
        Request::SourceFile { id, path } => json!(source::file(ctx, &id, &path).await?),
        Request::GitRepos { query, fresh } => json!(github(ctx).await?.repos(query.as_deref(), fresh).await?),
        Request::GitConnect { project, repo, branch, root } => {
            let api = github(ctx).await?;
            let link = connect::connect(ctx, &api, Connect { project: project.clone(), repo, branch, root, actor: actor.map(str::to_owned) }).await?;
            json!({"project": project, "git": link})
        }
        Request::GitDisconnect { project } => {
            let was = connect::disconnect(ctx, &project).await?;
            json!({"project": project, "was": was.map(|link| link.repo)})
        }
        Request::ProjectCreate { project, repo, branch, root, env, framework } => {
            if let Some(slug) = &framework {
                Change { framework: Some(Some(slug.clone())), ..Default::default() }.check()?;
            }
            let api = github(ctx).await?;
            json!(connect::create(ctx, &api, Create { project, repo, branch, root, env, actor: actor.map(str::to_owned), framework }).await?)
        }
        Request::ProjectDelete { project } => json!(project::delete(ctx, &project).await?),
        Request::ProjectRename { project, name } => json!(project::rename(ctx, &project, &name).await?),
        Request::Remove { id } => {
            let rec = deploy::record(ctx, &id).await?;
            deploy::remove_as(ctx, &id, actor).await?;
            json!({"id": id, "project": rec.project, "removed": true})
        }
        Request::ProjectNew { project, template } => json!(templates::create(ctx, templates::New { project, template, actor: actor.map(str::to_owned) }).await?),
        Request::GitPush { project, repo_id, branch, sha, before, pusher, pusher_role } => {
            let api = github(ctx).await?;
            // All zeros is how GitHub says a branch is new.
            let before = before.filter(|b| b.bytes().any(|c| c != b'0'));
            let pusher_role = pusher_role.filter(|r| matches!(r.as_str(), "owner" | "member" | "developer" | "viewer"));
            let push = Push { project: project.clone(), repo_id, branch, sha: Some(sha), before, pusher, hook: None, no_cache: false, webhook: true, pusher_role };
            match connect::pushed(ctx, &api, push).await? {
                Pushed::Started(started) => json!({"id": started.id, "project": started.project, "target": started.target, "urls": [started.url], "building": true}),
                Pushed::Skipped(why) => json!({"project": project, "skipped": why}),
            }
        }
        Request::DeployHooks { op } => deploy_hooks::run(ctx, op, actor).await?,
        Request::DeployHook { project, id, secret, no_cache } => {
            let api = github(ctx).await?;
            match deploy_hooks::trigger(ctx, &api, &project, &id, &secret, no_cache).await? {
                Pushed::Started(started) => json!({"id": started.id, "project": started.project, "target": started.target, "urls": [started.url], "building": true}),
                Pushed::Skipped(why) => json!({"project": project, "skipped": why}),
            }
        }
        Request::GitBranchDeleted { project, repo_id, branch } => json!(connect::branch_deleted(ctx, &project, repo_id, &branch).await?),
        Request::GitPullRequest { project, repo_id, number, branch } => {
            let api = github(ctx).await?;
            let said = connect::pull_request(ctx, &api, PullRequest { project: project.clone(), repo_id, number, branch }).await?;
            json!({"project": project, "number": number, "deployment": said})
        }
        Request::Ping => json!({"account": ctx.install.account, "home_region": ctx.install.home_region, "version": env!("CARGO_PKG_VERSION")}),
        // `sets_up`: picking a region that isn't set up sets it up (regions::check_ready), so the
        // dashboard lets it be saved; an older control refused it.
        Request::Regions => json!({"home_region": ctx.install.home_region, "regions": forkrail_cli::regions::views(ctx).await?, "sets_up": true}),
    })
}

/// Whether someone with `role` may ask for `request`: the dashboard's own check, made again.
/// The reason, if not.
async fn permitted(ctx: &Ctx, role: Option<&str>, request: &Request) -> Result<(), String> {
    let Some(role) = role else { return Ok(()) };
    // What a developer may do to a deployment depends on which it is.
    let record = match (role, request) {
        ("developer", Request::Redeploy { id, .. } | Request::BuildCancel { id } | Request::Remove { id } | Request::Retention { op: RetentionOp::Restore { id } })
        | ("member", Request::Redeploy { id, .. }) => Some(deploy::record(ctx, id).await.map_err(|e| format!("{e:#}"))?),
        _ => None,
    };
    let target = record.as_ref().filter(|_| role == "developer").map(|r| r.target);
    allowed(role, request, target)?;
    // A project running as one of forkrail's own roles (the dashboard) acts as the install's
    // owners: how it's reached is theirs too (a routing rule sending its sign-in elsewhere would
    // receive owners' tokens, as its domain would).
    if role != "owner"
        && let Some(project) = configures(request)
    {
        let record: Option<forkrail_cli::state::Project> =
            ctx.store.get_value(&forkrail_cli::state::project_key(project)).await.map_err(|e| format!("{e:#}"))?;
        if record.is_some_and(|p| runs_as_forkrail(p.role.as_deref())) {
            return Err(format!("{project} runs as one of forkrail's own roles: its routing rules, redirects, firewall, protection and settings are an owner's to change"));
        }
    }
    if role != "owner"
        && let Request::Domains { op } = request
        && op.changes()
    {
        owners_domain(ctx, op).await?;
    }
    // Which of its deployments serves (a promote or a rollback could bring back an older one, or a
    // preview built from any branch, to receive owners' sign-ins), and whether it's there at all.
    if role != "owner" {
        let project = match request {
            Request::Rollback { project, .. } | Request::ProjectDelete { project } | Request::ProjectRename { project, .. } | Request::GitDisconnect { project } => Some(project.clone()),
            Request::Promote { id, .. } | Request::Remove { id } => match &record {
                Some(r) => Some(r.project.clone()),
                None => Some(deploy::record(ctx, id).await.map_err(|e| format!("{e:#}"))?.project),
            },
            _ => None,
        };
        if let Some(project) = project {
            let record: Option<forkrail_cli::state::Project> =
                ctx.store.get_value(&forkrail_cli::state::project_key(&project)).await.map_err(|e| format!("{e:#}"))?;
            if record.is_some_and(|p| runs_as_forkrail(p.role.as_deref())) {
                return Err(format!("{project} runs as one of forkrail's own roles: which deployment it serves, and the project itself, are an owner's to change"));
            }
        }
    }
    // New code or env vars run with the project's role and reach its egress targets, previews too;
    // so does a redeploy, with the project's role and egress as they are now.
    let redeployed = record.as_ref().filter(|_| matches!(request, Request::Redeploy { .. })).map(|r| r.project.as_str());
    if let Some(project) = changes_code(request).or(redeployed)
        && matches!(role, "developer" | "member")
    {
        let record: Option<forkrail_cli::state::Project> =
            ctx.store.get_value(&forkrail_cli::state::project_key(project)).await.map_err(|e| format!("{e:#}"))?;
        if let Some(p) = record {
            allowed_for_project(role, p.role.as_deref(), !p.egress.is_empty())?;
        }
    }
    Ok(())
}

/// Whether a change to a domain is an owner's: one that a project running as one of forkrail's
/// own roles has or would get (the dashboard's address receives owners' CLI tokens, every build's
/// log key and deploy hooks' secrets, so whoever serves it acts as the owners), or one of the
/// names kept under the base domain for forkrail's own site (`dashboard.<base>`, `www.<base>`).
async fn owners_domain(ctx: &Ctx, op: &DomainsOp) -> Result<(), String> {
    let (domain, to) = match op {
        DomainsOp::Add { domain, project, .. } => (domain, Some(project.as_str())),
        DomainsOp::Remove { domain } | DomainsOp::Update { domain, .. } => (domain, None),
        _ => return Ok(()),
    };
    let fail = |e: anyhow::Error| format!("{e:#}");
    let domain = forkrail_common::routes::normalize_host(domain);
    let settings = ctx.store.settings().await.map_err(fail)?;
    let kept = domain
        .strip_suffix(&format!(".{}", settings.base_domain))
        .is_some_and(|label| forkrail_cli::deploy::RESERVED_NAMES.contains(&label.trim_start_matches("*.")));
    if kept {
        return Err(format!("{domain} is one of the names kept for forkrail's own site: an owner's to change"));
    }
    let routes: forkrail_common::routes::Routes = ctx.store.get_value(forkrail_cli::state::ROUTES_KEY).await.map_err(fail)?.unwrap_or_default();
    let holder = routes.domains.get(&domain).map(|d| d.project.as_str());
    for project in holder.into_iter().chain(to) {
        let record: Option<forkrail_cli::state::Project> = ctx.store.get_value(&forkrail_cli::state::project_key(project)).await.map_err(fail)?;
        if record.is_some_and(|p| runs_as_forkrail(p.role.as_deref())) {
            return Err(format!("{project} runs as one of forkrail's own roles: its domains are an owner's to change, since whoever serves them acts as the install's owners"));
        }
    }
    Ok(())
}

/// Whether a project's functions run as one of forkrail's own roles (not the shared worker role,
/// which has no rights).
fn runs_as_forkrail(project_role: Option<&str>) -> bool {
    let name = |r: &str| r.rsplit('/').next().unwrap_or(r).to_owned();
    project_role.is_some_and(|r| name(r) != "forkrail-worker" && name(r).starts_with("forkrail-"))
}

/// The project whose routing, firewall, protection or settings a request changes.
fn configures(request: &Request) -> Option<&str> {
    match request {
        Request::Routes { project, op } if op.changes() => Some(project),
        Request::Redirects { project, op } if op.changes() => Some(project),
        Request::Firewall { project: Some(project), op } if op.changes() => Some(project),
        Request::Protection { op } if op.changes() => op.project(),
        Request::Settings { project, .. } => Some(project),
        _ => None,
    }
}

/// The project a request puts new code or env vars in front of: a deploy of an upload, a change
/// to its env vars, or another repository to build from.
fn changes_code(request: &Request) -> Option<&str> {
    match request {
        Request::UploadUrl { project, .. }
        | Request::Deploy { project, .. }
        | Request::EnvSet { project, .. }
        | Request::EnvRemove { project, .. }
        | Request::GitConnect { project, .. } => Some(project),
        _ => None,
    }
}

/// `allowed`, for a request `changes_code` names a project for, once its role and egress are
/// known. Its functions run as its role and reach its egress targets in every deployment,
/// previews included: a developer's preview there would act with them, so a project that has
/// either is a member's to deploy. One that runs as one of forkrail's own roles (the dashboard's
/// `forkrail-dashboard` asks forkrail-control for anything, as any role) acts as the install's
/// owners do: an owner's.
fn allowed_for_project(role: &str, project_role: Option<&str>, egress: bool) -> Result<(), String> {
    let name = |r: &str| r.rsplit('/').next().unwrap_or(r).to_owned();
    // The shared worker role has no rights: as good as none.
    let project_role = project_role.filter(|r| name(r) != "forkrail-worker");
    let forkrails = project_role.is_some_and(|r| name(r).starts_with("forkrail-"));
    match role {
        "owner" => Ok(()),
        _ if forkrails => Err("this project's functions run as one of forkrail's own roles: new code or env vars there act as the install's owners, so they're an owner's to deploy".to_owned()),
        "member" => Ok(()),
        _ if project_role.is_some() || egress => Err(
            "this project's functions run with an AWS role or private egress of its own, previews too: new code or env vars there are a member's to deploy".to_owned(),
        ),
        _ => Ok(()),
    }
}

const PRODUCTION_IS_BY_MERGE: &str = "a developer deploys previews: production moves when a commit is merged to the production branch, or a member moves it";
const A_MEMBERS: &str = "a developer deploys previews and changes their env vars: this is a member's to change";

/// `permitted`, once the deployment a request names (`target`, for a redeploy or a cancel) is known.
fn allowed(role: &str, request: &Request, target: Option<Target>) -> Result<(), String> {
    use Request::*;
    if matches!(request, SourceList { .. } | SourceFile { .. } | Ping | Regions) {
        return Ok(());
    }
    // Looking at a project's redirects and rules, or testing a URL, is anyone's.
    match request {
        Redirects { op, .. } if !op.changes() => return Ok(()),
        Routes { op, .. } if !op.changes() => return Ok(()),
        Domains { op } if !op.changes() => return Ok(()),
        Schedules { op } if !op.changes() => return Ok(()),
        Cache { op } if !op.changes() => return Ok(()),
        Protection { op } if !op.changes() => return Ok(()),
        // Budgets are anyone's to see; only a router says one was reached, and only owners change
        // what the install may spend or pause and resume it.
        Spend { op } if !op.changes() => return Ok(()),
        Spend { op: SpendOp::Paused { .. } | SpendOp::MonthEnd { .. } | SpendOp::Build { .. } } => {
            return Err("only a router or the builders say a budget was reached, a month ended or a build cost".to_owned());
        }
        Spend { .. } if role != "owner" => return Err("what the install may spend, and pausing or resuming for it, is an owner's to change".to_owned()),
        Firewall { op, .. } if !op.changes() => return Ok(()),
        DeployHooks { op } if !op.changes() => return Ok(()),
        Retention { op } if !op.changes() => return Ok(()),
        // The install's default and a sweep of every project reach every project: its owners'.
        Retention { op } if op.install_wide() && role != "owner" => return Err("the install's retention policy is an owner's to change or run".to_owned()),
        // Values are a developer's to read, as they are to set.
        EnvPull { .. } if role == "viewer" => return Err("a viewer sees env var names, not values: Development's are a developer's or a member's to read".to_owned()),
        // The install's firewall stands in front of every project: its owners'.
        Firewall { project: None, .. } if role != "owner" => return Err("the install's firewall is an owner's to change".to_owned()),
        // The scope of every project without settings: the team's owners'.
        Protection { op: ProtectionOp::Default { .. } } if role != "owner" => {
            return Err("the install's default protection is an owner's to change".to_owned());
        }
        _ => {}
    }
    match role {
        "owner" | "member" => Ok(()),
        "viewer" => Err("a viewer can look, not change: an owner of the team can make you a member".to_owned()),
        "developer" => {
            let production = |refuse: bool| if refuse { Err(PRODUCTION_IS_BY_MERGE.to_owned()) } else { Ok(()) };
            match request {
                UploadUrl { .. } => Ok(()),
                Deploy { production: prod, skip_domain, .. } => production(*prod || *skip_domain),
                Redeploy { production: prod, .. } => production(*prod || target != Some(Target::Preview)),
                BuildCancel { .. } => production(target != Some(Target::Preview)),
                // A preview, as on Vercel; never a production deployment.
                Remove { .. } => production(target != Some(Target::Preview)),
                EnvSet { targets, .. } | EnvRemove { targets, .. } if targets.contains(&EnvTarget::Production) => {
                    Err("a developer changes previews' env vars: production's are a member's to change".to_owned())
                }
                EnvSet { .. } | EnvRemove { .. } => Ok(()),
                // Development's values, and every target's names: a developer works with both.
                EnvPull { .. } => Ok(()),
                Promote { .. } | Rollback { .. } => production(true),
                Settings { .. } | GitRepos { .. } | GitConnect { .. } | GitDisconnect { .. } | ProjectCreate { .. } | ProjectNew { .. } | ProjectDelete { .. } | ProjectRename { .. } => Err(A_MEMBERS.to_owned()),
                Redirects { .. } | Routes { .. } => Err("a developer deploys previews: a project's redirects and routing rules change every deployment, so they're a member's to change".to_owned()),
                // Asking for a domain to be looked at again changes nothing it serves.
                Domains { op: DomainsOp::Check { .. } } => Ok(()),
                Domains { .. } => Err("a developer deploys previews: domains are a member's to change".to_owned()),
                // Schedules run on production only.
                Schedules { .. } => Err("a developer deploys previews: schedules run on production, so they're a member's to run or change".to_owned()),
                // As on Vercel: purging the cache is an owner's or a member's.
                Cache { .. } => Err("a developer deploys previews: purging the cache is a member's, as on Vercel".to_owned()),
                // Share links open previews to someone outside: a developer's too, as on Vercel.
                Protection { op: ProtectionOp::ShareRevoke { .. } } => Ok(()),
                Protection { .. } => Err("a developer deploys previews: who may open them is a member's to change".to_owned()),
                Firewall { .. } => Err("a developer deploys previews: the firewall stands in front of production too, so it's a member's to change".to_owned()),
                Spend { .. } => Err("what the install may spend is an owner's to change".to_owned()),
                // A deleted preview back, as one may be deleted; never a production deployment.
                Retention { op: RetentionOp::Restore { .. } } => production(target != Some(Target::Preview)),
                Retention { .. } => Err("a developer deploys previews: how long deployments are kept is a member's to change".to_owned()),
                // What GitHub's webhook says happened, or a hook's URL was called: nobody's to ask for.
                GitPush { .. } | GitBranchDeleted { .. } | GitPullRequest { .. } | DeployHook { .. } => Err(A_MEMBERS.to_owned()),
                // A hook deploys production from its URL, whoever calls it: a member's to make.
                DeployHooks { .. } => Err("a developer deploys previews: a deploy hook can build production for anyone with its URL, so hooks are a member's to make or remove".to_owned()),
                SourceList { .. } | SourceFile { .. } | Ping | Regions => Ok(()),
            }
        }
        other => Err(format!("{other:?} isn't a role this forkrail-control knows, so it changes nothing for it: release a newer one")),
    }
}

/// What a request is about, for the log: ids, projects and env var names, never values.
fn about(request: &Request) -> Value {
    match request {
        Request::Promote { id, preview_env } => json!({"id": id, "preview_env": format!("{preview_env:?}")}),
        Request::Rollback { project, to } => json!({"project": project, "to": to}),
        Request::Redeploy { id, production, no_cache, skip_domain } => json!({"id": id, "production": production, "no_cache": no_cache, "skip_domain": skip_domain}),
        Request::UploadUrl { project, sha256, bytes } => json!({"project": project, "sha256": sha256, "bytes": bytes}),
        Request::Deploy { project, production, upload, git, memory, concurrent, skip_domain, same_source, no_cache } => json!({
            "project": project, "production": production, "sha256": upload.sha256, "files": upload.files,
            "commit": git.as_ref().map(|g| &g.sha), "memory": memory, "concurrent": concurrent,
            "skip_domain": skip_domain, "same_source": same_source, "no_cache": no_cache,
        }),
        Request::BuildCancel { id } => json!({"id": id}),
        Request::Settings { project, memory_mb, max_duration_s, concurrent, max_inflight, framework, auto_assign_domains, skew_protection, skew_max_age_s, skew_threshold, skew_allowed_domains, keep_warm, analytics, regions } => json!({
            "project": project, "memory_mb": memory_mb, "max_duration_s": max_duration_s, "concurrent": concurrent, "max_inflight": max_inflight, "framework": framework, "auto_assign_domains": auto_assign_domains,
            "skew_protection": skew_protection, "skew_max_age_s": skew_max_age_s, "skew_threshold": skew_threshold, "skew_allowed_domains": skew_allowed_domains,
            "keep_warm": keep_warm, "analytics": analytics, "regions": regions,
        }),
        Request::EnvSet { project, values, targets, branch } => json!({"project": project, "keys": values.keys().collect::<Vec<_>>(), "targets": targets, "branch": branch}),
        Request::EnvRemove { project, key, targets, branch } => json!({"project": project, "key": key, "targets": targets, "branch": branch}),
        Request::EnvPull { project, target, branch } => json!({"project": project, "target": target, "branch": branch}),
        Request::SourceList { id } => json!({"id": id}),
        Request::SourceFile { id, path } => json!({"id": id, "path": path}),
        Request::GitRepos { query, fresh } => json!({"query": query, "fresh": fresh}),
        Request::GitConnect { project, repo, branch, root } => json!({"project": project, "repo": repo, "branch": branch, "root": root}),
        Request::GitDisconnect { project } => json!({"project": project}),
        Request::ProjectCreate { project, repo, branch, root, env, framework } => json!({"project": project, "repo": repo, "branch": branch, "root": root, "keys": env.keys().collect::<Vec<_>>(), "framework": framework}),
        Request::ProjectNew { project, template } => json!({"project": project, "template": template}),
        Request::ProjectDelete { project } => json!({"project": project}),
        Request::ProjectRename { project, name } => json!({"project": project, "name": name}),
        Request::Remove { id } => json!({"id": id}),
        Request::GitPush { project, repo_id, branch, sha, before, pusher, pusher_role } => {
            json!({"project": project, "repo_id": repo_id, "branch": branch, "sha": sha, "before": before, "pusher": pusher, "pusher_role": pusher_role})
        }
        Request::GitBranchDeleted { project, repo_id, branch } => json!({"project": project, "repo_id": repo_id, "branch": branch}),
        Request::GitPullRequest { project, repo_id, number, branch } => json!({"project": project, "repo_id": repo_id, "number": number, "branch": branch}),
        Request::Redirects { project, op } => json!({"project": project, "op": op.name()}),
        Request::Routes { project, op } => json!({"project": project, "op": op.name()}),
        Request::Domains { op } => json!({"op": op.name(), "domain": op.domain(), "project": op.project()}),
        Request::Schedules { op } => json!({"op": op.name(), "project": op.project(), "key": op.key()}),
        Request::Cache { op } => json!({"op": op.name(), "project": op.project()}),
        Request::Protection { op } => json!({"op": op.name(), "project": op.project()}),
        Request::Spend { op } => json!({"op": op.name(), "projects": op.projects()}),
        Request::Firewall { project, op } => json!({"op": op.name(), "project": project}),
        // Never the secret.
        Request::DeployHooks { op } => json!({"op": op.name(), "project": op.project()}),
        Request::DeployHook { project, id, no_cache, .. } => json!({"project": project, "id": id, "no_cache": no_cache}),
        Request::Retention { op } => json!({"op": op.name(), "project": op.project(), "id": match &op { RetentionOp::Restore { id } => Some(id), _ => None }}),
        Request::Ping | Request::Regions => json!({}),
    }
}

/// Builder invocations' ends, from the builds topic: each published or recorded as failed. A
/// notice that fails is logged and dropped: retrying wouldn't change it, and the deployment then
/// reads as failed once its time is up.
async fn builds_ended(ctx: &Ctx, records: &[Value], started: Instant) -> Value {
    for record in records {
        let topic = record.pointer("/Sns/TopicArn").and_then(Value::as_str).unwrap_or("");
        let id = |n: &Value| n.pointer("/requestPayload/id").and_then(Value::as_str).unwrap_or("?").to_owned();
        let outcome = match record.pointer("/Sns/Message").and_then(Value::as_str) {
            _ if !topic.ends_with(&format!(":{BUILDS_TOPIC}")) => Err((String::from("?"), format!("not from {BUILDS_TOPIC}: {topic:?}"))),
            None => Err(("?".into(), "no message".into())),
            Some(text) => match serde_json::from_str::<Value>(text) {
                Err(e) => Err(("?".into(), format!("the message isn't JSON: {e}"))),
                // A notice turned away names a deployment it isn't the build of (a build's code
                // can publish to the topic): nothing is reported for it.
                Ok(notice) => remote::finished(ctx, &notice)
                    .await
                    .map(|done| (id(&notice), done))
                    .map_err(|e| (if e.is::<remote::Rejected>() { "?".to_owned() } else { id(&notice) }, format!("{e:#}"))),
            },
        };
        // A deployment a push made: its commit's status on GitHub says how it ended.
        let (Ok((id, _)) | Err((id, _))) = &outcome;
        let status = match id.as_str() {
            "?" => None,
            id => match connect::report(ctx, id, || github(ctx)).await {
                Ok(set) => set,
                Err(e) => Some(format!("not set: {e:#}")),
            },
        };
        let line = match outcome {
            Ok((id, done)) => json!({"action": "build_finished", "about": {"id": id, "done": done, "commit_status": status}, "ok": true, "ms": started.elapsed().as_millis() as u64}),
            Err((id, error)) => json!({"action": "build_finished", "about": {"id": id, "commit_status": status}, "ok": false, "error": error, "ms": started.elapsed().as_millis() as u64}),
        };
        println!("{line}");
    }
    json!({"ok": true})
}

/// One line per invocation, the control function's audit trail (CloudWatch Logs, kept a year).
fn log(action: &str, actor: Option<&str>, about: &Value, outcome: Result<(), &str>, started: Instant) {
    let line = json!({
        "action": action,
        "actor": actor,
        "about": about,
        "ok": outcome.is_ok(),
        "error": outcome.err(),
        "ms": started.elapsed().as_millis() as u64,
    });
    if ONCE.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_with_a_role_is_deployed_by_whoever_may_act_with_it() {
        let deploy = parse(json!({"action": "deploy", "project": "dashboard", "upload": {"sha256": "ab", "files": 3, "bytes": 10}}));
        assert_eq!(changes_code(&deploy), Some("dashboard"));
        assert_eq!(changes_code(&parse(json!({"action": "env_set", "project": "p", "values": {"A": "1"}, "targets": ["preview"]}))), Some("p"));
        assert_eq!(changes_code(&parse(json!({"action": "promote", "id": "a1b2c3d4"}))), None);
        let may = |role: &str, project_role: Option<&str>, egress: bool| allowed_for_project(role, project_role, egress).is_ok();
        for project_role in [Some("forkrail-dashboard"), Some("arn:aws:iam::123456789012:role/forkrail-dashboard")] {
            assert!(may("owner", project_role, false));
            assert!(!may("member", project_role, false) && !may("developer", project_role, false), "{project_role:?}");
        }
        assert!(may("member", Some("orders-app"), false) && !may("developer", Some("orders-app"), false));
        assert!(may("member", None, true) && !may("developer", None, true));
        assert!(may("developer", None, false) && may("developer", Some("forkrail-worker"), false), "the shared worker role has no rights");
        assert!(may("member", Some("arn:aws:iam::123456789012:role/forkrail-worker"), false));
    }

    fn parse(v: Value) -> Request {
        serde_json::from_value::<Invocation>(v).unwrap().request
    }

    #[test]
    fn requests() {
        assert!(matches!(parse(json!({"action": "promote", "id": "a1b2c3d4"})), Request::Promote { preview_env: PreviewEnv::Refuse, .. }));
        assert!(matches!(
            parse(json!({"action": "promote", "id": "a1b2c3d4", "preview_env": "republish", "actor": "Ada"})),
            Request::Promote { preview_env: PreviewEnv::Republish, .. }
        ));
        let Request::Settings { memory_mb, max_duration_s, concurrent, max_inflight, .. } = parse(json!({"action": "settings", "project": "p", "memory_mb": 768, "fluid": null})) else {
            panic!("not settings")
        };
        assert_eq!((memory_mb, max_duration_s, concurrent, max_inflight), (Some(Some(768)), None, Some(None), None));
        // Requests per worker: a number, or null back to the router's default.
        let Request::Settings { max_inflight, .. } = parse(json!({"action": "settings", "project": "p", "max_inflight": 3})) else { panic!("not settings") };
        assert_eq!(max_inflight, Some(Some(3)));
        let Request::Settings { max_inflight, .. } = parse(json!({"action": "settings", "project": "p", "max_inflight": null})) else { panic!("not settings") };
        assert_eq!(max_inflight, Some(None));
        // Skew protection: null clears the age and the threshold; absent leaves them.
        let skew = json!({"action": "settings", "project": "p", "skew_protection": true, "skew_max_age_s": null, "skew_allowed_domains": ["app.example.com"]});
        let Request::Settings { skew_protection, skew_max_age_s, skew_threshold, skew_allowed_domains, memory_mb, .. } = parse(skew) else { panic!("not settings") };
        assert_eq!((skew_protection, skew_max_age_s, skew_threshold, memory_mb), (Some(true), Some(None), None, None));
        assert_eq!(skew_allowed_domains.unwrap(), ["app.example.com"]);
        // Keep-warm: a number (0 is off) or "auto", null back to the routers' default, absent as it is.
        let keep = |body: serde_json::Value| match parse(body) {
            Request::Settings { keep_warm, .. } => keep_warm,
            _ => panic!("not settings"),
        };
        use forkrail_common::routes::KeepWarm;
        assert_eq!(keep(json!({"action": "settings", "project": "p", "keep_warm": 0})), Some(Some(KeepWarm::Count(0))));
        assert_eq!(keep(json!({"action": "settings", "project": "p", "keep_warm": 3})), Some(Some(KeepWarm::Count(3))));
        assert_eq!(keep(json!({"action": "settings", "project": "p", "keep_warm": "auto"})), Some(Some(KeepWarm::Auto)));
        assert_eq!(keep(json!({"action": "settings", "project": "p", "keep_warm": null})), Some(None));
        assert_eq!(keep(json!({"action": "settings", "project": "p", "memory_mb": 768})), None);
        // Function regions: a list, null for the default, absent as they are.
        let regions = |v: serde_json::Value| match parse(v) {
            Request::Settings { regions, .. } => regions,
            _ => panic!("not settings"),
        };
        assert_eq!(regions(json!({"action": "settings", "project": "p", "regions": ["iad1", "fra1"]})), Some(Some(vec!["iad1".to_owned(), "fra1".to_owned()])));
        assert_eq!(regions(json!({"action": "settings", "project": "p", "regions": null})), Some(None));
        assert_eq!(regions(json!({"action": "settings", "project": "p", "memory_mb": 768})), None);
        let Request::EnvSet { targets, values, .. } = parse(json!({"action": "env_set", "project": "p", "values": {"K": "v"}, "targets": ["production", "preview"]})) else {
            panic!("not env_set")
        };
        assert_eq!((targets, values.len()), (vec![EnvTarget::Production, EnvTarget::Preview], 1));
        // A branch's: no targets; a pull for one names no target either.
        let Request::EnvSet { targets, branch, .. } = parse(json!({"action": "env_set", "project": "p", "values": {"K": "v"}, "branch": "feature/logs"})) else {
            panic!("not env_set")
        };
        assert!(targets.is_empty() && branch.as_deref() == Some("feature/logs"));
        assert!(matches!(parse(json!({"action": "env_remove", "project": "p", "key": "K", "branch": "b"})), Request::EnvRemove { branch: Some(_), .. }));
        assert!(matches!(parse(json!({"action": "env_pull", "project": "p", "branch": "b"})), Request::EnvPull { target: None, branch: Some(_), .. }));
        assert!(matches!(parse(json!({"action": "env_pull", "project": "p", "target": "development"})), Request::EnvPull { target: Some(EnvTarget::Development), branch: None, .. }));
        assert!(matches!(parse(json!({"action": "source_file", "id": "a1b2c3d4", "path": "src/app.tsx"})), Request::SourceFile { .. }));
        let Request::GitPush { repo_id, sha, pusher, .. } =
            parse(json!({"action": "git_push", "project": "p", "repo_id": 7, "branch": "main", "sha": "0123456789abcdef0123456789abcdef01234567"}))
        else {
            panic!("not git_push")
        };
        assert_eq!((repo_id, sha.len(), pusher), (7, 40, None));
        assert!(matches!(parse(json!({"action": "git_branch_deleted", "project": "p", "repo_id": 7, "branch": "feature/logs"})), Request::GitBranchDeleted { repo_id: 7, .. }));
        assert!(matches!(
            parse(json!({"action": "git_pull_request", "project": "p", "repo_id": 7, "number": 12, "branch": "feature/logs"})),
            Request::GitPullRequest { number: 12, .. }
        ));
        assert!(matches!(parse(json!({"action": "git_repos"})), Request::GitRepos { query: None, fresh: false }));
        assert!(matches!(parse(json!({"action": "git_connect", "project": "p", "repo": "ada/engine"})), Request::GitConnect { branch: None, root: None, .. }));
        let Request::ProjectCreate { branch, root, env, .. } = parse(json!({"action": "project_create", "project": "p", "repo": "ada/engine", "env": {"K": "v"}})) else {
            panic!("not project_create")
        };
        assert_eq!((branch, root, env.len()), (None, None, 1));
        let Request::Deploy { production, upload, git, same_source, .. } =
            parse(json!({"action": "deploy", "project": "p", "upload": {"sha256": "ab", "files": 3, "bytes": 10}, "production": true, "via": "cli"}))
        else {
            panic!("not deploy")
        };
        assert_eq!((production, upload.files, git.is_none(), same_source), (true, 3, true, false));
        assert!(matches!(parse(json!({"action": "upload_url", "project": "p", "sha256": "ab", "bytes": 10})), Request::UploadUrl { bytes: 10, .. }));
        let asked = |v: Value| serde_json::from_value::<Invocation>(v).unwrap().via;
        assert_eq!(asked(json!({"action": "ping", "via": "cli"})), Some(Via::Cli));
        assert_eq!(asked(json!({"action": "ping"})), None);
        // A role can't be set through here, whatever the request says.
        assert!(serde_json::from_value::<Invocation>(json!({"action": "set_role", "project": "p", "role": "admin"})).is_err());
        assert!(matches!(parse(json!({"action": "settings", "project": "p", "role": "admin"})), Request::Settings { .. }));
    }

    #[test]
    fn logs_name_env_keys_never_values() {
        let request = parse(json!({"action": "env_set", "project": "p", "values": {"API_KEY": "s3cr3t"}, "targets": ["production"]}));
        let about = about(&request).to_string();
        assert!(about.contains("API_KEY") && !about.contains("s3cr3t"), "{about}");
        let request = parse(json!({"action": "project_create", "project": "p", "repo": "ada/engine", "env": {"API_KEY": "s3cr3t"}}));
        let about = super::about(&request).to_string();
        assert!(about.contains("API_KEY") && about.contains("ada/engine") && !about.contains("s3cr3t"), "{about}");
        let request = parse(json!({"action": "deploy_hook", "project": "p", "id": "aaaaaaaaaaaa", "secret": "hook-s3cr3t", "no_cache": true}));
        let about = super::about(&request).to_string();
        assert!(about.contains("aaaaaaaaaaaa") && !about.contains("hook-s3cr3t"), "{about}");
    }

    #[test]
    fn deploy_hooks_are_members_to_make_and_nobodys_to_call() {
        let may = |role: &str, v: Value| allowed(role, &parse(v), None).is_ok();
        let list = json!({"action": "deploy_hooks", "op": "list", "project": "p"});
        let create = json!({"action": "deploy_hooks", "op": "create", "project": "p", "name": "CMS", "branch": "main"});
        let remove = json!({"action": "deploy_hooks", "op": "remove", "project": "p", "id": "aaaaaaaaaaaa"});
        let call = json!({"action": "deploy_hook", "project": "p", "id": "aaaaaaaaaaaa", "secret": "x"});
        assert!(matches!(parse(call.clone()), Request::DeployHook { no_cache: false, .. }));
        for role in ["owner", "member", "developer", "viewer"] {
            assert!(may(role, list.clone()), "{role}");
        }
        for role in ["owner", "member"] {
            assert!(may(role, create.clone()) && may(role, remove.clone()), "{role}");
        }
        for role in ["developer", "viewer"] {
            assert!(!may(role, create.clone()) && !may(role, remove.clone()) && !may(role, call.clone()), "{role}");
        }
    }

    #[test]
    fn renaming_a_project_is_a_members() {
        let rename = json!({"action": "project_rename", "project": "p", "name": "shop"});
        assert!(matches!(parse(rename.clone()), Request::ProjectRename { ref name, .. } if name == "shop"));
        for (role, ok) in [("owner", true), ("member", true), ("developer", false), ("viewer", false)] {
            assert_eq!(allowed(role, &parse(rename.clone()), None).is_ok(), ok, "{role}");
        }
    }

    #[test]
    fn a_role_is_checked_again_here() {
        let may = |role: &str, v: Value, target: Option<Target>| allowed(role, &parse(v), target).is_ok();
        let (preview, production) = (Some(Target::Preview), Some(Target::Production));
        let promote = json!({"action": "promote", "id": "a1b2c3d4"});
        let redeploy = json!({"action": "redeploy", "id": "a1b2c3d4"});
        let deploy = |production: bool| json!({"action": "deploy", "project": "p", "upload": {"sha256": "ab", "files": 3, "bytes": 10}, "production": production});
        let env = |targets: Value| json!({"action": "env_set", "project": "p", "values": {"K": "v"}, "targets": targets});

        // Members and owners: everything. Viewers: reading only.
        for role in ["owner", "member"] {
            assert!(may(role, promote.clone(), None) && may(role, deploy(true), None) && may(role, env(json!(["production"])), None));
        }
        assert!(!may("viewer", promote.clone(), None) && !may("viewer", deploy(false), None));
        assert!(may("viewer", json!({"action": "source_list", "id": "a1b2c3d4"}), None));
        // A project's firewall: anyone looks; members and owners change. The install's: owners.
        let fw_get = json!({"action": "firewall", "project": "p", "op": "get"});
        let fw_change = json!({"action": "firewall", "project": "p", "op": "change", "changes": [{"action": "firewallEnabled", "value": false}]});
        let fw_install_get = json!({"action": "firewall", "op": "diff"});
        let fw_install_publish = json!({"action": "firewall", "op": "publish"});
        for role in ["owner", "member", "developer", "viewer"] {
            assert!(may(role, fw_get.clone(), None) && may(role, fw_install_get.clone(), None), "{role}");
        }
        assert!(may("owner", fw_change.clone(), None) && may("member", fw_change.clone(), None));
        assert!(!may("developer", fw_change.clone(), None) && !may("viewer", fw_change.clone(), None));
        assert!(may("owner", fw_install_publish.clone(), None));
        for role in ["member", "developer", "viewer"] {
            assert!(!may(role, fw_install_publish.clone(), None), "{role}");
        }
        assert!(matches!(parse(fw_install_get), Request::Firewall { project: None, op: FirewallOp::Diff { .. } }));
        // A project's redirects and rules: anyone looks and tests; members and owners change.
        let list = json!({"action": "redirects", "project": "p", "op": "list"});
        let add = json!({"action": "redirects", "project": "p", "op": "add", "redirects": [{"source": "/a", "destination": "/b", "statusCode": 308}]});
        let test = json!({"action": "routes", "project": "p", "op": "test", "url": "/a"});
        let publish = json!({"action": "routes", "project": "p", "op": "publish"});
        for role in ["viewer", "developer"] {
            assert!(may(role, list.clone(), None) && may(role, test.clone(), None));
            assert!(!may(role, add.clone(), None) && !may(role, publish.clone(), None));
        }
        assert!(may("member", add.clone(), None) && may("owner", publish.clone(), None));
        assert!(matches!(parse(json!({"action": "routes", "project": "p", "op": "move", "id": "rt_1", "position": {"placement": "at", "index": 2}})), Request::Routes { .. }));

        // Schedules: anyone looks; members and owners run and change them (production's).
        let sched_list = json!({"action": "schedules", "op": "list", "project": "p"});
        let sched_run = json!({"action": "schedules", "op": "run", "project": "p", "key": "daily"});
        let sched_off = json!({"action": "schedules", "op": "disable", "project": "p"});
        assert!(matches!(parse(sched_off.clone()), Request::Schedules { op: SchedulesOp::Disable { key: None, .. } }));
        assert!(may("viewer", sched_list.clone(), None) && may("developer", sched_list, None));
        assert!(!may("viewer", sched_run.clone(), None) && !may("developer", sched_run.clone(), None) && !may("developer", sched_off.clone(), None));
        assert!(may("member", sched_run.clone(), None) && may("owner", sched_off, None));
        assert_eq!(about(&parse(sched_run))["key"], "daily");
        // The cache: anyone sees the latest purges; members and owners purge, as on Vercel.
        let cache_ls = json!({"action": "cache", "op": "list", "project": "p"});
        let cache_invalidate = json!({"action": "cache", "op": "invalidate", "project": "p", "tags": ["posts"]});
        let cache_purge = json!({"action": "cache", "op": "purge", "project": "p", "target": "preview"});
        assert!(matches!(parse(cache_invalidate.clone()), Request::Cache { op: CacheOp::Invalidate { .. } }));
        assert!(may("viewer", cache_ls.clone(), None) && may("developer", cache_ls, None));
        for role in ["viewer", "developer"] {
            assert!(!may(role, cache_invalidate.clone(), None) && !may(role, cache_purge.clone(), None));
        }
        assert!(may("member", cache_invalidate.clone(), None) && may("owner", cache_purge, None));
        assert_eq!(about(&parse(cache_invalidate))["op"], "invalidate");
        // Domains: anyone looks; a developer may ask for a check again; members and owners change.
        let domains_list = json!({"action": "domains", "op": "list", "project": "p"});
        let domains_add = json!({"action": "domains", "op": "add", "domain": "api.example.com", "project": "p"});
        let domains_check = json!({"action": "domains", "op": "check", "domain": "api.example.com"});
        let domains_update = json!({"action": "domains", "op": "update", "domain": "www.example.com", "redirect": {"to": "example.com", "status": 301}});
        assert!(matches!(parse(domains_update.clone()), Request::Domains { op: DomainsOp::Update { redirect: Some(Some(_)), branch: None, .. } }));
        assert!(may("viewer", domains_list.clone(), None) && may("viewer", json!({"action": "domains", "op": "inspect", "domain": "a.com"}), None));
        assert!(!may("viewer", domains_check.clone(), None) && !may("viewer", domains_add.clone(), None));
        assert!(may("developer", domains_check, None) && !may("developer", domains_add.clone(), None) && !may("developer", domains_update.clone(), None));
        assert!(may("member", domains_add, None) && may("owner", domains_update.clone(), None));
        let about = about(&parse(domains_update)).to_string();
        assert!(about.contains("www.example.com") && about.contains("update"), "{about}");

        // Protection: anyone looks; members and owners change; the install's default is an owner's.
        let get = json!({"action": "protection", "op": "get", "project": "p"});
        let update = json!({"action": "protection", "op": "update", "project": "p", "scope": "all", "trusted_ips": null});
        let default = json!({"action": "protection", "op": "default", "scope": "standard"});
        assert!(matches!(parse(update.clone()), Request::Protection { op: ProtectionOp::Update { trusted_ips: Some(None), .. } }));
        assert!(may("viewer", get.clone(), None) && may("developer", get, None));
        assert!(!may("viewer", update.clone(), None) && !may("developer", update.clone(), None));
        assert!(may("member", update.clone(), None) && may("owner", update.clone(), None));
        assert!(!may("member", default.clone(), None) && may("owner", default, None));
        assert!(may("developer", json!({"action": "protection", "op": "share_revoke", "project": "p", "id": "abc"}), None));
        let secret = json!({"action": "protection", "op": "bypass_add", "project": "p", "hash": "ab".repeat(32), "secret": "never-logged"});
        let said = super::about(&parse(secret)).to_string();
        assert!(said.contains("bypass_add") && !said.contains("never-logged"), "{said}");

        // Spend: anyone looks; only owners change budgets or pause and resume; nobody says a
        // budget was reached but a router (its events).
        let get = json!({"action": "spend", "op": "get"});
        let budget = json!({"action": "spend", "op": "set_install", "dollars": 20.0});
        let resume = json!({"action": "spend", "op": "resume", "keep": ["a"]});
        assert!(matches!(parse(resume.clone()), Request::Spend { op: SpendOp::Resume { projects: None, .. } }));
        assert!(may("viewer", get.clone(), None) && may("developer", get, None));
        for role in ["viewer", "developer", "member"] {
            assert!(!may(role, budget.clone(), None) && !may(role, resume.clone(), None), "{role}");
        }
        assert!(may("owner", budget, None) && may("owner", resume, None));
        assert!(!may("owner", json!({"action": "spend", "op": "paused", "project": "p", "reason": "project_budget"}), None));

        // A developer: previews and their env vars, never production or the project itself.
        assert!(may("developer", deploy(false), None));
        assert!(!may("developer", deploy(true), None));
        assert!(!may("developer", json!({"action": "deploy", "project": "p", "upload": {"sha256": "ab", "files": 3, "bytes": 10}, "skip_domain": true}), None));
        assert!(may("developer", json!({"action": "upload_url", "project": "p", "sha256": "ab", "bytes": 10}), None));
        assert!(may("developer", redeploy.clone(), preview));
        assert!(!may("developer", redeploy.clone(), production), "a production deployment made again is production");
        assert!(!may("developer", redeploy, None), "an unknown deployment is refused");
        assert!(!may("developer", json!({"action": "redeploy", "id": "a1b2c3d4", "production": true}), preview));
        assert!(may("developer", json!({"action": "build_cancel", "id": "a1b2c3d4"}), preview));
        assert!(!may("developer", json!({"action": "build_cancel", "id": "a1b2c3d4"}), production));
        assert!(may("developer", env(json!(["preview"])), None));
        assert!(!may("developer", env(json!(["preview", "production"])), None));
        assert!(!may("developer", json!({"action": "env_remove", "project": "p", "key": "K", "targets": ["production"]}), None));
        // Development: a developer's to set and read; a viewer reads no values.
        assert!(may("developer", env(json!(["preview", "development"])), None));
        let pull = |target: &str| json!({"action": "env_pull", "project": "p", "target": target});
        for role in ["owner", "member", "developer"] {
            assert!(may(role, pull("development"), None) && may(role, pull("production"), None), "{role}");
        }
        assert!(!may("viewer", pull("development"), None));
        assert!(serde_json::from_value::<Invocation>(json!({"action": "env_pull", "project": "p", "target": "staging"})).is_err());
        // A branch's previews' values: a developer's, as preview's are; a viewer changes none.
        let branch_set = json!({"action": "env_set", "project": "p", "values": {"K": "v"}, "branch": "feature/logs"});
        let branch_rm = json!({"action": "env_remove", "project": "p", "key": "K", "branch": "feature/logs"});
        assert!(may("developer", branch_set.clone(), None) && may("developer", branch_rm.clone(), None));
        assert!(!may("viewer", branch_set, None) && !may("viewer", branch_rm, None));
        assert!(may("developer", json!({"action": "env_pull", "project": "p", "branch": "b"}), None));
        for refused in [
            promote,
            json!({"action": "rollback", "project": "p"}),
            json!({"action": "settings", "project": "p", "memory_mb": 768}),
            json!({"action": "git_connect", "project": "p", "repo": "ada/engine"}),
            json!({"action": "git_disconnect", "project": "p"}),
            json!({"action": "git_repos"}),
            json!({"action": "project_create", "project": "p", "repo": "ada/engine"}),
            json!({"action": "git_branch_deleted", "project": "p", "repo_id": 7, "branch": "feature/logs"}),
            json!({"action": "git_pull_request", "project": "p", "repo_id": 7, "number": 12, "branch": "feature/logs"}),
        ] {
            assert!(!may("developer", refused.clone(), None), "{refused}");
        }
        assert!(may("developer", json!({"action": "source_file", "id": "a1b2c3d4", "path": "a"}), None));

        // A role from a newer dashboard changes nothing; no role is a request nobody's role covers.
        assert!(!may("auditor", deploy(false), None) && may("auditor", json!({"action": "ping"}), None));
        let sent = |v: Value| serde_json::from_value::<Invocation>(v).unwrap().role;
        assert_eq!(sent(json!({"action": "ping", "role": "developer"})).as_deref(), Some("developer"));
        assert_eq!(sent(json!({"action": "git_push", "project": "p", "repo_id": 7, "branch": "main", "sha": "0123456789abcdef0123456789abcdef01234567"})), None);
    }

    #[test]
    fn retention_roles() {
        let may = |role: &str, v: Value, target: Option<Target>| allowed(role, &parse(v), target).is_ok();
        let (preview, production) = (Some(Target::Preview), Some(Target::Production));
        let get = json!({"action": "retention", "op": "get", "project": "p"});
        let plan = json!({"action": "retention", "op": "plan", "project": "p", "change": {"preview": 7}});
        let dry = json!({"action": "retention", "op": "sweep", "dry_run": true});
        let set = json!({"action": "retention", "op": "set", "project": "p", "change": {"preview": "1y"}});
        let install = json!({"action": "retention", "op": "set", "change": {"preview": 7}});
        let sweep_all = json!({"action": "retention", "op": "sweep"});
        let sweep_one = json!({"action": "retention", "op": "sweep", "project": "p"});
        let restore = json!({"action": "retention", "op": "restore", "id": "a1b2c3d4"});
        // Looking and dry runs: anyone.
        for role in ["owner", "member", "developer", "viewer"] {
            assert!(may(role, get.clone(), None) && may(role, plan.clone(), None) && may(role, dry.clone(), None), "{role}");
        }
        // A project's policy and its sweep: members and owners.
        for role in ["owner", "member"] {
            assert!(may(role, set.clone(), None) && may(role, sweep_one.clone(), None) && may(role, restore.clone(), production), "{role}");
        }
        assert!(!may("developer", set.clone(), None) && !may("viewer", set.clone(), None) && !may("developer", sweep_one.clone(), None));
        // The install's default and a sweep of everything: owners only.
        assert!(may("owner", install.clone(), None) && may("owner", sweep_all.clone(), None));
        assert!(!may("member", install.clone(), None) && !may("member", sweep_all.clone(), None));
        // A developer restores a preview, never a production deployment; a viewer nothing.
        assert!(may("developer", restore.clone(), preview) && !may("developer", restore.clone(), production));
        assert!(!may("viewer", restore.clone(), preview));
        // Auto-assign is a project setting: a member's.
        let auto = json!({"action": "settings", "project": "p", "auto_assign_domains": false});
        assert!(may("member", auto.clone(), None) && !may("developer", auto.clone(), None));
        let Request::Settings { auto_assign_domains, .. } = parse(auto) else { panic!("not settings") };
        assert_eq!(auto_assign_domains, Some(Some(false)));
        // Logged without anything but the op, the project and the id.
        let about = about(&parse(restore)).to_string();
        assert!(about.contains("restore") && about.contains("a1b2c3d4"), "{about}");
    }
}
