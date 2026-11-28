//! A deployment's status while it's made, as Vercel shows it: recorded as `building`, in its record
//! and in the project's list, before anything runs, with a build log that the dashboard shows live
//! (report.rs); `ready` once publishing routes it (publish.rs), or `error` or `canceled`.
//!
//! The log's key goes only to whatever writes the log (this process); the record keeps its hash,
//! which the dashboard's builds endpoint checks. The key works while the deployment is building and
//! for a short while after (the endpoint's rule), never again.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::buildlog::{BuildLog, Mask};
use crate::deploy::{Ctx, hosts_for, new_project};
use crate::publish::Destination;
use crate::report::{self, Echo, Endpoint, Report};
use crate::source::Archive;
use crate::state::{self, DeploymentRecord, Failure, GitCommit, HookRef, LogInfo, Project, Status, Summary, Via, now_rfc3339, now_rfc3339_ms};

/// Overrides the install's dashboard address for builds' live logs (a local dashboard, a preview).
pub const DASHBOARD_ENV: &str = "FORKRAIL_DASHBOARD_URL";

/// What's known about a deployment before it's built.
pub struct Begin {
    pub via: Via,
    pub deployer: Option<String>,
    pub git: Option<GitCommit>,
    pub redeploy_of: Option<String>,
    /// The values to hide in the log: the project's env vars for the target.
    pub env: BTreeMap<String, String>,
    pub echo: Echo,
    /// For a remote build: the builder function, and the source it builds.
    pub builder: Option<String>,
    pub upload: Option<Archive>,
    /// For a push: GitHub's deployment for it, made before the build was sent.
    pub github_deployment: Option<u64>,
    /// For a deploy hook's deployment: which hook.
    pub deploy_hook: Option<HookRef>,
}

/// A deployment being made.
pub struct Tracked {
    pub id: String,
    pub project: String,
    pub log: Arc<Report>,
}

/// How it ended.
pub enum End {
    Ready,
    Failed(String),
    Canceled,
}

/// Raised inside a tracked deployment when its user stops it (Ctrl-C).
#[derive(Debug)]
pub struct Canceled;

impl std::fmt::Display for Canceled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("canceled")
    }
}

impl std::error::Error for Canceled {}

/// The S3 key of a deployment's whole build log.
pub fn log_key(project: &str, id: &str) -> String {
    format!("logs/builds/{project}/{id}.ndjson.gz")
}

/// Where the dashboard takes deployment `id`'s lines, if the install has a dashboard.
pub fn endpoint_url(dashboard: Option<&str>, id: &str) -> Option<String> {
    let base = std::env::var(DASHBOARD_ENV).ok().filter(|u| !u.trim().is_empty()).or_else(|| dashboard.map(str::to_owned))?;
    Some(format!("{}/api/builds/{id}/log", base.trim().trim_end_matches('/')))
}

/// Record deployment `id` as building, and start its log.
pub async fn begin(ctx: &Ctx, dest: &Destination, id: &str, how: Begin) -> Result<Tracked> {
    let (mask, echo) = (Mask::new(&how.env), how.echo);
    let (key, record) = insert(ctx, dest, id, how).await?;
    crate::events::deployment(ctx, crate::events::DEPLOYMENT_CREATED, &record).await;
    let endpoint = endpoint_url(dest.settings.dashboard.as_deref(), id).map(|url| Endpoint { url, key });
    if endpoint.is_none() {
        eprintln!("note: the install has no dashboard address (`forkrail dashboard URL`), so the build log is kept only when the deployment ends");
    }
    let log = Report::start(endpoint, mask, echo);
    Ok(Tracked { id: id.to_owned(), project: dest.project.clone(), log })
}

/// Record deployment `id` as building, in its record and its project's list. Returns the key its
/// log is written with, which only the record's hash of it stays behind for. Its `created` event
/// waits for its build to be sent (remote.rs `launch`): a push to a branch whose deployments are
/// off is recorded here and then forgotten.
pub async fn create(ctx: &Ctx, dest: &Destination, id: &str, how: Begin) -> Result<String> {
    Ok(insert(ctx, dest, id, how).await?.0)
}

/// `create`, with the record made.
async fn insert(ctx: &Ctx, dest: &Destination, id: &str, how: Begin) -> Result<(String, DeploymentRecord)> {
    let key = forkrail_common::random_hex(32);
    let created = now_rfc3339();
    let record = DeploymentRecord {
        id: id.to_owned(),
        project: dest.project.clone(),
        target: dest.target,
        created: created.clone(),
        status: Status::Building,
        error: None,
        via: Some(how.via),
        building_at: Some(now_rfc3339_ms()),
        ended_at: None,
        log: Some(LogInfo { key_sha256: forkrail_common::hex(&Sha256::digest(key.as_bytes())), ..Default::default() }),
        function: dest.function.clone(),
        version: String::new(),
        runtime: String::new(),
        entry: String::new(),
        memory_mb: 0,
        max_duration_s: 0,
        concurrent: true,
        source_sha256: String::new(),
        package_sha256: String::new(),
        package_key: String::new(),
        package_bytes: 0,
        reads_sealed_env: false,
        app_aws_keys: false,
        system_env: false,
        env_keys: how.env.keys().cloned().collect(),
        hosts: hosts_for(&dest.project, id, &dest.settings.base_domain, false),
        config: None,
        region: Some(ctx.install.home_region.clone()),
        replicas: BTreeMap::new(),
        framework: None,
        deployer: how.deployer,
        git: how.git,
        static_files: None,
        static_count: None,
        static_bytes: None,
        rules: None,
        images: None,
        role: dest.role(),
        egress: dest.egress(),
        redeploy_of: how.redeploy_of,
        kept_source: None,
        builder: how.builder,
        upload: how.upload,
        github_deployment: how.github_deployment,
        pinned: None,
        importer: None,
        schedules: Vec::new(),
        functions: Vec::new(),
        deploy_hook: how.deploy_hook,
        skew_protection: false,
        skew_id: None,
        outline: None,
        next: None,
        next_router: None,
    };
    if !ctx.store.create(&state::deployment_key(id), &record).await? {
        bail!("a deployment {id} exists already; try again for a new id");
    }
    let summary = Summary { id: id.to_owned(), target: dest.target, created, version: String::new(), status: Status::Building };
    ctx.store
        .update::<Project, _>(&state::project_key(&dest.project), |current| {
            let mut p = current.unwrap_or_else(|| new_project(&dest.project));
            p.deployments.retain(|d| d.id != id);
            p.deployments.insert(0, summary.clone());
            Ok(p)
        })
        .await?;
    Ok((key, record))
}

/// What's known of a log once it ends.
#[derive(Debug, Clone, Default)]
pub struct LogEnd {
    pub lines: u64,
    pub bytes: u64,
    pub cut: bool,
    /// The S3 copy, if it was written.
    pub kept: Option<String>,
}

/// Record how deployment `id` ended: `status` unless publishing made it ready first (then it's
/// ready), and what its log holds. The project's list too, for an end that isn't `ready`.
pub async fn ended(ctx: &Ctx, project: &str, id: &str, status: Status, failure: Option<Failure>, log: LogEnd) -> Result<()> {
    // Whether this write is the one that ended it: then its event goes out.
    let mut ending = false;
    let record = ctx
        .store
        .update::<DeploymentRecord, _>(&state::deployment_key(id), |current| {
            let mut r = current.with_context(|| format!("deployment {id}'s record is gone"))?;
            ending = status != Status::Ready && r.status.in_progress();
            if ending {
                r.status = status;
                r.error = failure.clone();
                r.ended_at = Some(now_rfc3339_ms());
            }
            let mut info = r.log.clone().unwrap_or_default();
            (info.lines, info.bytes, info.cut, info.kept) = (log.lines, log.bytes, log.cut, log.kept.clone());
            r.log = Some(info);
            Ok(r)
        })
        .await?;
    if status != Status::Ready {
        set_summary_status(ctx, project, id, status).await?;
    }
    if ending {
        let kind = if status == Status::Canceled { crate::events::DEPLOYMENT_CANCELED } else { crate::events::DEPLOYMENT_ERROR };
        crate::events::deployment(ctx, kind, &record).await;
    }
    Ok(())
}

/// The project's list entry for a deployment still in progress, set to `status`.
pub async fn set_summary_status(ctx: &Ctx, project: &str, id: &str, status: Status) -> Result<()> {
    ctx.store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = current.context("no such project")?;
            for d in p.deployments.iter_mut().filter(|d| d.id == id && d.status.in_progress()) {
                d.status = status;
            }
            Ok(p)
        })
        .await?;
    Ok(())
}

/// Record how the deployment ended, with its log: the record's status (publishing set `ready`
/// already), the project's list, and the whole log in S3. Best effort past this point: a deployment
/// that published is ready whatever this manages.
pub async fn finish(ctx: &Ctx, tracked: &Tracked, end: End) -> Result<()> {
    let (status, failure) = match &end {
        End::Ready => (Status::Ready, None),
        End::Failed(message) => {
            let message = tracked.log.mask(message);
            tracked.log.info(&format!("Error: {message}"));
            (Status::Error, Some(Failure { step: tracked.log.step_now(), message }))
        }
        End::Canceled => {
            tracked.log.info("Canceled");
            (Status::Canceled, None)
        }
    };
    let closed = tracked.log.close().await;
    let key = log_key(&tracked.project, &tracked.id);
    let kept = match async { ctx.store.put_bytes(&key, report::gzip_lines(&closed.lines)?, "application/gzip").await }.await {
        Ok(()) => Some(key),
        Err(e) => {
            eprintln!("warning: the build log couldn't be kept in S3: {e:#}");
            None
        }
    };
    // Publishing may have routed it before failing at its very end: then it stays ready.
    let log = LogEnd { lines: closed.lines.len() as u64, bytes: closed.bytes, cut: closed.cut, kept };
    ended(ctx, &tracked.project, &tracked.id, status, failure, log).await
}

/// Make deployment `id` with `work`, recorded and logged from start to end. `work` writes its
/// lines to the log it's given; `deploy::log` writes there too while it runs.
pub async fn run<T, F, Fut>(ctx: &Ctx, dest: &Destination, id: &str, how: Begin, work: F) -> Result<T>
where
    F: FnOnce(Arc<Report>) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let tracked = begin(ctx, dest, id, how).await?;
    let result = report::scope(tracked.log.clone(), work(tracked.log.clone())).await;
    let end = match &result {
        Ok(_) => End::Ready,
        Err(e) if e.downcast_ref::<Canceled>().is_some() => End::Canceled,
        Err(e) => End::Failed(format!("{e:#}")),
    };
    if let Err(e) = finish(ctx, &tracked, end).await {
        eprintln!("warning: recording how deployment {id} ended: {e:#}");
    }
    result
}
