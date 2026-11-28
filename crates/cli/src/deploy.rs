//! Deploy, promote, roll back and remove deployments.
//!
//! A deployment is a Lambda version plus an entry in `routes.json`. Deploying builds the project
//! into a folder (build.rs) and publishes it (publish.rs): a version, the deployment's record, and
//! `<project>-<id>.<base>` pointed at it (and with `--prod`, `<project>.<base>` and the project's
//! custom domains too). Promote and rollback only repoint `<project>.<base>` and the custom
//! domains: one write that the router picks up within its poll interval, with no Lambda work.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use forkrail_common::envfile;
use forkrail_common::install::{ENV_KEY_ALIAS, Install};
use forkrail_common::routes::{Deployment, Routes, valid_id};

use crate::build;
use crate::buildlog::{self, Stream};
use crate::git;
use crate::lambda::{self, Lambda};
use crate::package;
use crate::publish::{self, Destination, Files, Published};
use crate::remote;
use crate::report::{self, Echo, Report};
use crate::retention;
use crate::secrets::{self, Secrets};
use crate::state::{self, DeploymentRecord, Project, ROUTES_KEY, Status, Store, Summary, Target, Via, now_rfc3339};
use crate::warm;
use crate::statics::Prepared;
use crate::track::{self, Begin, Canceled};
use crate::vercel::{self, Language};

/// The fixed counts kept before retention policies (`retained`; retention.rs keeps at least these):
/// the latest production ones (rollback targets) and previews.
pub const KEEP_PRODUCTION: usize = 5;
const KEEP_PREVIEWS: usize = 20;
/// Deployments that failed or were canceled, kept to read their logs.
const KEEP_FAILED: usize = 10;
/// Lambda's limit on a function's environment, keys and values together, as it counts them (the
/// JSON of the variables).
const MAX_ENV_BYTES: usize = 4096;
const MAX_PROJECT_LEN: usize = 40;
/// Labels under the base domain kept for the project's own site, so no project can take them.
pub const RESERVED_NAMES: [&str; 8] = ["www", "api", "app", "docs", "dashboard", "admin", "status", "mail"];
/// Deployment ids are this many hex digits.
const ID_HEX: usize = 8;

pub struct Ctx {
    /// The account, home region and bucket.
    pub install: Install,
    pub store: Store,
    /// Lambda in the home region.
    pub lambda: Lambda,
    /// Settings for clients in other regions.
    pub config: aws_config::SdkConfig,
}

impl Ctx {
    /// The install the credentials in `config` reach: their account's, with its state in
    /// `home_region` and the default bucket unless `bucket` names another.
    pub async fn connect(config: aws_config::SdkConfig, home_region: &str, bucket: Option<&str>) -> Result<Self> {
        let identity = aws_sdk_sts::Client::new(&config)
            .get_caller_identity()
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("{}", aws_sdk_sts::error::DisplayErrorContext(&e)))?;
        let account = identity.account().context("no account id")?;
        let partition = identity.arn().and_then(forkrail_common::install::partition_of).unwrap_or("aws");
        let install = Install::new(account, partition, home_region, bucket);
        Ok(Ctx {
            store: Store::new(aws_sdk_s3::Client::new(&config), install.bucket.clone(), Some(aws_sdk_dynamodb::Client::new(&config))),
            lambda: Lambda { client: aws_sdk_lambda::Client::new(&config) },
            install,
            config,
        })
    }

    /// Lambda in `region`.
    pub fn lambda_in(&self, region: &str) -> Lambda {
        if region == self.install.home_region {
            return self.lambda.clone();
        }
        let config = aws_sdk_lambda::config::Builder::from(&self.config).region(aws_config::Region::new(region.to_owned())).build();
        Lambda { client: aws_sdk_lambda::Client::from_conf(config) }
    }

    /// S3 in `region`.
    pub fn s3_in(&self, region: &str) -> aws_sdk_s3::Client {
        let config = aws_sdk_s3::config::Builder::from(&self.config).region(aws_config::Region::new(region.to_owned())).build();
        aws_sdk_s3::Client::from_conf(config)
    }

    /// The ARN of the install's env var key in `region` (deploy/install/secrets.yaml): functions
    /// there have their environments encrypted with it.
    pub async fn env_key(&self, region: &str) -> Result<String> {
        let config = aws_sdk_kms::config::Builder::from(&self.config).region(aws_config::Region::new(region.to_owned())).build();
        let described = aws_sdk_kms::Client::from_conf(config).describe_key().key_id(ENV_KEY_ALIAS).send().await;
        match described {
            Ok(out) => out.key_metadata.and_then(|k| k.arn).with_context(|| format!("{ENV_KEY_ALIAS} in {region} has no ARN")),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found_exception()) => bail!(
                "no {ENV_KEY_ALIAS} in {region}: set the region up with `forkrail regions setup {region}`, or pick it for a project in the dashboard"
            ),
            Err(e) => bail!("reading {ENV_KEY_ALIAS} in {region}: {}", aws_sdk_kms::error::DisplayErrorContext(&e)),
        }
    }
}

pub struct DeployArgs {
    pub dir: PathBuf,
    pub prod: bool,
    pub project: Option<String>,
    pub memory: Option<u32>,
    pub concurrent: Option<bool>,
    pub bootstrap: PathBuf,
    pub python: String,
    pub no_cache: bool,
    /// Refuse unless the files and settings are production's.
    pub same_source: bool,
    /// Deploy a framework's existing build output instead of building.
    pub prebuilt: bool,
    /// A production deployment that isn't put live: only its own host points at it.
    pub skip_domain: bool,
}

pub async fn deploy(ctx: &Ctx, args: DeployArgs) -> Result<()> {
    let started = Instant::now();
    let dir = args.dir.canonicalize().with_context(|| format!("{}", args.dir.display()))?;
    let project = match args.project {
        Some(name) => name,
        None => project_from_dir(&dir)?,
    };
    check_project(&project)?;
    let target = if args.prod { Target::Production } else { Target::Preview };
    let opts = publish::Options { memory: args.memory, concurrent: args.concurrent, skip_domain: args.skip_domain, same_source: args.same_source, redeploy_of: None };
    // The commit first: a preview of a branch gets the branch's own preview values.
    let git = git::commit(&dir);
    let dest = Destination::read(ctx, &project, target, git.as_ref()).await?;
    // Read here for the summary below, with the project's preset as the build reads it; the build
    // reads it again and reports its warnings.
    let spec = vercel::read_as(&dir, dest.current.as_ref().and_then(|p| p.framework.as_deref()))?;
    let (memory_mb, max_duration_s, concurrent) = publish::settings(&opts, (spec.memory_mb, spec.max_duration_s, spec.concurrent), spec.floor, dest.current.as_ref());
    let mut summary = if spec.static_only() {
        format!("{project}: {}, {}", spec.describe(), target.as_str())
    } else {
        format!(
            "{project}: {}, {memory_mb} MB, maxDuration {max_duration_s} s, {}, {}",
            spec.describe(),
            if concurrent { "concurrent compute" } else { "one request per worker" },
            target.as_str()
        )
    };
    // A branch's own preview values, by name: which of the deployment's values are its.
    if let (Some(p), Some(branch)) = (&dest.current, &dest.branch) {
        let own: Vec<&str> = p.env_names.branch(branch).iter().map(String::as_str).collect();
        if !own.is_empty() {
            summary.push_str(&format!(", with {branch}'s own {}", own.join(", ")));
        }
    }

    // The function's env vars must fit Lambda's limit: find out before building.
    if !spec.static_only() {
        function_env(&spec.entry, spec.language, &dest.env(), &BTreeMap::new(), &dest.egress())?;
    }
    // The id comes first, so the build knows its deployment's URL.
    let id = forkrail_common::random_hex(4);
    let mut request = dest.request(&id);
    request.git = git;
    request.deployer = git::deployer(&dir);
    let build_opts = build::Options {
        package: package::Options { bootstrap: args.bootstrap, python: args.python, no_cache: args.no_cache },
        prebuilt: args.prebuilt,
        install: crate::framework::Install::IfMissing,
    };
    let how = Begin {
        via: Via::Cli,
        deployer: request.deployer.clone(),
        git: request.git.clone(),
        redeploy_of: None,
        env: dest.env(),
        echo: Echo::Raw,
        builder: None,
        upload: None,
        github_deployment: None,
        deploy_hook: None,
    };
    let dest = &dest;
    track::run(ctx, dest, &id, how, |report| async move {
        log(&summary);
        let out = package::TempDir::new("deploy")?;
        let built = build_in_time(dir, request, build_opts, out.path().to_path_buf(), report).await?;
        publish_steadily(publish::publish(ctx, dest, Files::Folder(out.path()), &built, &opts, started)).await
    })
    .await
    .map(|_| ())
}

/// Run the build on a blocking thread, until it ends, its user stops it (Ctrl-C: `Canceled`), or
/// it passes Vercel's 45 minutes. Either of the last two stops the project's commands.
async fn build_in_time(dir: PathBuf, request: build::Request, opts: build::Options, out: PathBuf, report: Arc<Report>) -> Result<build::Build> {
    let task = tokio::task::spawn_blocking(move || build::build(&dir, &request, &opts, &out, &*report));
    tokio::select! {
        built = task => built.context("the build stopped")?,
        _ = tokio::signal::ctrl_c() => {
            buildlog::stop_commands();
            Err(Canceled.into())
        }
        _ = tokio::time::sleep(Duration::from_secs(state::BUILD_TIMEOUT_S)) => {
            buildlog::stop_commands();
            bail!("the build didn't finish within {} minutes, Vercel's limit", state::BUILD_TIMEOUT_S / 60)
        }
    }
}

/// Publishing can't stop halfway: a first Ctrl-C says so, a second leaves anyway (the deployment
/// then reads as failed once its time is up).
async fn publish_steadily<T>(publish: impl Future<Output = Result<T>>) -> Result<T> {
    let watch = tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("publishing can't stop halfway; it ends in a few seconds (Ctrl-C again to leave anyway)");
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        }
    });
    let result = publish.await;
    watch.abort();
    result
}

pub struct Redeploy {
    pub id: String,
    /// Production or preview; the deployment's own if not given.
    pub target: Option<Target>,
    /// For production: publish it without putting it live, until `promote`.
    pub skip_domain: bool,
    /// Who asked, recorded as the new deployment's deployer.
    pub actor: Option<String>,
    /// From a terminal or the dashboard.
    pub via: Via,
    /// For one that's built again: without the project's build cache (on Vercel, Redeploy with
    /// "Use existing Build Cache" unticked).
    pub no_cache: bool,
}

/// What a redeploy made.
#[derive(Debug, Clone)]
pub enum Redeployed {
    /// The deployment's build, published again at once.
    Published(Published),
    /// The deployment's source, building again in the builder.
    Building(remote::Started),
}

/// Vercel's Redeploy: deployment `id` made again as a new deployment, with the project's env vars
/// and settings as they are now. One built in the builder is built again from its source, as
/// Vercel does; one built where it was deployed from has no source here, so its build is published
/// again (`redeploy`). Either way its function settings are what a new deploy of the same files
/// gets (`publish::settings`): its own `vercel.json`, then the project's settings, then the
/// defaults. A setting the project went back to the default for is the default again, as the
/// dashboard says, not what the deployment had.
pub async fn redeploy_any(ctx: &Ctx, args: Redeploy) -> Result<Redeployed> {
    let rec = record(ctx, &args.id).await?;
    rec.check_ready()?;
    let Some(upload) = rec.upload.clone() else {
        return Ok(Redeployed::Published(redeploy(ctx, args).await?));
    };
    // No flags: the build reads the source's vercel.json again, and publishing reads the
    // project's settings as they are when it publishes.
    let publish = remote::PublishOptions {
        memory: None,
        concurrent: None,
        skip_domain: args.skip_domain,
        same_source: false,
        redeploy_of: Some(rec.id.clone()),
    };
    let started = remote::start(
        ctx,
        remote::Start {
            project: rec.project.clone(),
            target: args.target.unwrap_or(rec.target),
            upload,
            git: rec.git.clone(),
            deployer: args.actor.or_else(|| rec.deployer.clone()),
            publish,
            via: args.via,
            no_cache: args.no_cache,
            github_deployment: None,
            previous_sha: None,
        },
    )
    .await?;
    Ok(Redeployed::Building(started))
}

/// Publish deployment `id`'s build again as a new deployment, with the project's env vars and
/// settings as they are now: Vercel's Redeploy, without building again. The build's own
/// `vercel.json` still wins over the project's settings, and a setting the project doesn't have
/// is the default (`publish::settings`), as for a new deploy.
pub async fn redeploy(ctx: &Ctx, args: Redeploy) -> Result<Published> {
    let started = Instant::now();
    let rec = record(ctx, &args.id).await?;
    rec.check_ready()?;
    let target = args.target.unwrap_or(rec.target);
    // Its build holds the preview's env vars (`built_with_env`), and there's no source here to
    // build it again with production's.
    if rec.target == Target::Preview && target == Target::Production && rec.built_with_env() {
        bail!(
            "{} is a preview built where it was deployed from, so forkrail has no source to build it again, and its build holds the preview env vars (Next.js puts them in its pages and client code): deploy its source with --prod instead, or `forkrail promote {} --allow-preview-env` to serve production with the preview's",
            rec.id,
            rec.id
        );
    }
    let dest = Destination::read(ctx, &rec.project, target, rec.git.as_ref()).await?;
    // A static site has no function, and so no runtime or package.
    let language = if rec.has_function() {
        Language::from_record(&rec.runtime, rec.framework.as_deref()).with_context(|| format!("deployment {}: unknown runtime {:?}", rec.id, rec.runtime))?
    } else {
        Language::Node
    };
    // Publishing finds the package by its project and hash.
    let package_key = format!("packages/{}/{}.zip", rec.project, rec.package_sha256);
    if rec.has_function() && rec.package_key != package_key {
        bail!("deployment {}'s package is at {}, not {package_key}; deploy the project again instead", rec.id, rec.package_key);
    }
    let pinned = rec.pinned_settings();
    let build = build::Build {
        format: build::FORMAT,
        id: forkrail_common::random_hex(4),
        project: rec.project.clone(),
        target,
        built: now_rfc3339(),
        language,
        // The runtime it ran on, whatever the project asks for now: a redeploy builds nothing.
        runtime: Some(rec.runtime.clone()).filter(|r| rec.has_function() && !r.is_empty()),
        entry: rec.entry.clone(),
        memory_mb: pinned.memory_mb,
        max_duration_s: pinned.max_duration_s,
        concurrent: pinned.concurrent,
        floor: pinned.floor,
        regions: pinned.regions.clone(),
        config: rec.config.clone(),
        importer: Some(rec.importer.clone().unwrap_or_else(|| build::IMPORTER.to_owned())),
        framework: rec.framework.clone(),
        deployer: args.actor.or_else(|| rec.deployer.clone()),
        git: rec.git.clone(),
        source_sha256: rec.source_sha256.clone(),
        source_files: 0,
        package: rec.has_function().then(|| package::Package {
            sha256: rec.package_sha256.clone(),
            zipped: rec.package_bytes,
            unzipped: 0,
            entries: 0,
            deps_cached: None,
            reads_sealed_env: rec.reads_sealed_env,
            app_aws_keys: rec.app_aws_keys,
        }),
        static_files: rec.static_files.clone().map(|id| Prepared { id, files: rec.static_count.unwrap_or(0), bytes: rec.static_bytes.unwrap_or(0) }),
        rules: rec.rules.clone(),
        images: rec.images.clone(),
        kept_source: rec.kept_source.clone(),
        schedules: rec.schedules.clone(),
        functions: rec.functions.clone(),
        // Built as it was: its app is told what its build was.
        skew_protection: rec.skew_protection,
        skew_id: rec.skew_id.clone(),
        outline: rec.outline.clone(),
        // A Next.js build's shared cache and seed: the seed is the deployment's, so publishing
        // carries it over to the new one (`publish::seed_next`).
        next: rec.next.clone(),
        // The router's Next rules, as the record has them (their config is in the store still).
        next_router: rec.next_router.clone(),
    };
    let opts = publish::Options { skip_domain: args.skip_domain, redeploy_of: Some(rec.id.clone()), ..Default::default() };
    let how = Begin {
        via: args.via,
        deployer: build.deployer.clone(),
        git: build.git.clone(),
        redeploy_of: Some(rec.id.clone()),
        env: dest.env(),
        // Raw only on the asker's own machine: in the control function (a terminal's request comes
        // as `cli` too) its output is the CloudWatch log, which never shows values.
        echo: match args.via {
            Via::Cli if std::env::var_os("AWS_LAMBDA_FUNCTION_NAME").is_none() => Echo::Raw,
            _ => Echo::Masked,
        },
        builder: None,
        upload: None,
        github_deployment: None,
        deploy_hook: None,
    };
    let id = build.id.clone();
    let dest = &dest;
    track::run(ctx, dest, &id, how, |_| async move {
        log(&format!("{}: deployment {}'s build again, as {} ({})", rec.project, rec.id, build.id, target.as_str()));
        publish_steadily(publish::publish(ctx, dest, Files::Uploaded, &build, &opts, started)).await
    })
    .await
}

/// A warning if `what` would run differently from production: above all a concurrent deployment
/// replacing production that runs one request per worker, which would put requests in flight at
/// once behind one outbound IP.
pub(crate) async fn mode_change(ctx: &Ctx, project: Option<&Project>, concurrent: bool, what: &str) -> Result<Option<String>> {
    let Some(id) = project.and_then(|p| p.production.as_deref()) else { return Ok(None) };
    let Some(production) = ctx.store.get_value::<DeploymentRecord>(&state::deployment_key(id)).await? else {
        return Ok(None);
    };
    Ok(match (production.concurrent, concurrent) {
        (false, true) => Some(format!(
            "production ({id}) runs one request per worker, and {what} packs requests onto shared workers \
             (concurrent compute), so requests in flight at once would share outbound IPs"
        )),
        (true, false) => Some(format!("production ({id}) runs concurrent compute, and {what} runs one request per worker")),
        _ => None,
    })
}

/// Point the project's production domains at deployment `id`: not those that redirect or follow
/// a branch.
pub fn point_domains(routes: &mut Routes, project: &str, id: &str) {
    for domain in project_domains(routes, project) {
        routes.hosts.insert(domain, id.to_owned());
    }
}

/// The project's production domains in the table: those that serve its production deployment.
pub fn project_domains(routes: &Routes, project: &str) -> Vec<String> {
    routes.domains.iter().filter(|(_, d)| d.project == project && d.production()).map(|(name, _)| name.clone()).collect()
}

/// Point `<project>.<base>` at deployment `id`. A preview keeps the env vars it was built with, so
/// one whose preview env differs from production's is refused unless `allow_preview_env`.
/// Copy `target`, `runtime`, `entry` and the creation time from each deployment's record into its
/// routing entry, for entries from before the routing table had them. The router logs the first
/// three with each request; skew protection's maximum age counts from the last (routers keep their
/// pools when only it changes).
pub async fn fill_route_details(ctx: &Ctx) -> Result<()> {
    let routes: Routes = ctx.store.get_value(ROUTES_KEY).await?.unwrap_or_default();
    let mut details = BTreeMap::new();
    for (id, d) in &routes.deployments {
        if d.target.is_some() && d.runtime.is_some() && d.entry.is_some() && d.created.is_some() {
            continue;
        }
        if let Some(r) = ctx.store.get_value::<DeploymentRecord>(&state::deployment_key(id)).await? {
            let created = state::parse_rfc3339(&r.created);
            details.insert(id.clone(), (r.target.as_str().to_owned(), r.runtime, r.entry, created, r.skew_id));
        }
    }
    if details.is_empty() {
        println!("Every routing entry has its details.");
        return Ok(());
    }
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            for (id, (target, runtime, entry, created, skew_id)) in &details {
                if let Some(d) = routes.deployments.get_mut(id) {
                    d.target.get_or_insert_with(|| target.clone());
                    d.runtime.get_or_insert_with(|| runtime.clone());
                    d.entry.get_or_insert_with(|| entry.clone());
                    if d.created.is_none() {
                        d.created = *created;
                    }
                    if d.skew_id.is_none() {
                        d.skew_id = skew_id.clone();
                    }
                }
            }
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    println!("Filled in {} of {} deployments.", details.len(), routes.deployments.len());
    Ok(())
}

/// What promoting a preview does when its preview env vars aren't production's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewEnv {
    /// Refuse, naming the variables (`PreviewEnvDiffers`).
    Refuse,
    /// Serve production with the preview's env vars anyway.
    Keep,
    /// Publish the preview's build again as a new production deployment, with production's env
    /// vars, as Vercel does.
    Republish,
}

/// A preview whose env vars differ from production's, by name.
#[derive(Debug)]
pub struct PreviewEnvDiffers {
    pub id: String,
    pub keys: Vec<String>,
    /// Its build holds the values (`DeploymentRecord::built_with_env`): only building it again
    /// gives it production's.
    pub rebuild: bool,
    /// Its source is in the install (built in the builder), so it can be built again.
    pub source: bool,
}

impl std::fmt::Display for PreviewEnvDiffers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let how = match (self.rebuild, self.source) {
            (true, true) => "Its build has them in its pages and client code (Next.js), so it's built again with production's: `forkrail redeploy {id} --prod` does that from its source",
            (true, false) => "Its build has them in its pages and client code (Next.js), and it was built where it was deployed from, so the install has no source to build it again with production's: deploy its source with --prod",
            (false, _) => "`forkrail redeploy {id} --prod` publishes its build with production's",
        };
        write!(
            f,
            "{} is a preview built with the preview env vars, which differ from production's in {}. {}, or pass --allow-preview-env to serve production with them",
            self.id,
            self.keys.join(", "),
            how.replace("{id}", &self.id)
        )
    }
}

impl std::error::Error for PreviewEnvDiffers {}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Promoted {
    /// Production now points at the deployment itself.
    Instant { project: String, id: String },
    /// The preview's build was published again for production, as a new deployment.
    Republished(Published),
    /// The preview's source is building again for production, as a new deployment (a build that
    /// holds its env vars' values, `DeploymentRecord::built_with_env`).
    Rebuilding(remote::Started),
}

/// Make deployment `id` production. A preview keeps the env vars it was built with, so one whose
/// preview env differs from production's is handled as `preview_env` says; `actor` is recorded if
/// that publishes a new deployment.
pub async fn promote(ctx: &Ctx, id: &str, preview_env: PreviewEnv, actor: Option<&str>, via: Via) -> Result<Promoted> {
    let record = record(ctx, id).await?;
    record.check_ready()?;
    let current = project_record(ctx, &record.project).await?;
    if current.production.as_deref() != Some(id)
        && let Some(warning) = mode_change(ctx, Some(&current), record.concurrent, id).await?
    {
        eprintln!("warning: {warning}");
    }
    if record.target == Target::Preview {
        secrets::check_moved(&current)?;
        let secrets = Secrets::new(ctx);
        // What the preview was built with: preview's values, and its branch's own over them.
        let branch = state::env_branch(record.target, record.git.as_ref());
        let (preview, production) = tokio::try_join!(secrets.deployed(&record.project, Target::Preview, branch), secrets.values(&record.project, Target::Production))?;
        let mut differ = preview_env_differs(&preview, &production, &record);
        // Its app was told it's a preview (FORKRAIL_ENV, FORKRAIL_URL …): production's say otherwise.
        if record.system_env {
            differ.push("FORKRAIL_ENV".to_owned());
        }
        if !differ.is_empty() {
            let rebuild = record.built_with_env();
            match preview_env {
                PreviewEnv::Refuse => return Err(PreviewEnvDiffers { id: id.to_owned(), keys: differ, rebuild, source: record.upload.is_some() }.into()),
                PreviewEnv::Keep => {
                    eprintln!("note: {id} was built with the preview env vars ({}), and keeps them in production", differ.join(", "))
                }
                PreviewEnv::Republish if rebuild => {
                    // As Vercel promotes a preview: a complete rebuild, with production's env vars.
                    log(&format!("{id} was built with preview env vars that differ in {}, and its build holds them: building it again for production", differ.join(", ")));
                    let args = Redeploy { id: id.to_owned(), target: Some(Target::Production), skip_domain: false, actor: actor.map(str::to_owned), via, no_cache: false };
                    return match Box::pin(redeploy_any(ctx, args)).await? {
                        Redeployed::Building(started) => Ok(Promoted::Rebuilding(started)),
                        Redeployed::Published(published) => Ok(Promoted::Republished(published)),
                    };
                }
                PreviewEnv::Republish => {
                    log(&format!("{id} was built with preview env vars that differ in {}: publishing its build for production", differ.join(", ")));
                    let args = Redeploy { id: id.to_owned(), target: Some(Target::Production), skip_domain: false, actor: actor.map(str::to_owned), via, no_cache: false };
                    let published = Box::pin(redeploy(ctx, args)).await?;
                    // Asked for by name: production, even with "Auto-assign custom domains" off.
                    if !published.live {
                        let fresh = self::record(ctx, &published.id).await?;
                        warm::deployment(ctx, &fresh).await;
                        set_production(ctx, &published.project, &published.id, false).await?;
                        log(&format!("{}: production is {}", published.project, published.id));
                    }
                    return Ok(Promoted::Republished(Published { live: true, ..published }));
                }
            }
        }
    }
    // Its environments first, unless it's production already (warm.rs).
    if current.production.as_deref() != Some(id) {
        warm::deployment(ctx, &record).await;
    }
    set_production(ctx, &record.project, id, false).await?;
    if record.has_function() {
        log(&format!("{}: production is {id} (version {})", record.project, record.version));
    } else {
        log(&format!("{}: production is {id}", record.project));
    }
    Ok(Promoted::Instant { project: record.project, id: id.to_owned() })
}

/// Point production back: at `to`, a deployment of the project that has been production, or by
/// default the one before the current. Returns the deployment that is production now.
pub async fn rollback(ctx: &Ctx, project: &str, to: Option<&str>) -> Result<String> {
    let p = project_record(ctx, project).await?;
    let current = p.production.clone().context("no production deployment to roll back from")?;
    let target = match to {
        None => p.production_history.iter().find(|d| **d != current).cloned().with_context(|| format!("{project} has no earlier production deployment"))?,
        Some(id) if id == current => bail!("{id} is {project}'s production deployment already"),
        Some(id) => {
            let rec = record(ctx, id).await?;
            if rec.project != project {
                bail!("deployment {id} belongs to {}, not {project}", rec.project);
            }
            rec.check_ready()?;
            if rec.target != Target::Production && !p.production_history.iter().any(|d| d == id) {
                bail!("{id} has never been production; promote it instead");
            }
            id.to_owned()
        }
    };
    // A rollback doesn't wait: production moves at once, and the deployment it goes back to is
    // warmed meanwhile, for the requests after the first.
    let back_to = record(ctx, &target).await?;
    let moving = async {
        set_production(ctx, project, &target, true).await?;
        log(&format!("{project}: rolled back from {current} to {target}"));
        Ok::<_, anyhow::Error>(())
    };
    let (moved, _) = tokio::join!(moving, warm::deployment(ctx, &back_to));
    moved?;
    Ok(target)
}

/// Repoint the production host and the custom domains, then record it. With `rollback`, the
/// deployment rolled back from leaves the history, so a second rollback goes further back.
async fn set_production(ctx: &Ctx, project: &str, id: &str, rollback: bool) -> Result<()> {
    let settings = ctx.store.settings().await?;
    let host = format!("{project}.{}", settings.base_domain);
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            match routes.deployments.get(id) {
                Some(d) if d.project == project => {}
                Some(_) => bail!("deployment {id} belongs to another project"),
                None => bail!("deployment {id} isn't in the routing table"),
            }
            routes.hosts.insert(host.clone(), id.to_owned());
            point_domains(&mut routes, project, id);
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    ctx.store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = current.context("no such project")?;
            if rollback && p.production_history.first().is_some_and(|d| Some(d) == p.production.as_ref()) {
                p.production_history.remove(0);
            }
            p.production = Some(id.to_owned());
            p.production_history.retain(|d| d != id);
            p.production_history.insert(0, id.to_owned());
            Ok(p)
        })
        .await?;
    // Production's schedules now: a rollback brings back the old deployment's own.
    crate::schedules::after_production_change(ctx, project).await;
    match record(ctx, id).await {
        Ok(rec) => {
            let kind = if rollback { crate::events::DEPLOYMENT_ROLLBACK } else { crate::events::DEPLOYMENT_PROMOTED };
            crate::events::deployment(ctx, kind, &rec).await;
        }
        Err(e) => eprintln!("warning: no event for {id}: {e:#}"),
    }
    Ok(())
}

/// Show a project's limits, or set its max workers (`Some(None)` clears it): on the project, so
/// later deploys get it, and on its deployments in the routing table, which the router applies
/// within its poll interval. A pool whose entry changes is replaced: requests in flight finish on
/// its workers, and new ones start on a fresh pool.
pub async fn limits(ctx: &Ctx, project: &str, max_workers: Option<Option<u32>>) -> Result<()> {
    let mut p = project_record(ctx, project).await?;
    if let Some(max_workers) = max_workers {
        p = ctx
            .store
            .update::<Project, _>(&state::project_key(project), |current| {
                let mut p = current.context("no such project")?;
                p.max_workers = max_workers;
                Ok(p)
            })
            .await?;
        let mut changed = 0;
        ctx.store
            .update::<Routes, _>(ROUTES_KEY, |routes| {
                let mut routes = routes.unwrap_or_default();
                changed = 0;
                for d in routes.deployments.values_mut().filter(|d| d.project == project) {
                    if d.max_workers != max_workers {
                        d.max_workers = max_workers;
                        changed += 1;
                    }
                }
                routes.validate().map_err(anyhow::Error::msg)?;
                Ok(routes)
            })
            .await?;
        log(&format!("{project}: {changed} deployments updated; the router applies it within 5 s"));
    }
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let single = p.production.as_deref().and_then(|id| routes.deployments.get(id)).is_some_and(Deployment::single);
    match p.max_workers {
        Some(n) => println!(
            "max workers: {n} per deployment{}",
            if single { format!(", so production runs at most {n} requests at once") } else { String::new() }
        ),
        None => println!("max workers: no cap of its own (as many as Lambda's concurrency allows, unless the router sets FORKRAIL_POOL_MAX_WORKERS)"),
    }
    Ok(())
}

/// Delete one deployment, restorably (retention.rs): it stops being served at once and can be
/// restored for up to 30 days. Production can't be deleted; promote another first.
pub async fn remove(ctx: &Ctx, id: &str) -> Result<()> {
    remove_as(ctx, id, None).await
}

/// `remove`, recording who asked.
pub async fn remove_as(ctx: &Ctx, id: &str, actor: Option<&str>) -> Result<()> {
    let record = record(ctx, id).await?;
    let p = project_record(ctx, &record.project).await?;
    if p.production.as_deref() == Some(id) {
        bail!("{id} is {}'s production deployment; promote another first", record.project);
    }
    let Some(listed) = p.deployments.iter().find(|d| d.id == id) else {
        bail!("{id} isn't one of {}'s deployments (deleted already? `forkrail retention --project {}` lists the deleted)", record.project, record.project);
    };
    // One still being built would publish into a deleted deployment: it's canceled first.
    if listed.status.at(&listed.created, state::now_secs()).in_progress() {
        bail!("{id} is still being built: `forkrail cancel {id}` stops it, then it can be deleted");
    }
    retention::trash(ctx, &record.project, &[id.to_owned()], &retention::Removal::by_hand(actor)).await?;
    let until = project_record(ctx, &record.project).await?.deleted.iter().find(|t| t.id == id).map(|t| t.until.clone()).unwrap_or_default();
    log(&format!("deleted {id}; `forkrail restore {id}` brings it back until {until}"));
    Ok(())
}

/// Run the project's retention policy (retention.rs), as every publish does: what it no longer
/// keeps is deleted restorably, and deleted deployments whose time is up go for good. Returns the
/// ids deleted now.
pub async fn prune(ctx: &Ctx, project: &str) -> Result<Vec<String>> {
    let swept = retention::sweep(ctx, project, false).await?;
    Ok(swept.deleted)
}

/// What the fixed counts kept before retention policies: production, the newest production
/// deployments and rollback targets, the newest previews, every deployment still being made, and
/// the newest that failed or were canceled. The retention policy's exceptions keep all of these
/// (retention.rs tests it), so its defaults delete nothing these kept.
pub fn retained(p: &Project) -> HashSet<String> {
    let now = state::now_secs();
    let status = |d: &Summary| d.status.at(&d.created, now);
    let ready = |d: &&Summary| status(d) == Status::Ready;
    let mut keep: HashSet<String> = p.production.iter().cloned().collect();
    keep.extend(p.production_history.iter().take(KEEP_PRODUCTION).cloned());
    keep.extend(p.deployments.iter().filter(ready).filter(|d| d.target == Target::Production).take(KEEP_PRODUCTION).map(|d| d.id.clone()));
    keep.extend(p.deployments.iter().filter(ready).filter(|d| d.target == Target::Preview).take(KEEP_PREVIEWS).map(|d| d.id.clone()));
    keep.extend(p.deployments.iter().filter(|d| status(d).in_progress()).map(|d| d.id.clone()));
    keep.extend(p.deployments.iter().filter(|d| matches!(status(d), Status::Error | Status::Canceled)).take(KEEP_FAILED).map(|d| d.id.clone()));
    keep
}

/// Delete deployments of a branch that was deleted (connect.rs), restorably (retention.rs).
pub(crate) async fn remove_deployments(ctx: &Ctx, project: &str, ids: &[String]) -> Result<()> {
    let removal = retention::Removal { by: None, reason: "its branch was deleted".to_owned(), period: None };
    retention::trash(ctx, project, ids, &removal).await?;
    Ok(())
}

pub async fn record(ctx: &Ctx, id: &str) -> Result<DeploymentRecord> {
    if !valid_id(id) {
        bail!("{id:?} isn't a deployment id");
    }
    ctx.store.get_value(&state::deployment_key(id)).await?.with_context(|| format!("no deployment {id}"))
}

pub async fn project_record(ctx: &Ctx, project: &str) -> Result<Project> {
    check_project(project)?;
    ctx.store.get_value(&state::project_key(project)).await?.with_context(|| format!("no project {project}"))
}

pub fn new_project(name: &str) -> Project {
    Project { name: name.to_owned(), function: lambda::function_name(name), created: now_rfc3339(), ..Default::default() }
}

/// Env var names whose preview value isn't production's: in the project's env now, or among those
/// the deployment was built with. Values are compared, never printed.
fn preview_env_differs(preview: &BTreeMap<String, String>, production: &BTreeMap<String, String>, record: &DeploymentRecord) -> Vec<String> {
    let mut keys: BTreeSet<&String> = preview.keys().chain(production.keys()).collect();
    keys.extend(record.env_keys.iter());
    keys.into_iter()
        .filter(|k| {
            let built = record.env_keys.contains(k);
            preview.get(*k) != production.get(*k) || built != production.contains_key(*k)
        })
        .cloned()
        .collect()
}

pub(crate) fn hosts_for(project: &str, id: &str, base: &str, live: bool) -> Vec<String> {
    let mut hosts = vec![format!("{project}-{id}.{base}")];
    if live {
        hosts.push(format!("{project}.{base}"));
    }
    hosts
}

/// What joins a project's name and a branch's in a branch host, as on Vercel.
pub const BRANCH_INFIX: &str = "-git-";
/// A DNS label's longest.
const MAX_LABEL: usize = 63;

/// The host that follows a branch's newest preview, `<project>-git-<branch>.<base>`, as Vercel
/// names it: the branch in lowercase with everything but letters and digits as dashes
/// (`feature/Logs` is `feature-logs`), and cut to fit one DNS label, then with six hex digits of
/// the whole name's SHA-256 so two long names don't meet.
pub fn branch_host(project: &str, branch: &str, base: &str) -> String {
    let mut slug = String::new();
    for c in branch.chars() {
        let c = if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' };
        if !(c == '-' && (slug.is_empty() || slug.ends_with('-'))) {
            slug.push(c);
        }
    }
    let slug = slug.trim_end_matches('-');
    let room = MAX_LABEL - project.len().min(MAX_PROJECT_LEN) - BRANCH_INFIX.len();
    let hash = || forkrail_common::hex(&<sha2::Sha256 as sha2::Digest>::digest(branch.as_bytes()))[..6].to_owned();
    let slug = if slug.is_empty() {
        hash()
    } else if slug.len() > room {
        format!("{}-{}", slug[..room - 7].trim_end_matches('-'), hash())
    } else {
        slug.to_owned()
    };
    format!("{project}{BRANCH_INFIX}{slug}.{base}")
}

/// A function's environment: what goes in Lambda's (4 KB at most), and, when the project's env
/// vars don't fit there with the runtime's own, those vars, for publishing to seal into a file the
/// runtime opens (forkrail_common::envfile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FunctionEnv {
    pub lambda: BTreeMap<String, String>,
    pub sealed: Option<BTreeMap<String, String>>,
}

/// The function's environment: the app's variables (the system ones, then the project's own for
/// the target, which win), plus what the runtime needs to find the app and the egress targets it
/// listens for. The project's may take 64 KB in all, as on Vercel; past Lambda's 4 KB, or with a
/// name Lambda won't take, the app's are sealed.
pub(crate) fn function_env(entry: &str, language: Language, user: &BTreeMap<String, String>, system: &BTreeMap<String, String>, egress: &[String]) -> Result<FunctionEnv> {
    for key in user.keys() {
        check_env_key(key)?;
    }
    let size = envfile::size(user);
    if size > envfile::MAX_BYTES {
        bail!("the env vars take {size} bytes, names and values together; at most {} (64 KB)", envfile::MAX_BYTES);
    }
    let mut runtime = BTreeMap::new();
    // On a managed runtime our bootstrap is its exec wrapper; on provided.al2023 (Go, Rust) it's the
    // runtime itself, which Lambda starts as /var/task/bootstrap.
    if !language.compiled() {
        runtime.insert("AWS_LAMBDA_EXEC_WRAPPER".to_owned(), "/var/task/bootstrap".to_owned());
    }
    runtime.insert("FORKRAIL_APP_DIR".to_owned(), "/var/task/app".to_owned());
    runtime.insert("FORKRAIL_APP_ENTRY".to_owned(), entry.to_owned());
    if !egress.is_empty() {
        runtime.insert("FORKRAIL_EGRESS".to_owned(), egress.join(","));
    }
    if language == Language::Python {
        runtime.insert("PYTHONPATH".to_owned(), "/var/task/python".to_owned());
    }
    let mut app = system.clone();
    app.extend(user.clone());
    let mut all = app.clone();
    all.extend(runtime.clone());
    if app.keys().all(|k| lambda_takes(k)) && serde_json::to_string(&all)?.len() <= MAX_ENV_BYTES {
        return Ok(FunctionEnv { lambda: all, sealed: None });
    }
    // The runtime's own and the file's key (64 hex digits) must still fit.
    let mut with_key = runtime.clone();
    with_key.insert(envfile::KEY_VAR.to_owned(), "0".repeat(64));
    let size = serde_json::to_string(&with_key)?.len();
    if size > MAX_ENV_BYTES {
        bail!("the runtime's own env vars take {size} bytes; Lambda allows {MAX_ENV_BYTES} (too many egress targets?)");
    }
    Ok(FunctionEnv { lambda: runtime, sealed: Some(app) })
}

/// The env var a project's chosen bypass secret reaches its deployments as (protection.rs).
pub const BYPASS_SECRET_ENV: &str = "FORKRAIL_AUTOMATION_BYPASS_SECRET";
/// `FORKRAIL_*` names forkrail sets for a project through its own settings, never typed in: they
/// pass at publish, and `check_user_env_key` refuses them.
pub const SYSTEM_ENV: [&str; 1] = [BYPASS_SECRET_ENV];

/// A name someone types in: what `check_env_key` refuses, and the system ones.
pub fn check_user_env_key(key: &str) -> Result<()> {
    if SYSTEM_ENV.contains(&key) {
        bail!("{key} is set by deployment protection: `forkrail protection bypass add --env`, or Use as {key} in the dashboard");
    }
    check_env_key(key)
}

/// Names nobody sets, as on Vercel (its reserved list, which is Lambda's less the AWS SDK's five
/// below), plus `PYTHONPATH`, which the runtime sets for a Python app's packages.
pub const RESERVED_ENV: [&str; 13] = [
    "_HANDLER",
    "_X_AMZN_TRACE_ID",
    "AWS_ACCESS_KEY",
    "AWS_SECRET_KEY",
    "AWS_EXECUTION_ENV",
    "HOME",
    "LAMBDA_RUNTIME_DIR",
    "LAMBDA_TASK_ROOT",
    "NOW_REGION",
    "PATH",
    "PYTHONPATH",
    "TEMP",
    "TZ",
];
/// Whole families: Lambda's own (`AWS_LAMBDA_*`, the exec wrapper the runtime is started by
/// among them), and forkrail's (`FORKRAIL_*`: the runtime's and the system variables).
pub const RESERVED_ENV_PREFIXES: [&str; 3] = ["AWS_LAMBDA_", "FORKRAIL_", "_X_AMZN_"];
/// Names Lambda keeps out of a function's environment but Vercel lets a project set, for the AWS
/// SDK: they reach the app through the sealed file (forkrail_common::envfile).
pub const AWS_SDK_ENV: [&str; 5] = ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_REGION", "AWS_DEFAULT_REGION"];

/// Whether Lambda takes `key` in a function's environment: `[a-zA-Z][a-zA-Z0-9_]+`, and not one
/// of the names it keeps for itself that a project may set (`AWS_SDK_ENV`; the rest are refused
/// before this). Anything else goes through the sealed file.
pub(crate) fn lambda_takes(key: &str) -> bool {
    key.len() >= 2 && key.starts_with(|c: char| c.is_ascii_alphabetic()) && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !AWS_SDK_ENV.contains(&key)
}

/// What may be set: letters, digits and `_`, not starting with a digit, and none of the reserved
/// names; the system ones (`SYSTEM_ENV`) pass.
pub fn check_env_key(key: &str) -> Result<()> {
    if SYSTEM_ENV.contains(&key) {
        return Ok(());
    }
    let valid = key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!("{key:?}: env var names are letters, digits and _, not starting with a digit");
    }
    if RESERVED_ENV.contains(&key) || RESERVED_ENV_PREFIXES.iter().any(|p| key.starts_with(p)) {
        bail!("{key} is reserved: Lambda, the runtime or forkrail sets it (AWS_LAMBDA_*, FORKRAIL_*, PATH, HOME, TZ and the like)");
    }
    Ok(())
}

/// A project name from the directory's: lowercase, with anything else as dashes.
pub fn project_from_dir(dir: &Path) -> Result<String> {
    let base = dir.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut name = String::new();
    for c in base.chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if !(c == '-' && (name.is_empty() || name.ends_with('-'))) {
            name.push(c);
        }
    }
    let name: String = name.trim_end_matches('-').chars().take(MAX_PROJECT_LEN).collect();
    let name = name.trim_end_matches('-').to_owned();
    if name.is_empty() {
        bail!("can't name a project after {}; pass --project", dir.display());
    }
    Ok(name)
}

/// Project names become a Lambda function name and a DNS label (`<project>-<id>`).
pub fn check_project(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= MAX_PROJECT_LEN
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        bail!("{name:?}: project names are 1-{MAX_PROJECT_LEN} of a-z, 0-9 and -, not starting or ending with -");
    }
    if RESERVED_NAMES.contains(&name) {
        bail!("{name:?} is reserved for the site itself ({}); pick another project name", RESERVED_NAMES.join(", "));
    }
    // `<name>.<base>` would be another project's branch host, `<project>-git-<branch>.<base>`.
    if name.contains(BRANCH_INFIX) {
        bail!("{name:?} has {BRANCH_INFIX} in it, so its host could be another project's branch URL; pick another name");
    }
    // `<name>.<base>` would be another project's deployment host, `<project>-<id>.<base>`.
    if let Some((_, suffix)) = name.rsplit_once('-')
        && suffix.len() == ID_HEX
        && suffix.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("{name:?} ends like a deployment id (-<{ID_HEX} hex digits>), so its host could be another project's; pick another name");
    }
    Ok(())
}

/// What a terminal says once a preview is ready: how it goes to production, as `vercel` says
/// `vercel --prod`; before the project's first production deployment, also that its domain
/// answers nothing until then.
pub fn preview_ready_note(project: &str, id: &str, has_production: bool) -> String {
    let how = format!("`forkrail promote {id}` makes this preview production, or `forkrail deploy --prod` deploys production");
    if has_production { format!("{project}'s production is unchanged: {how}") } else { format!("{project} has no production deployment yet, so its domain answers nothing: {how}") }
}

/// One of forkrail's lines: to the deployment's build log while one is being made (which shows
/// it in the terminal too), else to the terminal.
pub fn log(message: &str) {
    if !report::emit(Stream::Forkrail, message) {
        eprintln!("==> {message}");
    }
}

/// A warning, the same way.
pub fn warn(message: &str) {
    let message = format!("warning: {message}");
    if !report::emit(Stream::Forkrail, &message) {
        eprintln!("{message}");
    }
}

/// The start of a step of the deployment's log.
pub fn step(name: &str) {
    if !report::emit(Stream::Step, name) {
        eprintln!("==> {}", name.to_lowercase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Summary;

    fn summary(id: &str, target: Target) -> Summary {
        Summary { id: id.into(), target, created: String::new(), version: "1".into(), status: Status::Ready }
    }

    #[test]
    fn retention_keeps_builds_running_and_the_newest_failures() {
        let mut p = new_project("p");
        let now = now_rfc3339();
        let long_ago = "2026-01-01T00:00:00Z";
        // Newest first: one building now, one left building long ago, 12 failed, 25 ready previews.
        p.deployments.push(Summary { status: Status::Building, created: now.clone(), ..summary("running", Target::Preview) });
        p.deployments.push(Summary { status: Status::Building, created: long_ago.into(), ..summary("stale", Target::Preview) });
        for i in 0..12 {
            p.deployments.push(Summary { status: if i % 2 == 0 { Status::Error } else { Status::Canceled }, version: String::new(), ..summary(&format!("bad{i}"), Target::Preview) });
        }
        for i in 0..25 {
            p.deployments.push(summary(&format!("pv{i}"), Target::Preview));
        }
        let keep = retained(&p);
        assert!(keep.contains("running"));
        // Failures don't push ready previews out.
        assert!((0..20).all(|i| keep.contains(&format!("pv{i}"))));
        assert!(!keep.contains("pv20"));
        // The stale build counts as failed: it and the nine newest failures stay.
        assert!(keep.contains("stale"));
        assert!((0..9).all(|i| keep.contains(&format!("bad{i}"))));
        assert!(!keep.contains("bad9") && !keep.contains("bad11"));
        assert_eq!(keep.len(), 1 + 20 + 10);
    }

    #[test]
    fn retention_keeps_production_rollbacks_and_newest_previews() {
        let mut p = new_project("p");
        // Newest first: 30 previews and 8 production deployments.
        for i in 0..30 {
            p.deployments.push(summary(&format!("pv{i}"), Target::Preview));
        }
        for i in 0..8 {
            p.deployments.push(summary(&format!("pd{i}"), Target::Production));
        }
        p.production_history = (0..8).map(|i| format!("pd{i}")).collect();
        // Rolled back to an old one, then promoted an old preview.
        p.production = Some("pv29".into());
        let keep = retained(&p);
        assert!(keep.contains("pv29"));
        assert!((0..20).all(|i| keep.contains(&format!("pv{i}"))));
        assert!(!keep.contains("pv20"));
        assert!((0..5).all(|i| keep.contains(&format!("pd{i}"))));
        assert!(!keep.contains("pd5"));
        assert_eq!(keep.len(), 20 + 5 + 1);
    }

    fn record(target: Target, env_keys: &[&str]) -> DeploymentRecord {
        DeploymentRecord {
            id: "a1b2c3d4".into(),
            project: "p".into(),
            target,
            created: String::new(),
            status: Status::Ready,
            error: None,
            via: None,
            building_at: None,
            ended_at: None,
            log: None,
            function: String::new(),
            version: "1".into(),
            runtime: String::new(),
            entry: String::new(),
            memory_mb: 640,
            max_duration_s: 300,
            concurrent: true,
            source_sha256: String::new(),
            package_sha256: String::new(),
            package_key: String::new(),
            package_bytes: 0,
            reads_sealed_env: false,
            app_aws_keys: false,
            system_env: false,
            env_keys: env_keys.iter().map(|k| k.to_string()).collect(),
            hosts: Vec::new(),
            config: None,
            region: None,
            replicas: BTreeMap::new(),
            framework: None,
            deployer: None,
            git: None,
            static_files: None,
            static_count: None,
            static_bytes: None,
            rules: None,
            images: None,
            role: None,
            egress: Vec::new(),
            redeploy_of: None,
            kept_source: None,
            builder: None,
            upload: None,
            github_deployment: None,
            pinned: None,
            importer: None,
            schedules: Vec::new(),
            deploy_hook: None,
            functions: Vec::new(),
            skew_protection: false,
            skew_id: None,
            outline: None,
            next: None,
            next_router: None,
        }
    }

    #[test]
    fn promote_guard_compares_preview_env_with_production() {
        let production: BTreeMap<String, String> = [("KEY".to_owned(), "secret".to_owned())].into();
        let mut preview = production.clone();
        assert!(preview_env_differs(&preview, &production, &record(Target::Preview, &["KEY"])).is_empty());
        // An app whose previews skip its key checks.
        preview.insert("SKIP_KEY_CHECK".into(), "1".into());
        assert_eq!(preview_env_differs(&preview, &production, &record(Target::Preview, &["KEY", "SKIP_KEY_CHECK"])), ["SKIP_KEY_CHECK"]);
        // A different value counts, and so does a key the deployment was built with that production lacks.
        preview.remove("SKIP_KEY_CHECK");
        preview.insert("KEY".into(), "other".into());
        assert_eq!(preview_env_differs(&preview, &production, &record(Target::Preview, &["KEY"])), ["KEY"]);
        preview.insert("KEY".into(), "secret".into());
        assert_eq!(preview_env_differs(&preview, &production, &record(Target::Preview, &["KEY", "OLD"])), ["OLD"]);
        // A Next.js build holds the values it was built with: promoting it is building it again.
        let plain = record(Target::Preview, &["NEXT_PUBLIC_API"]);
        assert!(!plain.built_with_env());
        let next = DeploymentRecord { framework: Some("Next.js".into()), ..plain.clone() };
        assert!(next.built_with_env());
        let message = PreviewEnvDiffers { id: "a1b2c3d4".into(), keys: vec!["NEXT_PUBLIC_API".into()], rebuild: true, source: true }.to_string();
        assert!(message.contains("NEXT_PUBLIC_API. Its build has them") && message.contains("built again with production's: `forkrail redeploy a1b2c3d4 --prod`") && message.contains("--allow-preview-env"), "{message}");
        // Deployed with --local: nothing to build it again from, so `redeploy` isn't offered.
        let message = PreviewEnvDiffers { id: "a1b2c3d4".into(), keys: vec!["NEXT_PUBLIC_API".into()], rebuild: true, source: false }.to_string();
        assert!(message.contains("deploy its source with --prod") && !message.contains("forkrail redeploy"), "{message}");
        let message = PreviewEnvDiffers { id: "a1b2c3d4".into(), keys: vec!["KEY".into()], rebuild: false, source: false }.to_string();
        assert!(message.contains("publishes its build with production's"), "{message}");
    }

    #[test]
    fn env_checks() {
        let user: BTreeMap<String, String> = [("SKIP_KEY_CHECK".to_owned(), "1".to_owned())].into();
        let env = function_env("api/main.py", Language::Python, &user, &BTreeMap::new(), &[]).unwrap().lambda;
        assert_eq!(env["FORKRAIL_APP_ENTRY"], "api/main.py");
        assert_eq!(env["PYTHONPATH"], "/var/task/python");
        assert_eq!(env["SKIP_KEY_CHECK"], "1");
        let node = function_env("server.mjs", Language::Node, &BTreeMap::new(), &BTreeMap::new(), &["analytics".to_owned()]).unwrap().lambda;
        assert!(!node.contains_key("PYTHONPATH"));
        assert_eq!(node["FORKRAIL_EGRESS"], "analytics");
        assert!(!env.contains_key("FORKRAIL_EGRESS"));
        for bad in ["AWS_LAMBDA_EXEC_WRAPPER", "AWS_EXECUTION_ENV", "FORKRAIL_APP_DIR", "PYTHONPATH", "_HANDLER", "_X_AMZN_TRACE_ID", "PATH", "HOME", "TZ", "1ABC", "A-B", ""] {
            assert!(check_env_key(bad).is_err(), "{bad}");
        }
        // What Vercel lets a project set, the AWS SDK's names among them.
        for good in ["AWS_REGION", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_S3_BUCKET", "LAMBDA_FOO", "_PRIVATE", "A"] {
            assert!(check_env_key(good).is_ok(), "{good}");
        }
        // Names Lambda won't take go in the sealed file; the rest stay where they were.
        let keys: BTreeMap<String, String> = [("AWS_ACCESS_KEY_ID".to_owned(), "AKIA".to_owned())].into();
        assert!(function_env("server.mjs", Language::Node, &keys, &BTreeMap::new(), &[]).unwrap().sealed.is_some());
        for name in ["_PRIVATE", "A"] {
            let odd: BTreeMap<String, String> = [(name.to_owned(), "1".to_owned())].into();
            assert!(function_env("server.mjs", Language::Node, &odd, &BTreeMap::new(), &[]).unwrap().sealed.is_some(), "{name}");
        }
        let bucket: BTreeMap<String, String> = [("AWS_S3_BUCKET".to_owned(), "b".to_owned())].into();
        assert!(function_env("server.mjs", Language::Node, &bucket, &BTreeMap::new(), &[]).unwrap().sealed.is_none());
        // The system variables are the app's too, and the project's own win.
        let system: BTreeMap<String, String> = [("VERCEL_ENV".to_owned(), "preview".to_owned()), ("VERCEL".to_owned(), "1".to_owned())].into();
        let mine: BTreeMap<String, String> = [("VERCEL_ENV".to_owned(), "mine".to_owned())].into();
        let both = function_env("server.mjs", Language::Node, &mine, &system, &[]).unwrap().lambda;
        assert_eq!((both["VERCEL_ENV"].as_str(), both["VERCEL"].as_str()), ("mine", "1"));
        // The bypass secret's env var: forkrail sets it, so it publishes; nobody types it in.
        assert!(check_env_key(BYPASS_SECRET_ENV).is_ok() && check_user_env_key(BYPASS_SECRET_ENV).is_err());
        assert!(check_user_env_key("FORKRAIL_APP_DIR").is_err() && check_user_env_key("API_KEY").is_ok());
        let secret: BTreeMap<String, String> = [(BYPASS_SECRET_ENV.to_owned(), "s".to_owned())].into();
        assert_eq!(function_env("server.mjs", Language::Node, &secret, &BTreeMap::new(), &[]).unwrap().lambda[BYPASS_SECRET_ENV], "s");
        // Past Lambda's 4 KB, the project's go into a sealed file and Lambda keeps the runtime's.
        let big: BTreeMap<String, String> = [("BIG".to_owned(), "x".repeat(4000)), ("SMALL".to_owned(), "1".to_owned())].into();
        let sealed = function_env("api/main.py", Language::Python, &big, &BTreeMap::new(), &[]).unwrap();
        assert_eq!(sealed.sealed.as_ref(), Some(&big));
        assert!(!sealed.lambda.contains_key("BIG") && !sealed.lambda.contains_key("SMALL"));
        assert_eq!(sealed.lambda["FORKRAIL_APP_ENTRY"], "api/main.py");
        // 64 KB in all, names and values together, as on Vercel.
        let most: BTreeMap<String, String> = [("A".to_owned(), "x".repeat(65_535))].into();
        assert!(function_env("api/main.py", Language::Python, &most, &BTreeMap::new(), &[]).is_ok());
        let over: BTreeMap<String, String> = [("AB".to_owned(), "x".repeat(65_535))].into();
        assert!(function_env("api/main.py", Language::Python, &over, &BTreeMap::new(), &[]).unwrap_err().to_string().contains("65536"));
        // What fits stays as it was: no file.
        assert!(function_env("api/main.py", Language::Python, &user, &BTreeMap::new(), &[]).unwrap().sealed.is_none());
    }

    #[test]
    fn project_names() {
        assert_eq!(project_from_dir(Path::new("/x/orbit 2")).unwrap(), "orbit-2");
        assert_eq!(project_from_dir(Path::new("/x/realestate.com.au")).unwrap(), "realestate-com-au");
        assert_eq!(project_from_dir(Path::new("/x/python-fastapi")).unwrap(), "python-fastapi");
        assert!(check_project("atlas-v").is_ok());
        assert!(check_project("Atlas").is_err());
        assert!(check_project("-x").is_err());
        assert!(check_project(&"a".repeat(41)).is_err());
        for reserved in ["www", "api", "app", "docs", "dashboard", "admin", "status", "mail"] {
            assert!(check_project(reserved).is_err(), "{reserved}");
        }
        assert!(check_project("api-v2").is_ok());
        assert!(check_project("pyapp-a1b2c3d4").is_err());
        assert!(check_project("pyapp-deadbeef").is_err());
        assert!(check_project("pyapp-a1b2c3d").is_ok());
        assert!(check_project("pyapp-a1b2c3d4e").is_ok());
        assert!(check_project("pyapp-a1b2c3dg").is_ok());
        assert_eq!(hosts_for("pyapp", "ab12cd34", "forkrail.test", true), ["pyapp-ab12cd34.forkrail.test", "pyapp.forkrail.test"]);
    }

    #[test]
    fn a_branch_has_a_host_of_one_label() {
        let host = |project: &str, branch: &str| branch_host(project, branch, "forkrail.test");
        assert_eq!(host("pyapp", "feature/Logs"), "pyapp-git-feature-logs.forkrail.test");
        assert_eq!(host("pyapp", "fix_#12--again."), "pyapp-git-fix-12-again.forkrail.test");
        assert_eq!(host("pyapp", "dependabot/npm_and_yarn/vite-8.0.1"), "pyapp-git-dependabot-npm-and-yarn-vite-8-0-1.forkrail.test");
        // Too long for a label: cut, with a few digits of the whole name's hash, so two long
        // branches that start alike keep hosts of their own.
        let long = host("pyapp", &format!("feature/{}", "a-very-long-name-".repeat(6)));
        let other = host("pyapp", &format!("feature/{}x", "a-very-long-name-".repeat(6)));
        let label = |h: &str| h.split('.').next().unwrap().to_owned();
        assert_eq!(label(&long).len(), 63, "{long}");
        assert!(long.starts_with("pyapp-git-feature-a-very-long-name-") && long != other, "{long} {other}");
        let widest = host(&"p".repeat(40), &"b".repeat(200));
        assert!(label(&widest).len() <= 63 && !label(&widest).contains("--"), "{widest}");
        // A name with nothing a host can hold still gets one.
        assert_eq!(label(&host("pyapp", "///")).len(), "pyapp-git-".len() + 6);
        for h in [long, other, widest] {
            assert!(forkrail_common::routes::one_label_under(&h, "forkrail.test") && !label(&h).ends_with('-'), "{h}");
        }
        // No project can be named into another's branch hosts.
        assert!(check_project("pyapp-git-main").unwrap_err().to_string().contains("-git-"));
        assert!(check_project("pyapp-git").is_ok() && check_project("git-pyapp").is_ok());
    }
}
