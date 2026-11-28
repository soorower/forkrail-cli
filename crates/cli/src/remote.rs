//! Remote builds: a deployment built in the install's builder, the Lambda function
//! `forkrail-builder` (crates/builder, deploy/install/builder.yaml), instead of on the machine it's
//! deployed from. As on Vercel, the build runs in the install's own account, on the Linux and
//! Python that Lambda runs, with its log live in the dashboard and the terminal.
//!
//! How one goes:
//! 1. The source goes up (`upload`): the project's files as a git checkout would have them, to
//!    `uploads/<project>/<sha256>.zip`, encrypted under the env key, where a rebuild finds it again.
//! 2. `start` records the deployment as building, writes its order (the build request, env values
//!    and all, encrypted under the env key) to `builds/<project>/<id>/order.json`, and invokes the
//!    builder asynchronously with URLs presigned for this build's own objects: its order and
//!    source to read, its record to watch for a cancel, its output and its log's copy to write. The
//!    builder's role has no rights over the install, so the project's code running in it has none.
//! 3. The builder builds (builder.rs), sending its lines to the dashboard as a local build does.
//!    It starts from the project's build cache (cache.rs) and writes it back if it changed (a Next
//!    build's `.next/cache` after every build), through URLs presigned for that project's cache
//!    alone.
//! 4. Lambda reports the invocation's end, whatever it was, to the SNS topic `forkrail-builds`,
//!    which the control function listens on (`finished`): it publishes the output, or records why
//!    there's none, then deletes the order and the output.
//!
//! A cancel (`cancel`) marks the record; the builder sees it within seconds and stops.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::InvocationType;
use aws_sdk_s3::primitives::ByteStream;
use forkrail_common::install::{BUILDER_FUNCTION, BUILDER_LARGE_FUNCTION, build_key, cache_key, cancel_key, next_cache_key, upload_key};
use forkrail_common::routes::valid_id;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::analytics::Analytics;
use crate::build::Build;
use crate::buildlog::{self, BuildLog, Mask, Stream, print_line};
use crate::deploy::{self, Ctx, check_project, hosts_for, log};
use crate::git;
use crate::package::TempDir;
use crate::publish::{self, Destination, Files};
use crate::report::{self, Echo, Endpoint, Line, Report};
use crate::source::{self, Archive};
use crate::state::{self, DeploymentRecord, Failure, GitCommit, LargeBuilds, Project, Status, Target, Via, now_ms, now_rfc3339, now_rfc3339_ms};
use crate::track::{self, Begin, LogEnd};
use crate::vercel;

/// How long a build's URLs work: its wait for a free builder (at most 30 minutes, the builder's
/// event age) and its 15 minutes, with room to spare.
const URL_TTL: Duration = Duration::from_secs(2 * 60 * 60);
pub const ORDER_FILE: &str = "order.json";
pub const OUTPUT_FILE: &str = "out.zip";
/// Development only, never read on Lambda: a folder that builds' jobs are written to, in place
/// of invoking the builder.
pub const JOBS_DIR_ENV: &str = "FORKRAIL_BUILDER_JOBS_DIR";
/// How often `follow` looks for new lines and for the deployment's end.
const FOLLOW_EVERY: Duration = Duration::from_millis(700);

/// What the builder is invoked with. The URLs reach only this build's own objects, for `URL_TTL`;
/// the key writes only this deployment's log, and only while it builds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub project: String,
    pub key: String,
    /// The dashboard's endpoint for the lines, if the install has a dashboard.
    #[serde(default)]
    pub log_endpoint: Option<String>,
    pub order_url: String,
    pub source_url: String,
    /// Read every few seconds to see a cancel: the deployment's record as it was canceled
    /// (`state/deployments/<id>.cancel`), missing until then. The builder has no rights of its
    /// own, and the record itself may be in the install's table, out of a URL's reach.
    pub record_url: String,
    pub output_url: String,
    /// The log's copy, `logs/builds/<project>/<id>.ndjson.gz`.
    pub log_url: String,
    /// The project's build cache. None from a forkrail older than the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheUrls>,
    /// A Next.js project's `.next/cache`, kept after every build (cache.rs). None from a forkrail
    /// older than it, whose builder keeps it in `cache` as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cache: Option<CacheUrls>,
    /// When it was sent, Unix milliseconds: how long it waited for a builder.
    pub sent_ms: u64,
    /// A build sent again on the large builder after the first ran out of memory (`again_large`):
    /// why, said in its log, which goes on after the first build's lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub again: Option<String>,
}

/// The project's build cache (cache.rs), as one build may use it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheUrls {
    /// Where to read it, the first that's there: the target's own, then for a preview production's.
    pub read: Vec<String>,
    /// Where to write the target's own.
    pub write: String,
}

/// A build's order: what to build, the project's env vars included, and how to publish it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub request: crate::build::Request,
    pub publish: PublishOptions,
    /// Build without the project's build cache, and write a new one: Vercel's `--force`.
    #[serde(default)]
    pub no_cache: bool,
}

/// Publishing's flags, from the deploy that asked for the build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PublishOptions {
    #[serde(default)]
    pub memory: Option<u32>,
    /// Concurrent compute, under the key the project's setting is kept under: a builder from
    /// before the name reads it.
    #[serde(default, rename = "fluid")]
    pub concurrent: Option<bool>,
    #[serde(default)]
    pub skip_domain: bool,
    #[serde(default)]
    pub same_source: bool,
    #[serde(default)]
    pub redeploy_of: Option<String>,
}

/// How a build ended, as the builder answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub ending: Ending,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
    /// The log: its last line's number and its size, whether it passed 4 MB, and whether its copy
    /// reached S3.
    pub lines: u32,
    pub bytes: u64,
    #[serde(default)]
    pub cut: bool,
    #[serde(default)]
    pub log_kept: bool,
    #[serde(default)]
    pub timings: Timings,
    /// The build's command was killed from outside, as one that ran out of memory is
    /// (`build::OutOfMemory`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub out_of_memory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ending {
    /// Built, and the output is uploaded.
    Built,
    Failed,
    Canceled,
    /// The project's `ignoreCommand` called it off (build.rs): nothing failed, nothing was built.
    Skipped,
}

/// Where a build's time went, in milliseconds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Timings {
    /// From the invoke to the builder starting on it.
    pub queued_ms: u64,
    /// Reading the order and the source.
    pub fetch_ms: u64,
    pub build_ms: u64,
    /// Writing the output.
    pub upload_ms: u64,
    /// Reading and unpacking the build cache, and packing and writing it (0 when it hadn't changed).
    #[serde(default)]
    pub restore_ms: u64,
    #[serde(default)]
    pub keep_ms: u64,
}

/// Zip the project in `dir` for a remote build of `project` and upload it, unless the same source
/// is there already.
pub async fn upload(ctx: &Ctx, dir: &Path, project: &str) -> Result<Archive> {
    let tmp = TempDir::new("upload")?;
    let path = tmp.path().join("source.zip");
    let archive = {
        let (dir, path) = (dir.to_path_buf(), path.clone());
        tokio::task::spawn_blocking(move || source::upload_archive(&dir, &path)).await.context("zipping the source")??
    };
    let zipped = std::fs::metadata(&path)?.len();
    let key = upload_key(project, &archive.sha256);
    if ctx.store.exists(&key).await? {
        log(&format!("source: {} files ({:.1} MB), the same as uploaded before", archive.files, zipped as f64 / 1e6));
        return Ok(archive);
    }
    let kms = ctx.env_key(&ctx.install.home_region).await?;
    let started = Instant::now();
    let body = ByteStream::from_path(&path).await.context("reading the zipped source")?;
    ctx.store.put_sealed(&key, body, "application/zip", &kms).await?;
    log(&format!("source: {} files ({:.1} MB) uploaded in {:.1} s", archive.files, zipped as f64 / 1e6, started.elapsed().as_secs_f64()));
    Ok(archive)
}

/// How long a signed upload address works.
const UPLOAD_URL_TTL: Duration = Duration::from_secs(15 * 60);
/// A source's zip at most: what `source::upload_archive` takes unzipped.
pub const MAX_UPLOAD_ZIP_BYTES: u64 = 1 << 30;

/// Where someone without AWS credentials puts a project's source: a signed address for that one
/// zip, or word that the same source is there already.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadUrl {
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// To send with the PUT, as given.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
}

/// A signed address for `project`'s source whose zip is `bytes` long with the SHA-256 `sha256`:
/// S3 takes that zip there and nothing else, encrypted under the env key as `upload` stores it.
pub async fn upload_url(ctx: &Ctx, project: &str, sha256: &str, bytes: u64) -> Result<UploadUrl> {
    check_project(project)?;
    if sha256.len() != 64 || !sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        bail!("{sha256:?} isn't a SHA-256 in lowercase hex");
    }
    if bytes == 0 || bytes > MAX_UPLOAD_ZIP_BYTES {
        bail!("a source of {bytes} bytes zipped: from 1 byte to {} MB", MAX_UPLOAD_ZIP_BYTES >> 20);
    }
    let key = upload_key(project, sha256);
    if ctx.store.exists(&key).await? {
        return Ok(UploadUrl { exists: true, url: None, headers: Vec::new() });
    }
    let kms = ctx.env_key(&ctx.install.home_region).await?;
    let (url, headers) = ctx.store.presign_put_sealed(&key, UPLOAD_URL_TTL, &kms, sha256, bytes).await?;
    Ok(UploadUrl { exists: false, url: Some(url), headers })
}

/// `forkrail deploy --remote`.
pub struct RemoteDeploy {
    pub dir: PathBuf,
    pub prod: bool,
    pub project: Option<String>,
    pub memory: Option<u32>,
    pub concurrent: Option<bool>,
    pub same_source: bool,
    pub skip_domain: bool,
    pub no_cache: bool,
}

/// Upload the project in `dir`, build it in the builder and show its log until it ends.
pub async fn deploy(ctx: &Ctx, args: RemoteDeploy) -> Result<()> {
    let dir = args.dir.canonicalize().with_context(|| format!("{}", args.dir.display()))?;
    // Read here too, so a vercel.json the builder would refuse fails before anything's uploaded.
    let spec = vercel::read(&dir)?;
    let project = match args.project {
        Some(name) => name,
        None => deploy::project_from_dir(&dir)?,
    };
    check_project(&project)?;
    let target = if args.prod { Target::Production } else { Target::Preview };
    log(&format!("{project}: {}, {}, built in the install's builder", spec.describe(), target.as_str()));
    let upload = upload(ctx, &dir, &project).await?;
    let started = start(
        ctx,
        Start {
            project,
            target,
            upload,
            git: git::commit(&dir),
            deployer: git::deployer(&dir),
            publish: PublishOptions { memory: args.memory, concurrent: args.concurrent, skip_domain: args.skip_domain, same_source: args.same_source, redeploy_of: None },
            via: Via::Cli,
            no_cache: args.no_cache,
            github_deployment: None,
            previous_sha: None,
        },
    )
    .await?;
    watch(ctx, &started).await
}

/// Say where a remote build can be seen, show its log until it ends, and say how it ended: its
/// URLs on stdout once ready, an error if it failed or was canceled.
pub async fn watch(ctx: &Ctx, started: &Started) -> Result<()> {
    let settings = ctx.store.settings().await?;
    let id = &started.id;
    match &settings.dashboard {
        Some(base) => log(&format!("deployment {id} is building: {}/projects/{}/deployments/{id}", base.trim_end_matches('/'), started.project)),
        None => log(&format!("deployment {id} is building")),
    }
    let rec = follow(ctx, started).await?;
    match rec.status_now() {
        Status::Ready => {
            let took = match (rec.building_at.as_deref().and_then(state::parse_rfc3339_ms), rec.ended_at.as_deref().and_then(state::parse_rfc3339_ms)) {
                (Some(from), Some(to)) => format!(" in {:.1} s", to.saturating_sub(from) as f64 / 1000.0),
                _ => String::new(),
            };
            log(&format!("deployment {id} is ready{took}"));
            for host in &rec.hosts {
                println!("{}", settings.url(host));
            }
            if started.target == Target::Preview
                && let Ok(project) = deploy::project_record(ctx, &started.project).await
            {
                log(&deploy::preview_ready_note(&started.project, id, project.production.is_some()));
            }
            Ok(())
        }
        Status::Error => {
            let error = rec.error.as_ref();
            let step = error.and_then(|e| e.step.as_deref()).map(|s| format!(" ({})", s.to_lowercase())).unwrap_or_default();
            bail!("deployment {id} failed{step}: {}", error.map_or("no reason recorded", |e| e.message.as_str()))
        }
        Status::Canceled => match rec.error.as_ref() {
            Some(why) => bail!("deployment {id} was canceled: {}", why.message),
            None => bail!("deployment {id} was canceled"),
        },
        // Ctrl-C: it goes on without us.
        Status::Building | Status::Queued => Ok(()),
    }
}

/// A remote build to start.
pub struct Start {
    pub project: String,
    pub target: Target,
    /// The source, uploaded already (`upload`).
    pub upload: Archive,
    pub git: Option<GitCommit>,
    pub deployer: Option<String>,
    pub publish: PublishOptions,
    pub via: Via,
    /// Build without the project's build cache.
    pub no_cache: bool,
    /// For a push: GitHub's deployment for it (connect.rs), kept on the record.
    pub github_deployment: Option<u64>,
    /// For a push: the commit its branch was at before, which the build gets as
    /// VERCEL_GIT_PREVIOUS_SHA.
    pub previous_sha: Option<String>,
}

/// A remote build started.
#[derive(Debug, Clone, Serialize)]
pub struct Started {
    pub id: String,
    pub project: String,
    pub target: Target,
    /// The deployment's own URL, once it's ready.
    pub url: String,
}

/// Record a deployment as building and send its build to the builder.
pub async fn start(ctx: &Ctx, s: Start) -> Result<Started> {
    check_project(&s.project)?;
    refuse_paused(ctx, &s.project).await?;
    if s.publish.skip_domain && s.target != Target::Production {
        bail!("--skip-domain is for production deployments");
    }
    let source_key = upload_key(&s.project, &s.upload.sha256);
    if !ctx.store.exists(&source_key).await? {
        bail!("no source at {source_key}: upload it first");
    }
    let reserved = reserve(
        ctx,
        Reserve {
            project: s.project.clone(),
            target: s.target,
            via: s.via,
            deployer: s.deployer.clone(),
            git: s.git.clone(),
            redeploy_of: s.publish.redeploy_of.clone(),
            upload: Some(s.upload.clone()),
            github_deployment: s.github_deployment,
            deploy_hook: None,
        },
    )
    .await?;
    match launch(ctx, &reserved, s).await? {
        Launched::Sent(started) | Launched::Stopped(started) => Ok(started),
    }
}

/// A deployment to record before its source is ready.
pub struct Reserve {
    pub project: String,
    pub target: Target,
    pub via: Via,
    pub deployer: Option<String>,
    pub git: Option<GitCommit>,
    pub redeploy_of: Option<String>,
    pub upload: Option<Archive>,
    pub github_deployment: Option<u64>,
    pub deploy_hook: Option<state::HookRef>,
}

/// A deployment recorded as building whose build hasn't been sent yet (`launch`).
pub struct Reserved {
    pub id: String,
    key: String,
    dest: Destination,
}

impl Reserved {
    /// Its page and log go by this.
    pub fn started(&self) -> Started {
        let host = hosts_for(&self.dest.project, &self.id, &self.dest.settings.base_domain, false).remove(0);
        Started { url: self.dest.settings.url(&host), id: self.id.clone(), project: self.dest.project.clone(), target: self.dest.target }
    }
}

/// Record a deployment as building, before its source is uploaded: a push's shows in the list
/// while GitHub's archive of the commit is fetched (connect.rs). `launch` sends its build; `abandon`
/// or `forget` end it if that never happens.
pub async fn reserve(ctx: &Ctx, r: Reserve) -> Result<Reserved> {
    check_project(&r.project)?;
    let dest = Destination::read(ctx, &r.project, r.target, r.git.as_ref()).await?;
    // Chosen first, so the build knows its URL.
    let id = forkrail_common::random_hex(4);
    let how = Begin {
        via: r.via,
        deployer: r.deployer,
        git: r.git,
        redeploy_of: r.redeploy_of,
        env: dest.env(),
        echo: Echo::Masked,
        // A project one of whose builds ran out of the builder's memory builds on the large one.
        builder: Some(builder_for(dest.current.as_ref()).to_owned()),
        upload: r.upload,
        github_deployment: r.github_deployment,
        deploy_hook: r.deploy_hook,
    };
    let key = track::create(ctx, &dest, &id, how).await?;
    Ok(Reserved { id, key, dest })
}

/// Send a recorded deployment's build to the builder, with its source (uploaded already). If it
/// was canceled meanwhile, nothing is sent. A failure to send is recorded on the deployment.
pub async fn launch(ctx: &Ctx, r: &Reserved, s: Start) -> Result<Launched> {
    let id = &r.id;
    let source_key = upload_key(&s.project, &s.upload.sha256);
    let updated = ctx
        .store
        .update::<DeploymentRecord, _>(&state::deployment_key(id), |current| {
            let mut rec = current.with_context(|| format!("deployment {id}'s record is gone"))?;
            rec.upload = Some(s.upload.clone());
            rec.github_deployment = rec.github_deployment.or(s.github_deployment);
            Ok(rec)
        })
        .await;
    let rec = match updated {
        Ok(rec) => rec,
        Err(e) => {
            abandon(ctx, r, &format!("the build couldn't be recorded: {e:#}")).await;
            return Err(e);
        }
    };
    if !rec.status_now().in_progress() {
        log(&format!("deployment {id} is {} already: its build wasn't sent", rec.status_now().as_str()));
        cleanup(ctx, &s.project, id).await;
        return Ok(Launched::Stopped(r.started()));
    }
    let mut request = r.dest.request(id);
    request.git = s.git.clone();
    request.deployer = s.deployer.clone();
    // Only a push's build asks the project's ignoreCommand: one asked for by hand is built.
    request.pushed = s.via == Via::Git;
    request.previous_sha = s.previous_sha.clone();
    let function = rec.builder.clone().unwrap_or_else(|| BUILDER_FUNCTION.to_owned());
    if let Err(e) = send(ctx, &r.dest, &function, id, &r.key, &source_key, Order { request, publish: s.publish, no_cache: s.no_cache }).await {
        abandon(ctx, r, &format!("the build couldn't be sent to the builder: {e:#}")).await;
        return Err(e.context(format!("starting deployment {id}'s build")));
    }
    crate::events::deployment(ctx, crate::events::DEPLOYMENT_CREATED, &rec).await;
    Ok(Launched::Sent(r.started()))
}

/// What `launch` came to.
pub enum Launched {
    /// The build is on its way to the builder.
    Sent(Started),
    /// It was canceled (or ended) while its source was readied: nothing was sent.
    Stopped(Started),
}

/// Whether a recorded deployment was canceled (or ended) while its source was being readied.
pub async fn was_stopped(ctx: &Ctx, r: &Reserved) -> Result<bool> {
    let rec = deploy::record(ctx, &r.id).await?;
    Ok(!rec.status_now().in_progress())
}

/// A recorded deployment whose build never started: it ends as failed, saying why.
pub async fn abandon(ctx: &Ctx, r: &Reserved, message: &str) {
    let failure = Failure { step: None, message: message.to_owned() };
    if let Err(e) = track::ended(ctx, &r.dest.project, &r.id, Status::Error, Some(failure), LogEnd::default()).await {
        eprintln!("warning: recording that deployment {} failed: {e:#}", r.id);
    }
    cleanup(ctx, &r.dest.project, &r.id).await;
}

/// A recorded deployment that shouldn't have been (a push to a branch whose deployments are
/// turned off): its record and its line in the project go, as if it never was.
pub async fn forget(ctx: &Ctx, r: &Reserved) -> Result<()> {
    let id = r.id.as_str();
    ctx.store
        .update::<Project, _>(&state::project_key(&r.dest.project), |current| {
            let mut p = current.context("no such project")?;
            p.deployments.retain(|d| d.id != id);
            Ok(p)
        })
        .await?;
    cleanup(ctx, &r.dest.project, id).await;
    // A newer push may have canceled it meanwhile: its marker goes too.
    if let Err(e) = ctx.store.delete(&cancel_key(id)).await {
        eprintln!("warning: deleting {}: {e:#}", cancel_key(id));
    }
    ctx.store.delete(&state::deployment_key(id)).await
}

/// Write the order, presign the build's URLs and invoke the builder.
/// The builder a project's builds run on: the large one once a build of its ran out of the
/// builder's memory (`again_large`).
fn builder_for(project: Option<&Project>) -> &'static str {
    if project.is_some_and(|p| p.large_builds.is_some()) { BUILDER_LARGE_FUNCTION } else { BUILDER_FUNCTION }
}

/// Write the build's order and send its job to `function`.
#[allow(clippy::too_many_arguments)]
async fn send(ctx: &Ctx, dest: &Destination, function: &str, id: &str, key: &str, source_key: &str, order: Order) -> Result<()> {
    let project = &dest.project;
    let kms = ctx.env_key(&ctx.install.home_region).await?;
    let order_key = build_key(project, id, ORDER_FILE);
    ctx.store.put_sealed(&order_key, ByteStream::from(serde_json::to_vec(&order)?), "application/json", &kms).await?;
    let job = job_for(ctx, project, dest.target, dest.settings.dashboard.as_deref(), id, key, source_key).await?;
    invoke(ctx, function, &job).await
}

/// A build's job, its order already written: URLs to this build's own objects, fresh for `URL_TTL`.
async fn job_for(ctx: &Ctx, project: &str, target: Target, dashboard: Option<&str>, id: &str, key: &str, source_key: &str) -> Result<Job> {
    let order_key = build_key(project, id, ORDER_FILE);
    Ok(Job {
        id: id.to_owned(),
        project: project.to_owned(),
        key: key.to_owned(),
        log_endpoint: track::endpoint_url(dashboard, id),
        order_url: ctx.store.presign_get(&order_key, URL_TTL).await?,
        source_url: ctx.store.presign_get(source_key, URL_TTL).await?,
        record_url: ctx.store.presign_get(&cancel_key(id), URL_TTL).await?,
        output_url: ctx.store.presign_put(&build_key(project, id, OUTPUT_FILE), URL_TTL).await?,
        log_url: ctx.store.presign_put(&track::log_key(project, id), URL_TTL).await?,
        // A preview reads only previews' caches: production's dependency archive also holds
        // framework caches (Gatsby's, Nuxt's, Eleventy's fetches, and before the split Next's
        // `.next/cache`) that production's build filled with production's env vars.
        cache: Some(cache_urls(ctx, project, target, cache_key, false).await?),
        // Sent whatever the project is: only the builder knows it's Next.js.
        // Never production's for a preview: it holds production's Server Actions key (`.rscinfo`) and
        // what production's build fetched with production's env vars.
        next_cache: Some(cache_urls(ctx, project, target, next_cache_key, false).await?),
        sent_ms: now_ms(),
        again: None,
    })
}

/// No build for a paused project (spend protection: its budget or the install's is reached, or a
/// person paused it): a build costs too, and the routers would serve nothing of it.
async fn refuse_paused(ctx: &Ctx, project: &str) -> Result<()> {
    let routes = ctx.store.get::<forkrail_common::routes::Routes>(state::ROUTES_KEY).await?.map(|(r, _)| r).unwrap_or_default();
    if routes.projects.get(project).and_then(|p| p.spend.as_ref()).is_some_and(|s| s.paused.is_some()) {
        bail!("{project} is paused (spend protection), so nothing of it builds: raise the budget and resume it first (Settings → Spend, or `forkrail spend resume {project}`)");
    }
    Ok(())
}

/// The longest a builder runs (deploy/install/builder.yaml's `Timeout`, 900 s), with a minute
/// to spare: no build is counted longer.
const MAX_BUILDER_RUN_MS: u64 = 16 * 60 * 1000;

/// Count what one builder run cost (its memory from when control sent the job until its end was
/// reported, so a little over) toward the project's month. A failure is said, never fatal.
///
/// The job comes back in the notice, which a build's own code can send (`finished`): its send
/// time is taken no earlier than the deployment's record was made, and the run no longer than a
/// builder can run, so a made-up notice can't claim a cost that pauses the install.
async fn count_cost(ctx: &Ctx, job: &Job, builder: &str, recorded_ms: Option<u64>) {
    use forkrail_common::prices::{self, Meter};
    let sent = job.sent_ms.max(recorded_ms.unwrap_or(0));
    let ms = now_ms().saturating_sub(sent).min(MAX_BUILDER_RUN_MS);
    let (mb, x86) = match ctx.lambda.client.get_function_configuration().function_name(builder).send().await {
        Ok(c) => (
            c.memory_size().map_or(4096, |m| m.max(128) as u64),
            !c.architectures().iter().any(|a| *a == aws_sdk_lambda::types::Architecture::Arm64),
        ),
        Err(_) => (if builder == BUILDER_LARGE_FUNCTION { 8192 } else { 4096 }, true),
    };
    let region = ctx.install.home_region.as_str();
    let meter = if x86 { Meter::FnX86MbMs } else { Meter::FnMbMs };
    let cost_n = prices::cost(meter, region, None, ms * mb) + prices::cost(Meter::FnRequests, region, None, 1);
    let op = crate::spend::SpendOp::Build {
        project: job.project.clone(),
        build: format!("{}:{builder}", job.id),
        month: prices::month_of(state::now_secs()),
        cost_n,
    };
    if let Err(e) = crate::spend::run(ctx, op).await {
        eprintln!("warning: deployment {}'s build cost wasn't counted toward its budget: {e:#}", job.id);
    }
}

/// Hand `job` to the builder `function`, asynchronously: Lambda reports its end on the builds topic.
async fn invoke(ctx: &Ctx, function: &str, job: &Job) -> Result<()> {
    // Away from Lambda only, for trying a change before it's released: the job is left in a
    // folder for a builder run by hand (`forkrail-builder --job FILE`), and no builder is invoked.
    if std::env::var_os("AWS_LAMBDA_FUNCTION_NAME").is_none()
        && let Some(dir) = std::env::var_os(JOBS_DIR_ENV).filter(|d| !d.is_empty())
    {
        let path = Path::new(&dir).join(format!("{}.json", job.id));
        std::fs::write(&path, serde_json::to_vec_pretty(job)?).with_context(|| format!("writing {}", path.display()))?;
        eprintln!("note: {JOBS_DIR_ENV} is set, so no builder was invoked: the job for {function} is in {}", path.display());
        return Ok(());
    }
    let sent = ctx
        .lambda
        .client
        .invoke()
        .function_name(function)
        .invocation_type(InvocationType::Event)
        .payload(Blob::new(serde_json::to_vec(job)?))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("invoking {function}: {}", aws_sdk_lambda::error::DisplayErrorContext(&e)))?;
    if sent.status_code() != 202 {
        bail!("{function} answered {} to an asynchronous invoke", sent.status_code());
    }
    Ok(())
}

/// One of the project's build caches, named by `key`, as a build for `target` uses it: the target's
/// own, and for a preview production's while it has none of its own when `fallback`. A production
/// build reads only what production builds wrote.
async fn cache_urls(ctx: &Ctx, project: &str, target: Target, key: fn(&str, &str) -> String, fallback: bool) -> Result<CacheUrls> {
    let own = key(project, target.as_str());
    let mut read = vec![ctx.store.presign_get(&own, URL_TTL).await?];
    if fallback && target != Target::Production {
        read.push(ctx.store.presign_get(&key(project, Target::Production.as_str()), URL_TTL).await?);
    }
    Ok(CacheUrls { read, write: ctx.store.presign_put(&own, URL_TTL).await? })
}

/// Stop a remote build: its record says canceled at once, and the builder stops within seconds.
pub async fn cancel(ctx: &Ctx, id: &str) -> Result<DeploymentRecord> {
    let rec = deploy::record(ctx, id).await?;
    if rec.builder.is_none() && rec.status_now().in_progress() {
        bail!("deployment {id} is building where it was deployed from, not in the builder: stop it there (Ctrl-C)");
    }
    let rec = ctx
        .store
        .update::<DeploymentRecord, _>(&state::deployment_key(id), |current| {
            let mut r = current.with_context(|| format!("deployment {id}'s record is gone"))?;
            let status = r.status_now();
            if !status.in_progress() {
                bail!("deployment {id} is {} already", status.as_str());
            }
            r.status = Status::Canceled;
            r.error = None;
            r.ended_at = Some(now_rfc3339_ms());
            Ok(r)
        })
        .await?;
    // What the builder looks for. A builder from before the marker reads the record itself, which
    // says the same while the bucket keeps a copy of each record.
    ctx.store.put_bytes(&cancel_key(id), serde_json::to_vec(&rec)?, "application/json").await?;
    track::set_summary_status(ctx, &rec.project, id, Status::Canceled).await?;
    crate::events::deployment(ctx, crate::events::DEPLOYMENT_CANCELED, &rec).await;
    Ok(rec)
}

/// The end of a builder invocation, as Lambda reports it on the builds topic: publish what was
/// built, or record why there's nothing. Returns what was done, for the control function's log.
///
/// Anything that doesn't match its deployment's record (an unknown id, a key whose hash isn't the
/// record's) is refused: code running in the builder can publish to the topic too, but it knows
/// only its own build's key, so it can end only its own build.
pub async fn finished(ctx: &Ctx, notice: &Value) -> Result<String> {
    let rejected = |why: String| anyhow::Error::new(Rejected(why));
    let job: Job = serde_json::from_value(notice.get("requestPayload").cloned().context("no requestPayload")?).map_err(|e| rejected(format!("requestPayload isn't a build: {e}")))?;
    if !valid_id(&job.id) {
        return Err(rejected(format!("{:?} isn't a deployment id", job.id)));
    }
    check_project(&job.project).map_err(|e| rejected(format!("{e:#}")))?;
    let Some(rec) = ctx.store.get_value::<DeploymentRecord>(&state::deployment_key(&job.id)).await? else {
        // Its project was deleted while it built (project.rs `delete`): what the builder left goes
        // too, and the project's build cache if the builder wrote one after the delete, so a new
        // project of the same name never starts from it. Nothing here names another deployment's.
        cleanup(ctx, &job.project, &job.id).await;
        let mut gone = vec![track::log_key(&job.project, &job.id)];
        if ctx.store.get_value::<Project>(&state::project_key(&job.project)).await?.is_none() {
            gone.extend(ctx.store.bucket_list(&format!("cache/{}/", job.project)).await?);
        }
        for key in gone {
            if let Err(e) = ctx.store.delete(&key).await {
                eprintln!("warning: deleting {key}: {e:#}");
            }
        }
        return Err(rejected(format!("no deployment {} (its project was deleted?): the build's files are deleted", job.id)));
    };
    let hash = forkrail_common::hex(&Sha256::digest(job.key.as_bytes()));
    if rec.project != job.project || rec.builder.is_none() || rec.log.as_ref().map(|l| l.key_sha256.as_str()) != Some(hash.as_str()) {
        return Err(rejected(format!("the notice for deployment {} doesn't match its record", job.id)));
    }
    // Which builder this is the end of (Lambda's record names the function). A build sent again on
    // the large builder is that builder's to end: a late or repeated notice of its first build
    // changes nothing, and leaves the build's files to the second.
    let from = notice.pointer("/requestContext/functionArn").and_then(Value::as_str).and_then(function_of);
    if let (Some(from), Some(builder)) = (from, rec.builder.as_deref())
        && from != builder
    {
        return Ok(format!("the end of deployment {}'s build on {from}, which builds on {builder} now: left to it", job.id));
    }
    // What this run of the builder cost counts toward the project's month (spend.rs).
    if let Some(builder) = from.or(rec.builder.as_deref()) {
        count_cost(ctx, &job, builder, state::parse_rfc3339_ms(&rec.created)).await;
    }
    let condition = notice.pointer("/requestContext/condition").and_then(Value::as_str).unwrap_or("");
    let function_error = notice.pointer("/responseContext/functionError").and_then(Value::as_str);
    let outcome = match (condition, function_error) {
        ("Success", None) => notice.get("responsePayload").and_then(|p| serde_json::from_value::<Outcome>(p.clone()).ok()),
        _ => None,
    };
    // Out of memory on the builder: once more on the large one, never again after that.
    let oom = match &outcome {
        Some(o) => o.ending == Ending::Failed && o.out_of_memory,
        None => out_of_memory(notice.get("responsePayload")),
    };
    let mut note = None;
    // A new account's memory limit (Settings::memory_limits) leaves the builder small and makes no
    // large builder: the advice says so.
    let limit = match oom {
        true => ctx.store.settings().await.ok().and_then(|s| s.memory_limits.get(&ctx.install.home_region).copied()).filter(|limit| *limit <= LARGE_BUILDER_ABOVE_MB),
        false => None,
    };
    if let Some(limit) = limit {
        note = Some(account_limit_note(limit));
    } else if oom && rec.status_now().in_progress() && rec.builder.as_deref() == Some(BUILDER_FUNCTION) {
        match again_large(ctx, &job, &rec).await {
            Ok(done) => return Ok(done),
            Err(e) => note = Some(format!("building it again on the large builder failed: {e:#}")),
        }
    }
    let on_large = rec.builder.as_deref() == Some(BUILDER_LARGE_FUNCTION);
    let done = match outcome {
        Some(mut outcome) => {
            if oom && let Some(error) = outcome.error.as_mut() {
                error.message = format!("{}; {}", error.message, memory_advice(on_large, note.as_deref()));
            }
            ended_with(ctx, &job, &rec, &outcome).await
        }
        None => {
            // No answer: the builder crashed, ran out of time or memory, or never got to it. Its
            // lines are in ClickHouse, which the dashboard keeps showing.
            let mut message = stopped(condition, notice.get("responsePayload"), on_large);
            if let Some(note) = &note {
                message = format!("{message} ({note})");
            }
            if rec.status.in_progress() {
                track::ended(ctx, &job.project, &job.id, Status::Error, Some(Failure { step: None, message: message.clone() }), LogEnd::default())
                    .await
                    .map(|()| format!("failed: {message}"))
            } else {
                Ok(format!("{} already; the builder stopped: {message}", rec.status.as_str()))
            }
        }
    };
    cleanup(ctx, &job.project, &job.id).await;
    done
}

/// A builds-topic notice `finished` turned away before acting on it: not one of a build's own
/// (whatever published it), so nothing more is done for the deployment it names.
#[derive(Debug)]
pub struct Rejected(pub String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

/// The builder's error, from Lambda's answer for an invocation that didn't return.
fn error_of(response: Option<&Value>) -> (String, String) {
    let text = |key: &str| response.and_then(|r| r.get(key)).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    (text("errorType"), text("errorMessage"))
}

/// Lambda stopped the builder for running out of its memory.
fn out_of_memory(response: Option<&Value>) -> bool {
    let (kind, message) = error_of(response);
    kind.contains("OutOfMemory") || message.contains("out of memory")
}

/// The function an ARN names (`arn:aws:lambda:REGION:ACCOUNT:function:NAME[:QUALIFIER]`).
fn function_of(arn: &str) -> Option<&str> {
    arn.split(':').nth(6).filter(|name| !name.is_empty())
}

/// The builder's memory (deploy/install/builder.yaml); a large builder exists only where the
/// account allows more than this.
const LARGE_BUILDER_ABOVE_MB: u32 = 4096;

/// Why a build had little memory on a new account.
fn account_limit_note(limit: u32) -> String {
    format!("this AWS account lets each function use only {limit} MB for now, the builder too; AWS raises the limit as the account gets used, or sooner through a support case")
}

/// What a build that ran out of memory with nowhere bigger to go can do.
fn memory_advice(on_large: bool, note: Option<&str>) -> String {
    let advice = if on_large {
        "it ran out on the large builder too: build it with `forkrail deploy --local` on a machine with more memory, or give the large builder more (deploy/install/builder.yaml: LargeMemorySize, at most Lambda's 10,240 MB)"
    } else {
        "build it with `forkrail deploy --local` on a machine with more memory, or give the builder more (deploy/install/builder.yaml: MemorySize, at most Lambda's 10,240 MB)"
    };
    match note {
        Some(note) => format!("{advice} ({note})"),
        None => advice.to_owned(),
    }
}

/// What a build sent again on the large builder says first, its log going on after the first build's.
const AGAIN: &str = "The build ran out of the builder's memory: building it again on the large builder, where this project's next builds start too";

/// A build that ran out of the builder's memory, sent once to the large builder: the same
/// deployment, key, source and order, with fresh URLs, its log going on after the first build's
/// lines. The record names the large builder before it's sent, so a repeated notice of the first
/// build is left alone (`finished`), and one that runs out there too fails. The project's next
/// builds start on the large builder (`Project::large_builds`).
async fn again_large(ctx: &Ctx, job: &Job, rec: &DeploymentRecord) -> Result<String> {
    let id = job.id.clone();
    let source = rec.upload.as_ref().map(|u| upload_key(&job.project, &u.sha256)).context("its source isn't recorded")?;
    let key = state::deployment_key(&id);
    let moved = |to: &'static str, from: &'static str| {
        let id = id.clone();
        move |current: Option<DeploymentRecord>| {
            let mut r = current.with_context(|| format!("deployment {id}'s record is gone"))?;
            if r.builder.as_deref() != Some(from) {
                bail!("deployment {id} builds on {} already", r.builder.as_deref().unwrap_or("this machine"));
            }
            r.builder = Some(to.to_owned());
            Ok(r)
        }
    };
    // Paused while it built (its budget reached): no second build.
    refuse_paused(ctx, &job.project).await?;
    ctx.store.update::<DeploymentRecord, _>(&key, moved(BUILDER_LARGE_FUNCTION, BUILDER_FUNCTION)).await?;
    let sent = async {
        let settings = ctx.store.settings().await?;
        let mut again = job_for(ctx, &job.project, rec.target, settings.dashboard.as_deref(), &id, &job.key, &source).await?;
        again.again = Some(AGAIN.to_owned());
        invoke(ctx, BUILDER_LARGE_FUNCTION, &again).await
    }
    .await;
    if let Err(e) = sent {
        // Back on the builder, whose failure it then is.
        if let Err(back) = ctx.store.update::<DeploymentRecord, _>(&key, moved(BUILDER_FUNCTION, BUILDER_LARGE_FUNCTION)).await {
            eprintln!("warning: putting deployment {id} back on {BUILDER_FUNCTION}: {back:#}");
        }
        return Err(e);
    }
    let marked = ctx
        .store
        .update::<Project, _>(&state::project_key(&job.project), |current| {
            let mut p = current.context("no such project")?;
            p.large_builds.get_or_insert_with(|| LargeBuilds { since: now_rfc3339(), deployment: id.clone() });
            Ok(p)
        })
        .await;
    if let Err(e) = marked {
        eprintln!("warning: noting that {}'s builds need the large builder: {e:#}", job.project);
    }
    Ok(format!("ran out of {BUILDER_FUNCTION}'s memory: sent again to {BUILDER_LARGE_FUNCTION}"))
}

/// What a builder that didn't answer stopped of.
fn stopped(condition: &str, response: Option<&Value>, on_large: bool) -> String {
    let (kind, message) = error_of(response);
    if condition == "EventAgeExceeded" {
        return "no builder was free for 30 minutes; try again, or build with --local".to_owned();
    }
    if out_of_memory(response) {
        return match on_large {
            // The advice says it ran out there too.
            true => memory_advice(true, None).replacen("it ran out", "the build ran out of the builder's memory, and", 1),
            false => format!("the build ran out of the builder's memory; {}", memory_advice(false, None)),
        };
    }
    if kind.contains("Timedout") || message.contains("timed out") {
        return format!("the builder stopped at Lambda's 15 minutes ({message}); build with --local for longer builds");
    }
    match (kind.is_empty(), message.is_empty()) {
        (true, true) => format!("the builder stopped without answering ({})", if condition.is_empty() { "no condition" } else { condition }),
        (false, true) => format!("the builder stopped: {kind}"),
        _ => format!("the builder stopped: {message}"),
    }
}

/// The builder answered: publish, or record the failure or the cancel.
async fn ended_with(ctx: &Ctx, job: &Job, rec: &DeploymentRecord, outcome: &Outcome) -> Result<String> {
    let kept = outcome.log_kept.then(|| track::log_key(&job.project, &job.id));
    let log = LogEnd { lines: outcome.lines.into(), bytes: outcome.bytes, cut: outcome.cut, kept };
    let t = &outcome.timings;
    let timings = format!(
        "queued {} ms, fetch {} ms, cache {} ms, build {} ms, upload {} ms, keeping the cache {} ms",
        t.queued_ms, t.fetch_ms, t.restore_ms, t.build_ms, t.upload_ms, t.keep_ms
    );
    let status = rec.status_now();
    match outcome.ending {
        // Canceled while it was finishing: nothing is published.
        Ending::Built if status == Status::Canceled => {
            track::ended(ctx, &job.project, &job.id, Status::Canceled, None, log).await?;
            Ok(format!("built, but canceled first ({timings})"))
        }
        Ending::Built if status.in_progress() => {
            publish_built(ctx, job, rec, outcome).await?;
            Ok(format!("published ({timings})"))
        }
        Ending::Built => Ok(format!("built, but {} already", status.as_str())),
        Ending::Failed => {
            track::ended(ctx, &job.project, &job.id, Status::Error, outcome.error.clone(), log).await?;
            Ok(format!("failed: {} ({timings})", outcome.error.as_ref().map_or("?", |e| e.message.as_str())))
        }
        Ending::Canceled => {
            track::ended(ctx, &job.project, &job.id, Status::Canceled, None, log).await?;
            Ok(format!("canceled ({timings})"))
        }
        // Called off by the project itself: canceled, with the reason where a failure's would be.
        Ending::Skipped => {
            let why = Failure { step: None, message: crate::build::IGNORED.to_owned() };
            track::ended(ctx, &job.project, &job.id, Status::Canceled, Some(why), log).await?;
            Ok(format!("skipped by ignoreCommand ({timings})"))
        }
    }
}

/// Publish the builder's output, its lines going on in the deployment's log after the builder's.
async fn publish_built(ctx: &Ctx, job: &Job, rec: &DeploymentRecord, outcome: &Outcome) -> Result<()> {
    let order: Order = ctx.store.get_value(&build_key(&job.project, &job.id, ORDER_FILE)).await?.context("the build's order is gone")?;
    let env = order.request.env.clone();
    // Publishing's lines go to the dashboard with the build's key, after the builder's.
    let settings = ctx.store.settings().await?;
    let endpoint = track::endpoint_url(settings.dashboard.as_deref(), &job.id).map(|url| Endpoint { url, key: job.key.clone() });
    let report = Report::start_after(endpoint, Mask::new(&env), Echo::Masked, outcome.lines, outcome.bytes);
    let started = Instant::now();
    let work = async {
        let dest = Destination::read(ctx, &job.project, rec.target, rec.git.as_ref()).await?.with_env(env.clone());
        let tmp = TempDir::new("publish")?;
        let zip = tmp.path().join(OUTPUT_FILE);
        if !ctx.store.download(&build_key(&job.project, &job.id, OUTPUT_FILE), &zip).await? {
            bail!("the build's output is gone");
        }
        let out = tmp.path().join("out");
        let (from, to) = (zip.clone(), out.clone());
        tokio::task::spawn_blocking(move || unzip(&from, &to)).await.context("unzipping the output")??;
        let mut build = Build::read(&out)?;
        if build.id != job.id || build.project != job.project || build.target != rec.target {
            bail!("the output isn't deployment {}'s build", job.id);
        }
        // The output is the build's own code's to write (statics.rs treats it as hostile): which
        // commit and branch it is, and who deployed it, are the record's, set when the build was
        // asked for (a push's by GitHub's word). A build that named another branch would take that
        // branch's host and custom domains, and its commit statuses.
        build.git = rec.git.clone();
        build.deployer = rec.deployer.clone();
        // As on Vercel, a push's production build that ends after a newer one was started keeps
        // its own address and leaves production to the newer one.
        let newer = match (rec.via, rec.target, &dest.current) {
            (Some(via), Target::Production, Some(project)) if via.from_repository() => superseded_by(project, &job.id),
            _ => None,
        };
        if let Some(newer) = &newer {
            log(&format!("deployment {newer} was started after this one: production is left to it, and this one keeps its own address"));
        }
        let opts = publish::Options {
            memory: order.publish.memory,
            concurrent: order.publish.concurrent,
            skip_domain: order.publish.skip_domain || newer.is_some(),
            same_source: order.publish.same_source,
            redeploy_of: order.publish.redeploy_of.clone(),
        };
        publish::publish(ctx, &dest, Files::Folder(&out), &build, &opts, started).await
    };
    let result = report::scope(report.clone(), work).await;
    let failure = match &result {
        Ok(_) => None,
        Err(e) => {
            let message = report.mask(&format!("{e:#}"));
            report.info(&format!("Error: {message}"));
            Some(Failure { step: report.step_now().or_else(|| Some(buildlog::PUBLISHING.to_owned())), message })
        }
    };
    let closed = report.close().await;
    let kept = match outcome.log_kept {
        true => keep_whole_log(ctx, job, &closed.lines).await,
        // A copy without the builder's lines would hide them: ClickHouse has them all.
        false => None,
    };
    let log = LogEnd { lines: u64::from(outcome.lines) + closed.lines.len() as u64, bytes: closed.bytes, cut: outcome.cut || closed.cut, kept };
    let status = if failure.is_none() { Status::Ready } else { Status::Error };
    track::ended(ctx, &job.project, &job.id, status, failure, log).await?;
    result.map(|_| ())
}

/// The production deployment started after `id` that is building or ready, if there is one.
fn superseded_by(project: &state::Project, id: &str) -> Option<String> {
    if !project.deployments.iter().any(|d| d.id == id) {
        return None;
    }
    let now = state::now_secs();
    project
        .deployments
        .iter()
        .take_while(|d| d.id != id)
        .find(|d| d.target == Target::Production && matches!(d.status.at(&d.created, now), Status::Queued | Status::Building | Status::Ready))
        .map(|d| d.id.clone())
}

/// The builder's copy of the log with `more` after it, written back; its key, if that worked.
async fn keep_whole_log(ctx: &Ctx, job: &Job, more: &[Line]) -> Option<String> {
    let key = track::log_key(&job.project, &job.id);
    if more.is_empty() {
        return Some(key);
    }
    let joined = async {
        let bytes = ctx.store.get_bytes(&key).await?.context("the builder's copy of the log is gone")?;
        let mut lines = report::gunzip_lines(&bytes)?;
        lines.extend_from_slice(more);
        ctx.store.put_bytes(&key, report::gzip_lines(&lines)?, "application/gzip").await
    };
    match joined.await {
        Ok(()) => Some(key),
        Err(e) => {
            eprintln!("warning: deployment {}'s log couldn't be kept whole in S3: {e:#}", job.id);
            None
        }
    }
}

/// A build's order and output are gone once it ends, whatever happened. The source stays, for a
/// rebuild.
async fn cleanup(ctx: &Ctx, project: &str, id: &str) {
    for name in [ORDER_FILE, OUTPUT_FILE] {
        let key = build_key(project, id, name);
        if let Err(e) = ctx.store.delete(&key).await {
            eprintln!("warning: deleting {key}: {e:#}");
        }
    }
}

/// Unzip `zip` into the folder `out`. Names that would land outside it are refused.
pub fn unzip(zip: &Path, out: &Path) -> Result<()> {
    let file = std::fs::File::open(zip).with_context(|| format!("opening {}", zip.display()))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).with_context(|| format!("{} isn't a zip", zip.display()))?;
    std::fs::create_dir_all(out)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else { bail!("{}: {:?} would land outside the folder", zip.display(), entry.name()) };
        let path = out.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        std::io::copy(&mut entry, &mut file).with_context(|| format!("unzipping {}", path.display()))?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o755))?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Row {
    seq: u32,
    stream: String,
    text: String,
}

/// Show a remote build's log here as it's written (from ClickHouse, as the dashboard reads it),
/// until the deployment ends; how it ended. Ctrl-C stops showing it, not the build.
pub async fn follow(ctx: &Ctx, started: &Started) -> Result<DeploymentRecord> {
    let id = &started.id;
    let analytics = match Analytics::connect(ctx).await {
        Ok(a) => Some(a),
        Err(e) => {
            eprintln!("note: the log can't be shown here as it's written ({e:#}); the dashboard shows it");
            None
        }
    };
    let mut live = analytics.is_some();
    let mut after = 0u32;
    let mut ended: Option<Instant> = None;
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        if live && let Some(a) = &analytics {
            let sql = "SELECT seq, stream, text FROM build_logs WHERE deployment = {id:String} AND seq > {after:UInt32} \
                       ORDER BY seq LIMIT 1 BY seq LIMIT 5000";
            match a.query::<Row>(sql, &[("id", id.clone()), ("after", after.to_string())]).await {
                Ok(rows) => {
                    // In order only: a line missing here comes from the kept copy at the end.
                    for row in rows {
                        if row.seq != after + 1 {
                            break;
                        }
                        print_line(Stream::parse(&row.stream), &row.text);
                        after = row.seq;
                    }
                }
                Err(e) => {
                    eprintln!("note: the log can't be shown here as it's written ({e:#}); the dashboard shows it");
                    live = false;
                }
            }
        }
        let rec = deploy::record(ctx, id).await?;
        let status = rec.status_now();
        if !status.in_progress() {
            // Its last lines and its copy in S3 come a moment after the record says it ended.
            let since = *ended.get_or_insert_with(Instant::now);
            let settled = rec.log.as_ref().is_some_and(|l| l.kept.is_some() || l.lines > 0) || status == Status::Canceled;
            if (settled && since.elapsed() > Duration::from_secs(2)) || since.elapsed() > Duration::from_secs(20) {
                let rec = deploy::record(ctx, id).await?;
                if let Some(key) = rec.log.as_ref().and_then(|l| l.kept.clone())
                    && let Ok(Some(bytes)) = ctx.store.get_bytes(&key).await
                    && let Ok(lines) = report::gunzip_lines(&bytes)
                {
                    for line in lines.iter().filter(|l| l.seq > after) {
                        print_line(Stream::parse(&line.stream), &crate::buildlog::printable(&line.text));
                    }
                }
                return Ok(rec);
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(FOLLOW_EVERY) => {}
            _ = &mut ctrl_c => {
                eprintln!("\nstopped showing the log; deployment {id} goes on building (`forkrail cancel {id}` stops it)");
                return deploy::record(ctx, id).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn why_a_builder_stopped() {
        assert!(stopped("EventAgeExceeded", None, false).contains("no builder was free"));
        let oom = json!({"errorType": "Runtime.OutOfMemory", "errorMessage": "RequestId: x Error: Runtime exited with error: signal: killed"});
        assert!(out_of_memory(Some(&oom)));
        assert!(stopped("RetriesExhausted", Some(&oom), false).contains("ran out of the builder's memory"));
        let large = stopped("RetriesExhausted", Some(&oom), true);
        assert!(large.starts_with("the build ran out of the builder's memory, and on the large builder too: build it with") && large.contains("LargeMemorySize"), "{large}");
        let timeout = json!({"errorType": "Sandbox.Timedout", "errorMessage": "RequestId: x Error: Task timed out after 900.00 seconds"});
        assert!(!out_of_memory(Some(&timeout)));
        assert!(stopped("RetriesExhausted", Some(&timeout), false).contains("15 minutes"));
        assert_eq!(stopped("RetriesExhausted", None, false), "the builder stopped without answering (RetriesExhausted)");
    }

    #[test]
    fn a_notice_names_the_builder_it_came_from() {
        assert_eq!(function_of("arn:aws:lambda:us-east-1:123456789012:function:forkrail-builder"), Some(BUILDER_FUNCTION));
        assert_eq!(function_of("arn:aws:lambda:us-east-1:123456789012:function:forkrail-builder-large:$LATEST"), Some(BUILDER_LARGE_FUNCTION));
        assert_eq!(function_of("not an arn"), None);
    }

    #[test]
    fn a_project_out_of_memory_once_builds_on_the_large_builder() {
        assert_eq!(builder_for(None), BUILDER_FUNCTION, "a new project");
        let mut project = state::Project::default();
        assert_eq!(builder_for(Some(&project)), BUILDER_FUNCTION);
        project.large_builds = Some(LargeBuilds { since: "2026-11-18T12:00:00Z".into(), deployment: "aaaa1111".into() });
        assert_eq!(builder_for(Some(&project)), BUILDER_LARGE_FUNCTION);
        // Kept through a round trip, and absent from a project that never needed it.
        let back: state::Project = serde_json::from_value(serde_json::to_value(&project).unwrap()).unwrap();
        assert_eq!(back.large_builds, project.large_builds);
        assert!(serde_json::to_value(state::Project::default()).unwrap().get("large_builds").is_none());
    }

    #[test]
    fn memory_advice_says_where_to_go() {
        assert!(memory_advice(false, None).contains("MemorySize") && !memory_advice(false, None).contains("too"));
        assert!(memory_advice(true, None).starts_with("it ran out on the large builder too"));
        assert!(memory_advice(false, Some("building it again on the large builder failed: no such function")).ends_with("(building it again on the large builder failed: no such function)"));
        assert!(memory_advice(false, Some(&account_limit_note(512))).ends_with("(this AWS account lets each function use only 512 MB for now, the builder too; AWS raises the limit as the account gets used, or sooner through a support case)"));
    }

    #[test]
    fn an_older_production_build_leaves_production_to_a_newer_one() {
        let summary = |id: &str, target, status| state::Summary { id: id.into(), target, created: state::now_rfc3339(), version: String::new(), status };
        // Newest first, as a project lists them.
        let mut project = state::Project {
            deployments: vec![
                summary("cccc3333", Target::Preview, Status::Ready),
                summary("bbbb2222", Target::Production, Status::Building),
                summary("aaaa1111", Target::Production, Status::Building),
                summary("00000000", Target::Production, Status::Ready),
            ],
            ..Default::default()
        };
        assert_eq!(superseded_by(&project, "aaaa1111").as_deref(), Some("bbbb2222"));
        assert_eq!(superseded_by(&project, "bbbb2222"), None, "a newer preview doesn't count");
        // The newer one failed: the older takes production after all.
        project.deployments[1].status = Status::Error;
        assert_eq!(superseded_by(&project, "aaaa1111"), None);
        assert_eq!(superseded_by(&project, "unknown0"), None);
        // The older one published first: it keeps its place in the list, so the newer one, ending
        // after it, isn't taken for the older and still gets production.
        project.deployments[1].status = Status::Building;
        crate::publish::list_published(&mut project, state::Summary { version: "4".into(), ..summary("aaaa1111", Target::Production, Status::Ready) });
        assert_eq!(superseded_by(&project, "bbbb2222"), None);
        assert_eq!(superseded_by(&project, "aaaa1111").as_deref(), Some("bbbb2222"));
    }

    #[test]
    fn outcomes_round_trip() {
        let outcome = Outcome {
            ending: Ending::Failed,
            error: Some(Failure { step: Some("Building".into()), message: "`npm run build` failed (exit status: 1)".into() }),
            lines: 42,
            bytes: 3_000,
            cut: false,
            log_kept: true,
            timings: Timings { queued_ms: 30, fetch_ms: 120, build_ms: 30_000, restore_ms: 900, ..Default::default() },
            out_of_memory: true,
        };
        let back: Outcome = serde_json::from_value(serde_json::to_value(&outcome).unwrap()).unwrap();
        assert_eq!((back.ending, back.lines, back.error.unwrap().step.as_deref(), back.out_of_memory), (Ending::Failed, 42, Some("Building"), true));
        assert_eq!(back.timings.restore_ms, 900);
        assert_eq!(serde_json::to_value(Ending::Built).unwrap(), json!("built"));
        assert_eq!(serde_json::from_value::<Ending>(json!("skipped")).unwrap(), Ending::Skipped);
        // What a builder and a CLI from before the build cache send is still read.
        let older = json!({"ending": "built", "lines": 1, "bytes": 2, "timings": {"queued_ms": 1, "fetch_ms": 2, "build_ms": 3, "upload_ms": 4}});
        assert_eq!(serde_json::from_value::<Outcome>(older).unwrap().timings.keep_ms, 0);
        let job = json!({"id": "a1b2c3d4", "project": "shop", "key": "k", "order_url": "o", "source_url": "s", "record_url": "r", "output_url": "u", "log_url": "l", "sent_ms": 1});
        let older = serde_json::from_value::<Job>(job).unwrap();
        assert!(older.cache.is_none() && older.next_cache.is_none() && older.again.is_none());
        // A builder from before the large builder says nothing of memory: not out of it.
        assert!(!serde_json::from_value::<Outcome>(json!({"ending": "failed", "lines": 1, "bytes": 2})).unwrap().out_of_memory);
        let order = json!({"request": {"id": "a1b2c3d4", "project": "shop", "target": "preview", "base_domain": "example.dev"}, "publish": {}});
        assert!(!serde_json::from_value::<Order>(order).unwrap().no_cache);
    }

    #[test]
    fn unzip_refuses_names_outside_the_folder() {
        let dir = TempDir::new("unzip-test").unwrap();
        let zip_path = dir.path().join("evil.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        zip.start_file("../outside.txt", zip::write::SimpleFileOptions::default()).unwrap();
        std::io::Write::write_all(&mut zip, b"x").unwrap();
        zip.finish().unwrap();
        assert!(unzip(&zip_path, &dir.path().join("out")).unwrap_err().to_string().contains("outside"));
        assert!(!dir.path().join("outside.txt").exists());
    }
}
