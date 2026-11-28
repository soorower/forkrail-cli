//! The publish half of a deploy: from a build folder (build.rs) to a deployment. Uploads the
//! package and the static files, publishes a Lambda version in every region, then records the
//! deployment and routes its hosts. Nothing from the project runs here.
//!
//! A preview that a push to the project's connected repository made also takes its branch's host,
//! `<project>-git-<branch>.<base>` (`branch_alias`), which so follows the branch's newest preview,
//! as on Vercel.
//!
//! Publishes of one project wait for each other: Lambda publishes a version from the function's
//! one `$LATEST`, which two at once would fight over. A lock in S3 keeps them apart; one left by a
//! publish that died expires after 15 minutes.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use forkrail_common::install::lock_key;
use forkrail_common::routes::{DEFAULT_MAX_DURATION_S, DEFAULT_MEMORY_MB, Deployment, Routes};
use serde::{Deserialize, Serialize};

use crate::build::{Build, Request};
use crate::buildlog;
use crate::system_env;
use crate::deploy::{Ctx, FunctionEnv, branch_host, check_project, function_env, hosts_for, log, mode_change, new_project, point_domains, project_domains, prune, step, warn};
use crate::lambda;
use crate::regions;
use crate::secrets::{self, Secrets};
use crate::source;
use crate::state::{self, DeploymentRecord, Floor, GitCommit, Pinned, Project, ROUTES_KEY, Settings, Status, Summary, Target, now_rfc3339, now_rfc3339_ms};
use crate::statics;
use crate::warm;

/// How long a publish may hold its project's lock before another may take it.
const LOCK_TTL_S: u64 = 15 * 60;
const LOCK_POLL: Duration = Duration::from_secs(2);

/// What publishing to a project needs from the install. Read before building, so a deploy that
/// couldn't publish fails before it builds.
pub struct Destination {
    pub project: String,
    pub target: Target,
    pub function: String,
    pub settings: Settings,
    /// Regions whose function, if there is one, was checked to be this project's: the home
    /// region, those with a router, and those the project picks. Its function regions are known
    /// only once it's built (`vercel.json`'s `regions` win), and any other is checked then.
    pub checked_regions: Vec<String>,
    /// Where the project's production runs its function now (`regions::production_regions`):
    /// where a project with no regions setting stays.
    pub production_regions: Vec<String>,
    /// The project as it is, if it exists yet.
    pub current: Option<Project>,
    /// For a preview of a Git branch: the branch, whose own preview values win over preview's.
    pub branch: Option<String>,
    /// Its env vars for the target, values and all (from SSM): for a preview of a branch with
    /// values of its own, preview's with the branch's over them, as on Vercel.
    env: BTreeMap<String, String>,
    /// Its skew protection setting in the routing table, if it has one (skew.rs).
    pub skew: Option<forkrail_common::routes::SkewProtection>,
}

impl Destination {
    /// `git` is the commit to be deployed, when known: a preview of a branch gets the branch's own
    /// preview values (`state::env_branch`).
    pub async fn read(ctx: &Ctx, project: &str, target: Target, git: Option<&GitCommit>) -> Result<Self> {
        check_project(project)?;
        let function = lambda::function_name(project);
        // Refuse if the function belongs to something else, in any region it may be published in.
        ctx.lambda.check(&function, project).await?;
        let settings = ctx.store.settings().await?;
        // The routing table holds its skew protection setting, which the build needs.
        let key = state::project_key(project);
        let (current, routes) = tokio::join!(ctx.store.get_value::<Project>(&key), ctx.store.get_value::<Routes>(ROUTES_KEY));
        let current = current?;
        let mut checked_regions = regions::router_regions(&ctx.install.home_region, &settings);
        for region in current.iter().flat_map(|p| p.regions.iter()) {
            if !checked_regions.contains(region) {
                checked_regions.push(region.clone());
            }
        }
        for region in checked_regions.iter().filter(|r| **r != ctx.install.home_region) {
            ctx.lambda_in(region).check(&function, project).await.with_context(|| format!("in {region}"))?;
        }
        // A first deploy makes the project: not under a name another project answers to.
        if current.is_none() {
            crate::project::check_new(ctx, project).await?;
        }
        let routes = routes?;
        let production_regions = match (&current, &routes) {
            (Some(p), Some(routes)) => regions::production_regions(&ctx.install.home_region, routes, p),
            _ => Vec::new(),
        };
        let skew = routes.and_then(|mut r| r.projects.remove(project)).and_then(|p| p.skew);
        let branch = state::env_branch(target, git);
        let env = match &current {
            Some(p) => {
                secrets::check_moved(p)?;
                let env = Secrets::new(ctx).deployed(project, target, branch).await?;
                // A name listed without a value would deploy without it: say so.
                let missing: Vec<String> = p.env_names.deployed(target, branch).into_iter().filter(|k| !env.contains_key(k)).collect();
                if !missing.is_empty() {
                    eprintln!("warning: {project} lists {} for {} but has no value for them", missing.join(", "), target.as_str());
                }
                env
            }
            None => BTreeMap::new(),
        };
        let branch = branch.map(str::to_owned);
        Ok(Self { project: project.to_owned(), target, function, settings, checked_regions, production_regions, current, branch, env, skew })
    }

    /// The project's env vars for the target.
    pub fn env(&self) -> BTreeMap<String, String> {
        self.env.clone()
    }

    /// Publish with `env` instead: the values a remote build was made with, read when it started,
    /// so the function runs with what its build saw.
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn egress(&self) -> Vec<String> {
        self.current.as_ref().map(|p| p.egress.clone()).unwrap_or_default()
    }

    pub fn role(&self) -> Option<String> {
        self.current.as_ref().and_then(|p| p.role.clone())
    }

    /// What to build for deployment `id`.
    pub fn request(&self, id: &str) -> Request {
        let (skew, skew_if_framework) = crate::skew::at_build(self.skew.as_ref(), self.current.as_ref());
        Request {
            id: id.to_owned(),
            project: self.project.clone(),
            name: self.current.as_ref().and_then(|p| p.display_name.clone()),
            target: self.target,
            base_domain: self.settings.base_domain.clone(),
            env: self.env(),
            git: None,
            deployer: None,
            pushed: false,
            previous_sha: None,
            framework: self.current.as_ref().and_then(|p| p.framework.clone()),
            skew,
            skew_if_framework,
        }
    }
}

/// Settings given when publishing, which win over the build's.
#[derive(Default)]
pub struct Options {
    pub memory: Option<u32>,
    pub concurrent: Option<bool>,
    /// A production deployment that isn't put live: only its own host points at it.
    pub skip_domain: bool,
    /// Refuse unless the files and settings are production's.
    pub same_source: bool,
    /// The deployment whose build is being published again, for a redeploy.
    pub redeploy_of: Option<String>,
}

/// Where the build's package and static files are.
#[derive(Clone, Copy)]
pub enum Files<'a> {
    /// In the folder `forkrail build` made, uploaded as needed.
    Folder(&'a Path),
    /// Already in the install's buckets: a deployment's build, published again.
    Uploaded,
}

/// Memory, maxDuration and concurrent compute: the flags', then `vercel.json`'s (`config`), then the project's
/// settings, then the defaults. A deploy, a push's build and a redeploy all take them this way, so
/// a project that goes back to a default gets the default on its next one.
/// Per-file functions' `floor` raises memory and maxDuration to what the functions that set them ask for.
pub fn settings(opts: &Options, (memory, max_duration, concurrent): (Option<u32>, Option<u64>, Option<bool>), floor: Option<Floor>, project: Option<&Project>) -> (u32, u64, bool) {
    let floor = floor.unwrap_or_default();
    (
        opts.memory.or(memory).or(project.and_then(|p| p.memory_mb)).unwrap_or(DEFAULT_MEMORY_MB).max(floor.memory_mb.unwrap_or(0)),
        max_duration.or(project.and_then(|p| p.max_duration_s)).unwrap_or(DEFAULT_MAX_DURATION_S).max(floor.max_duration_s.unwrap_or(0)),
        opts.concurrent.or(concurrent).or(project.and_then(|p| p.concurrent)).unwrap_or(true),
    )
}

/// A deployment just published.
#[derive(Debug, Clone, Serialize)]
pub struct Published {
    pub id: String,
    pub project: String,
    pub target: Target,
    /// Whether it serves the project's production host and domains.
    pub live: bool,
    /// Its own URL first, then the production ones it serves.
    pub urls: Vec<String>,
}

/// Publish a build: from the folder `forkrail build` made, or (`Files::Uploaded`) a deployment's
/// build that's in the buckets already. `started` is when the deploy began, for the time it reports.
pub async fn publish(ctx: &Ctx, dest: &Destination, files: Files<'_>, build: &Build, opts: &Options, started: Instant) -> Result<Published> {
    if build.project != dest.project || build.target != dest.target {
        bail!("the build is for {} ({}), not {} ({})", build.project, build.target.as_str(), dest.project, dest.target.as_str());
    }
    if opts.skip_domain && build.target != Target::Production {
        bail!("--skip-domain is for production builds; this one is a {}", build.target.as_str());
    }
    step(buildlog::PUBLISHING);
    let (project, id, target, function) = (&dest.project, &build.id, build.target, &dest.function);
    // A production deployment goes live unless asked to wait for `promote`, or its project has
    // "Auto-assign custom domains" off (read now, so a change made while it built counts).
    let held = target == Target::Production && !opts.skip_domain && !auto_assigns(ctx, dest).await?;
    let live = target == Target::Production && !opts.skip_domain && !held;
    // A static site has no function: no package, no Lambda version, nothing to warm.
    let has_function = build.package.is_some();
    let (memory_mb, max_duration_s, concurrent) = settings(opts, (build.memory_mb, build.max_duration_s, build.concurrent), build.floor, dest.current.as_ref());
    if has_function && let Some(warning) = mode_change(ctx, dest.current.as_ref(), concurrent, "this deployment").await? {
        let hint = if concurrent { ". Set \"fluid\": false in vercel.json or pass --concurrent false to keep one per worker" } else { "" };
        warn(&format!("{warning}{hint}"));
    }
    let (user_env, egress, role) = (dest.env(), dest.egress(), dest.role());
    if has_function && !build.schedules.is_empty() {
        check_cron_secret(user_env.get(forkrail_common::schedules::SECRET_ENV).map(String::as_str))?;
    }
    let branch_alias = branch_alias(ctx, dest, build).await?;
    // The branch whose host it takes: that branch's domains follow it too.
    let alias_branch = branch_alias.as_ref().and(build.git.as_ref()).map(|g| g.branch.clone());

    // The app's system variables (system_env.rs): this deployment's hosts and commit.
    let mut system = system_env::runtime(&system_env::Deployment {
        project,
        name: dest.current.as_ref().map_or(project, |p| p.shown_name()),
        id,
        target,
        base_domain: &dest.settings.base_domain,
        git: build.git.as_ref(),
        branch_url: branch_alias.as_deref(),
        user: &user_env,
        skew: build.skew_protection,
    });
    // Where the install keeps its state, which a function in another region can't tell from its own
    // (the dashboard reads it for its tables, bucket and functions).
    system.insert(forkrail_common::install::HOME_REGION_ENV.to_owned(), ctx.install.home_region.clone());
    // Next.js's caches are the router's (N2): the runtime hands Next forkrail's handlers.
    if has_function && build.next.as_ref().is_some_and(|n| n.shared_cache) {
        system.insert(crate::next::CACHE_SWITCH.to_owned(), "1".to_owned());
    }
    let package = build.package.as_ref();
    let (reads_sealed, app_aws_keys) = (package.is_some_and(|p| p.reads_sealed_env), package.is_some_and(|p| p.app_aws_keys));
    let mut system_included = has_function;
    let env = if has_function {
        let mut planned = function_env(&build.entry, build.language, &user_env, &system, &egress)?;
        if planned.sealed.is_some() && !reads_sealed {
            // An older runtime can't open a sealed file: without the system variables, the
            // project's own may still fit as they always did.
            let without = function_env(&build.entry, build.language, &user_env, &BTreeMap::new(), &egress)?;
            if without.sealed.is_none() {
                warn(&format!(
                    "deployment {id} was built with an older forkrail runtime, so its app goes without the system variables (VERCEL_ENV, VERCEL_URL …): they don't fit beside the project's env vars in Lambda's 4 KB. Deploy again to build it with a current one"
                ));
                system_included = false;
                planned = without;
            }
        }
        Some(planned)
    } else {
        None
    };
    // Past Lambda's 4 KB, or with names Lambda keeps for itself (AWS_ACCESS_KEY_ID …), the app's
    // variables are sealed into a file with a key of this deployment's own, one file for every
    // region.
    let (env, env_file) = match env {
        Some(FunctionEnv { mut lambda, sealed: Some(sealed) }) => {
            if !reads_sealed {
                bail!(
                    "{project}'s env vars need a sealed file (they take {} bytes, past what Lambda's environment holds, or have names Lambda keeps for itself, like AWS_ACCESS_KEY_ID), and deployment {id} was built with an older forkrail runtime, which can't open one: build it again with a current one (deploy again)",
                    forkrail_common::envfile::size(&user_env)
                );
            }
            if !app_aws_keys && sealed.keys().any(|k| AWS_KEY_NAMES.contains(&k.as_str())) {
                bail!("{project} sets its app's own AWS credentials (AWS_ACCESS_KEY_ID …), and deployment {id} was built with a forkrail runtime too old to hand them over: build it again with a current one (deploy again)");
            }
            let (key, file) = forkrail_common::envfile::seal(&sealed).map_err(anyhow::Error::msg)?;
            lambda.insert(forkrail_common::envfile::KEY_VAR.to_owned(), key);
            log(&format!("env vars: {} in a sealed file ({} bytes)", sealed.len(), forkrail_common::envfile::size(&sealed)));
            (lambda, Some(file))
        }
        Some(FunctionEnv { lambda, sealed: None }) => (lambda, None),
        None => (BTreeMap::new(), None),
    };
    if opts.same_source {
        same_as_production(ctx, dest, build, (memory_mb, max_duration_s, concurrent)).await?;
    }


    let package_key = build.package.as_ref().map(|p| format!("packages/{project}/{}.zip", p.sha256));
    if let (Some(package), Some(package_key)) = (&build.package, &package_key) {
        let exists = ctx.store.exists(package_key).await?;
        let uploaded = match files {
            _ if exists => "already uploaded",
            Files::Folder(out) => {
                ctx.store.upload(package_key, &Build::package_path(out)).await?;
                "uploaded"
            }
            Files::Uploaded => bail!("deployment {}'s package {package_key} is gone, so it can't be published again", opts.redeploy_of.as_deref().unwrap_or("?")),
        };
        log(&format!("package {}: {uploaded}", &package.sha256[..12]));
    }
    let kept_source = keep_source(ctx, project, files, build).await?;

    // Where its function runs: vercel.json's `regions`, else the project's, else the home region
    // alone. Its own region first (the routing table entry's `region`); the others get replicas.
    let function_regions = if has_function { regions::function_regions(&ctx.install.home_region, build.regions.as_deref(), dest.current.as_ref(), &dest.production_regions) } else { Vec::new() };
    regions::check_ready(ctx, &function_regions).await?;
    for region in function_regions.iter().filter(|r| !dest.checked_regions.contains(r)) {
        ctx.lambda_in(region).check(function, project).await.with_context(|| format!("in {region}"))?;
    }
    let own_region = function_regions.first().cloned().unwrap_or_else(|| ctx.install.home_region.clone());
    // Static files and routing rules go up first, to every region with a router, so each router
    // finds them as soon as the deployment is routed. The home region's bucket always has them.
    let router_regions = regions::router_regions(&ctx.install.home_region, &dest.settings);
    let regions: Vec<&str> = router_regions.iter().map(String::as_str).collect();
    if let Some(prepared) = &build.static_files {
        let mut sent = 0;
        for region in &regions {
            sent += match files {
                Files::Folder(out) => statics::upload(ctx, region, out, &prepared.id).await,
                Files::Uploaded => statics::ensure(ctx, region, &prepared.id).await,
            }
            .with_context(|| format!("static files in {region}"))?;
        }
        log(&format!("static files: {sent} objects uploaded"));
    }
    if let Some(rules) = &build.rules {
        let out = match files {
            Files::Folder(out) => Some(out),
            Files::Uploaded => None,
        };
        for region in &regions {
            statics::upload_rules(ctx, region, out, rules).await.with_context(|| format!("routing rules in {region}"))?;
        }
        log(&format!("routing rules {}: in {}", &rules[..12], regions.join(", ")));
    }
    if let Some(images) = &build.images {
        let out = match files {
            Files::Folder(out) => Some(out),
            Files::Uploaded => None,
        };
        for region in &regions {
            statics::upload_object(ctx, region, out, &forkrail_common::statics::images_key(images), "application/json")
                .await
                .with_context(|| format!("image settings in {region}"))?;
        }
        log(&format!("image settings {}: in {}", &images[..12], regions.join(", ")));
    }
    // Next.js's prerendered pages, as the shared cache's entries: read by a router the first time
    // one is asked for.
    if let Some(seed) = build.next.as_ref().and_then(|n| n.seed) {
        let sent = seed_next(ctx, files, build, opts, &regions).await?;
        log(&format!("Next.js cache seed: {} pages ({:.1} MB), {sent} objects in {}", seed.entries, seed.bytes as f64 / 1e6, regions.join(", ")));
    }
    // N3: a Next.js deployment's config for the router, beside its static files.
    if let Some(next) = &build.next_router {
        let out = match files {
            Files::Folder(out) => Some(out),
            Files::Uploaded => None,
        };
        for region in &regions {
            statics::upload_object(ctx, region, out, &forkrail_common::next::config_key(&next.config), "application/json")
                .await
                .with_context(|| format!("Next.js router config in {region}"))?;
        }
        log(&format!("Next.js router config {}: in {}", &next.config[..12], regions.join(", ")));
    }

    // Each region's functions are encrypted with that region's key.
    let mut keys = BTreeMap::new();
    for region in &function_regions {
        keys.insert(region.clone(), ctx.env_key(region).await?);
    }
    let cfg = has_function.then(|| lambda::Config {
        runtime: build.runtime().to_owned(),
        memory_mb,
        env,
        role_arn: ctx.install.role_arn(role.as_deref()),
        kms_key_arn: keys[own_region.as_str()].clone(),
        env_file,
    });
    let code_sha256 = build.package.as_ref().map(|p| p.sha256_b64()).transpose()?;
    let description = format!("forkrail deployment {id}");
    let lock = Lock::acquire(ctx, project, id).await?;
    let published = async {
        // Deleted while this waited for its lock (project.rs `delete` holds it): nothing of it may
        // come back, its env values least of all.
        if dest.current.is_some() && ctx.store.get_value::<Project>(&state::project_key(project)).await?.is_none() {
            bail!("{project} was deleted while deployment {id} waited to publish; nothing was published");
        }
        let (version, replicas, got) = match (&cfg, &package_key, &code_sha256) {
            (Some(cfg), Some(package_key), Some(code_sha256)) => {
                log(&format!("{function}: publishing in {}", function_regions.join(", ")));
                // Every region at the same time; every one must succeed. The package is in the home
                // region's bucket already; Lambda elsewhere reads it from that region's own.
                let local = match files {
                    Files::Folder(out) => Some(Build::package_path(out)),
                    Files::Uploaded => None,
                };
                let description = &description;
                let each = function_regions.iter().map(|region| {
                    let (local, cfg) = (local.as_deref(), lambda::Config { kms_key_arn: keys[region.as_str()].clone(), ..cfg.clone() });
                    async move {
                        let bucket = if *region == ctx.install.home_region { ctx.store.bucket.clone() } else { regions::stage_package(ctx, region, package_key, local).await? };
                        let version = ctx
                            .lambda_in(region)
                            .publish_package(function, project, &cfg, (&bucket, package_key), code_sha256, description)
                            .await
                            .with_context(|| format!("in {region}"))?;
                        Ok::<_, anyhow::Error>((region.clone(), version))
                    }
                });
                let published = futures_util::future::join_all(each).await.into_iter().collect::<Result<Vec<_>>>()?;
                let got: BTreeMap<String, u32> = published.iter().map(|(region, (_, memory))| (region.clone(), *memory)).collect();
                let mut published = published.into_iter().map(|(region, (version, _))| (region, version));
                let (_, version) = published.next().context("no region to publish in")?;
                (version, published.collect::<BTreeMap<_, _>>(), got)
            }
            _ => (String::new(), BTreeMap::new(), BTreeMap::new()),
        };
        // Where the account allows less memory than asked (a new account), the function got that.
        note_memory_limits(ctx, &dest.settings, memory_mb, &got).await;
        let memory_mb = got.values().copied().min().map_or(memory_mb, |got| got.min(memory_mb));

        let record = DeploymentRecord {
            id: id.clone(),
            project: project.clone(),
            target,
            created: now_rfc3339(),
            status: Status::Ready,
            error: None,
            via: None,
            building_at: None,
            ended_at: Some(now_rfc3339_ms()),
            log: None,
            function: if has_function { function.clone() } else { String::new() },
            version: version.clone(),
            runtime: cfg.as_ref().map(|c| c.runtime.clone()).unwrap_or_default(),
            entry: build.entry.clone(),
            memory_mb: if has_function { memory_mb } else { 0 },
            max_duration_s: if has_function { max_duration_s } else { 0 },
            concurrent,
            source_sha256: build.source_sha256.clone(),
            package_sha256: build.package.as_ref().map(|p| p.sha256.clone()).unwrap_or_default(),
            package_key: package_key.clone().unwrap_or_default(),
            package_bytes: build.package.as_ref().map_or(0, |p| p.zipped),
            reads_sealed_env: build.package.as_ref().is_some_and(|p| p.reads_sealed_env),
            app_aws_keys,
            system_env: system_included,
            env_keys: user_env.keys().cloned().collect(),
            hosts: hosts_for(project, id, &dest.settings.base_domain, live).into_iter().chain(branch_alias.clone()).collect(),
            config: build.config.clone(),
            region: Some(own_region.clone()),
            replicas: replicas.clone(),
            framework: build.framework.clone(),
            deployer: build.deployer.clone(),
            git: build.git.clone(),
            static_files: build.static_files.as_ref().map(|s| s.id.clone()),
            static_count: build.static_files.as_ref().map(|s| s.files),
            static_bytes: build.static_files.as_ref().map(|s| s.bytes),
            rules: build.rules.clone(),
            images: build.images.clone(),
            role: role.clone(),
            egress: egress.clone(),
            redeploy_of: opts.redeploy_of.clone(),
            kept_source: kept_source.clone(),
            builder: None,
            upload: None,
            github_deployment: None,
            pinned: Some(Pinned { memory_mb: build.memory_mb, max_duration_s: build.max_duration_s, concurrent: build.concurrent, floor: build.floor, regions: build.regions.clone() }),
            importer: build.importer.clone(),
            schedules: build.schedules.clone(),
            functions: build.functions.clone(),
            deploy_hook: None,
            skew_protection: build.skew_protection,
            skew_id: build.skew_id.clone(),
            outline: build.outline.clone(),
            next: build.next.clone(),
            next_router: build.next_router.clone(),
        };
        // The id was chosen before the build, which was told its URL; one in four billion clashes.
        // A deployment recorded as building (track.rs) is this one: it keeps when it started, how
        // it was asked for, where it was built, and its log.
        let record = ctx
            .store
            .update::<DeploymentRecord, _>(&state::deployment_key(id), |current| match current {
                None => Ok(record.clone()),
                Some(r) if r.project == *project && r.status.in_progress() => Ok(DeploymentRecord {
                    created: r.created,
                    via: r.via,
                    building_at: r.building_at,
                    log: r.log,
                    builder: r.builder,
                    upload: r.upload,
                    github_deployment: r.github_deployment,
                    deploy_hook: r.deploy_hook,
                    ..record.clone()
                }),
                Some(_) => bail!("a deployment {id} exists already; build again for a new id"),
            })
            .await?;

        let mut entry = Deployment {
            project: project.clone(),
            function: record.function.clone(),
            qualifier: version.clone(),
            region: Some(own_region.clone()),
            replicas: replicas.clone(),
            // With concurrent compute on, the project's cap on requests per worker; `set_single`
            // below makes it one for concurrent compute off.
            max_inflight: dest.current.as_ref().and_then(|p| p.max_inflight),
            cpu_target_pct: None,
            max_workers: dest.current.as_ref().and_then(|p| p.max_workers),
            idle_ms: None,
            headroom_pct: None,
            max_duration_s,
            memory_mb,
            target: Some(target.as_str().to_owned()),
            runtime: Some(record.runtime.clone()).filter(|r| !r.is_empty()),
            entry: Some(record.entry.clone()).filter(|e| !e.is_empty()),
            env: BTreeMap::new(),
            static_files: record.static_files.clone(),
            egress: if has_function { egress.clone() } else { Vec::new() },
            rules: record.rules.clone(),
            images: record.images.clone(),
            schedules: if has_function { record.schedules.clone() } else { Vec::new() },
            // For skew protection's maximum age, counted from creation as on Vercel: for a build
            // in the builder, when it was asked for.
            created: state::parse_rfc3339(&record.created),
            skew_id: record.skew_id.clone(),
            next: record.next_router.clone(),
        };
        if !concurrent && has_function {
            entry.set_single();
        }
        // Before production points at it: environments with the app started, in every region it
        // runs in, so its first requests don't wait for them (warm.rs).
        if live && has_function {
            let mut versions = vec![(own_region.clone(), version.clone())];
            versions.extend(replicas.iter().map(|(region, version)| (region.clone(), version.clone())));
            warm::versions(ctx, function, &versions, concurrent).await;
        }
        let routes = ctx
            .store
            .update::<Routes, _>(ROUTES_KEY, |routes| {
                let mut routes = routes.unwrap_or_default();
                routes.deployments.insert(id.clone(), entry.clone());
                crate::skew::default_after_build(&mut routes, project, build.skew_protection);
                for host in &record.hosts {
                    // Never a host that serves another project (no project's name allows it; a
                    // table edited by hand might).
                    if routes.lookup(host).is_some_and(|(_, d)| d.project != *project) {
                        continue;
                    }
                    routes.hosts.insert(host.clone(), id.clone());
                }
                if live {
                    point_domains(&mut routes, project, id);
                }
                if let Some(branch) = &alias_branch
                    && routes.hosts.get(branch_alias.as_deref().unwrap_or_default()) == Some(id)
                {
                    crate::domains::point_branch_domains(&mut routes, project, branch, id);
                }
                routes.validate().map_err(anyhow::Error::msg)?;
                Ok(routes)
            })
            .await?;
        let summary = Summary { id: id.clone(), target, created: record.created.clone(), version: version.clone(), status: Status::Ready };
        ctx.store
            .update::<Project, _>(&state::project_key(project), |current| {
                let mut p = current.unwrap_or_else(|| new_project(project));
                list_published(&mut p, summary.clone());
                if live {
                    p.production = Some(id.clone());
                    p.production_history.retain(|d| d != id);
                    p.production_history.insert(0, id.clone());
                }
                Ok(p)
            })
            .await?;
        Ok::<_, anyhow::Error>((version, replicas, record, routes))
    }
    .await;
    lock.release(ctx).await;
    let (version, replicas, record, routes) = published?;
    crate::events::deployment(ctx, crate::events::DEPLOYMENT_SUCCEEDED, &record).await;
    if live {
        crate::events::deployment(ctx, crate::events::DEPLOYMENT_PROMOTED, &record).await;
    }

    if has_function {
        let elsewhere: String = replicas.iter().map(|(r, v)| format!(", {r} version {v}")).collect();
        log(&format!("deployment {id}: {} version {version}{elsewhere}, {:.1} s", record.region.as_deref().unwrap_or_default(), started.elapsed().as_secs_f64()));
    } else {
        log(&format!("deployment {id}: static files only, {:.1} s", started.elapsed().as_secs_f64()));
    }
    if live {
        crate::schedules::after_production_change(ctx, project).await;
    }
    let pruned = prune(ctx, project).await?;
    if !pruned.is_empty() {
        log(&format!("pruned {} old deployments: {}", pruned.len(), pruned.join(", ")));
    }
    let mut urls: Vec<String> = record.hosts.iter().map(|h| dest.settings.url(h)).collect();
    if live {
        urls.extend(project_domains(&routes, project).iter().map(|d| format!("https://{d}")));
    }
    for url in &urls {
        println!("{url}");
    }
    if held {
        log(&format!("{project}: \"Auto-assign custom domains\" is off, so production is unchanged; `forkrail promote {id}` puts this deployment live"));
    } else if !live && target == Target::Production {
        log(&format!("{project}: production is unchanged; `forkrail promote {id}` puts this deployment live"));
    }
    Ok(Published { id: id.clone(), project: project.clone(), target, live, urls })
}

/// Whether the project's production deployments go live as they're published ("Auto-assign
/// custom domains", on unless turned off), as the project file says now.
/// Put a Next.js build's seed in every region it runs in: from the build folder, or, for a
/// deployment's build published again under a new id, from that deployment's, written again as
/// this one's. Returns how many objects were sent.
async fn seed_next(ctx: &Ctx, files: Files<'_>, build: &Build, opts: &Options, regions: &[&str]) -> Result<usize> {
    match files {
        Files::Folder(out) => {
            let mut sent = 0;
            for region in regions {
                sent += crate::nextseed::upload(ctx, region, &out.join(crate::next::SEED_DIR), &build.id).await.with_context(|| format!("Next.js cache seed in {region}"))?;
            }
            Ok(sent)
        }
        Files::Uploaded => match opts.redeploy_of.as_deref() {
            Some(from) => crate::nextseed::carry(ctx, from, &build.id, regions).await.with_context(|| format!("deployment {from}'s Next.js cache seed")),
            None => Ok(0),
        },
    }
}

async fn auto_assigns(ctx: &Ctx, dest: &Destination) -> Result<bool> {
    let now = ctx.store.get_value::<Project>(&state::project_key(&dest.project)).await?;
    Ok(now.or_else(|| dest.current.clone()).is_none_or(|p| p.auto_assigns_domains()))
}

/// `CRON_SECRET` goes in a header on every run of a schedule: a value that can't, or that a header
/// would change (spaces at either end, a line break pasted in), would fail every run with 401.
/// The project's own AWS credentials, which only a runtime that hands them over keeps apart from
/// the function role's (the runtime's `userenv`).
const AWS_KEY_NAMES: [&str; 3] = ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"];

pub fn check_cron_secret(value: Option<&str>) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    if value.trim() != value || hyper::header::HeaderValue::from_str(&format!("Bearer {value}")).is_err() || value.chars().any(char::is_control) {
        bail!(
            "{} has characters a header can't carry (a line break, spaces at either end, or a control character): every schedule's run would be refused. Set it again and deploy",
            forkrail_common::schedules::SECRET_ENV
        );
    }
    Ok(())
}

/// Put a published deployment in its project's list: where it has been since it started building,
/// or first if it wasn't listed. The list is in the order deployments were started, newest first,
/// which is how one that ends late knows a newer one was started after it.
pub(crate) fn list_published(p: &mut Project, summary: Summary) {
    match p.deployments.iter_mut().find(|d| d.id == summary.id) {
        Some(listed) => *listed = summary,
        None => p.deployments.insert(0, summary),
    }
}

/// The branch host this build's deployment takes when it's published, if it takes one: it's a
/// preview of a branch of the project's connected repository (never the production branch), made
/// by a push, or made again from a deployment that was given the host; and no newer preview of
/// the branch is ready, which would hold the host already.
async fn branch_alias(ctx: &Ctx, dest: &Destination, build: &Build) -> Result<Option<String>> {
    if build.target != Target::Preview {
        return Ok(None);
    }
    let Some(project) = &dest.current else { return Ok(None) };
    let (Some(link), Some(git)) = (&project.git, &build.git) else { return Ok(None) };
    if git.branch.is_empty() || git.branch == "HEAD" || git.branch == link.production_branch {
        return Ok(None);
    }
    let host = branch_host(&dest.project, &git.branch, &dest.settings.base_domain);
    let record = async |id: &str| ctx.store.get_value::<DeploymentRecord>(&state::deployment_key(id)).await;
    // A build published from a folder has no record yet, and no push behind it.
    let Some(rec) = record(&build.id).await? else { return Ok(None) };
    let of_the_branch = match (rec.via, &rec.redeploy_of) {
        (Some(via), _) if via.from_repository() => true,
        (_, Some(of)) => record(of).await?.is_some_and(|r| r.hosts.contains(&host)),
        _ => false,
    };
    if !of_the_branch {
        return Ok(None);
    }
    // Deployments started after this one, newest first: usually none.
    let started = |r: &DeploymentRecord| r.building_at.as_deref().and_then(state::parse_rfc3339_ms);
    let now = state::now_secs();
    for newer in project.deployments.iter().take_while(|d| d.id != build.id).filter(|d| d.target == Target::Preview && d.status.at(&d.created, now) == Status::Ready) {
        if let Some(r) = record(&newer.id).await?
            && r.hosts.contains(&host)
            && started(&r) > started(&rec)
        {
            log(&format!("{host} stays with deployment {}, which was started after this one", r.id));
            return Ok(None);
        }
    }
    // Branch names that differ can share a host (`Staging` and `staging` both make `-git-staging`):
    // one held by another branch's deployment stays with it, so a branch takes only its own name's.
    let routes: Routes = ctx.store.get_value(ROUTES_KEY).await?.unwrap_or_default();
    if let Some(holder) = routes.hosts.get(&host).filter(|h| **h != build.id)
        && let Some(theirs) = record(holder).await?.and_then(|r| r.git).map(|g| g.branch)
        && theirs != git.branch
    {
        log(&format!("{host} serves branch {theirs}, not {}: it stays with it", git.branch));
        return Ok(None);
    }
    Ok(Some(host))
}

/// Store the build's source, by content, beside the packages; a redeploy's is there already. A
/// source that can't be stored only means the dashboard can't show it: the publish goes on.
async fn keep_source(ctx: &Ctx, project: &str, files: Files<'_>, build: &Build) -> Result<Option<source::Archive>> {
    let Some(kept) = &build.kept_source else { return Ok(None) };
    let key = source::archive_key(project, &kept.sha256);
    if ctx.store.exists(&key).await? {
        return Ok(Some(kept.clone()));
    }
    match files {
        Files::Folder(out) => {
            ctx.store.upload(&key, &out.join(source::ARCHIVE_FILE)).await?;
            log(&format!("source: {} files kept", kept.files));
            Ok(Some(kept.clone()))
        }
        Files::Uploaded => {
            warn(&format!("{key} is gone; the new deployment has no source to show"));
            Ok(None)
        }
    }
}

/// `--same-source`: the files and settings must be production's, for deploying the same app again
/// on a new runtime.
/// Keeps the install's settings saying where Lambda allows less memory than a deployment asked
/// for (`Settings::memory_limits`, which the dashboard shows): a region that gave less is noted,
/// and one that gave more than its noted limit is taken off, AWS having raised it. A failure is
/// only a warning.
async fn note_memory_limits(ctx: &Ctx, settings: &state::Settings, asked: u32, got: &BTreeMap<String, u32>) {
    if memory_limits_after(&settings.memory_limits, asked, got).is_none() {
        return;
    }
    let saved = ctx
        .store
        .update::<state::Settings, _>(state::SETTINGS_KEY, |current| {
            let mut s = current.unwrap_or_default();
            if let Some(next) = memory_limits_after(&s.memory_limits, asked, got) {
                s.memory_limits = next;
            }
            Ok(s)
        })
        .await;
    if let Err(e) = saved {
        warn(&format!("noting the account's memory limit in the install's settings: {e:#}"));
    }
}

/// The noted memory limits after a publish that asked for `asked` MB and got `got` in each region,
/// if they change.
fn memory_limits_after(limits: &BTreeMap<String, u32>, asked: u32, got: &BTreeMap<String, u32>) -> Option<BTreeMap<String, u32>> {
    let mut next = limits.clone();
    for (region, &memory) in got {
        if memory < asked {
            next.insert(region.clone(), memory);
        } else if next.get(region).is_some_and(|&limit| memory > limit) {
            next.remove(region);
        }
    }
    (next != *limits).then_some(next)
}

async fn same_as_production(ctx: &Ctx, dest: &Destination, build: &Build, (memory_mb, max_duration_s, concurrent): (u32, u64, bool)) -> Result<()> {
    let project = &dest.project;
    let production =
        dest.current.as_ref().and_then(|p| p.production.clone()).with_context(|| format!("{project} has no production deployment"))?;
    let record: DeploymentRecord = ctx
        .store
        .get_value(&state::deployment_key(&production))
        .await?
        .with_context(|| format!("no record of {project}'s production deployment {production}"))?;
    let mut differs = Vec::new();
    if record.source_sha256 != build.source_sha256 {
        differs.push("its files".to_owned());
    }
    if record.memory_mb != memory_mb {
        differs.push(format!("memory {memory_mb} MB against {} (pass --memory {})", record.memory_mb, record.memory_mb));
    }
    if record.max_duration_s != max_duration_s {
        differs.push(format!("maxDuration {max_duration_s} s against {}", record.max_duration_s));
    }
    if record.concurrent != concurrent {
        let on = |c: bool| if c { "on" } else { "off" };
        differs.push(format!("concurrent compute {} against {}", on(concurrent), on(record.concurrent)));
    }
    if !differs.is_empty() {
        bail!("the build differs from production {production}: {}", differs.join("; "));
    }
    log(&format!("{project}: same files and settings as production {production}"));
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct LockFile {
    /// The deployment publishing.
    holder: String,
    /// Unix seconds after which another publish may take the lock.
    until: u64,
}

/// The lock's holder while a project is being deleted (project.rs `delete`).
pub(crate) const DELETE_HOLDER: &str = "delete";

/// One publish's hold on its project.
pub(crate) struct Lock {
    key: String,
    holder: String,
}

impl Lock {
    pub(crate) async fn acquire(ctx: &Ctx, project: &str, holder: &str) -> Result<Self> {
        let key = lock_key(project);
        let give_up = Instant::now() + Duration::from_secs(LOCK_TTL_S + 60);
        let mut waiting = false;
        loop {
            let mine = LockFile { holder: holder.to_owned(), until: now_secs() + LOCK_TTL_S };
            if ctx.store.create(&key, &mine).await? {
                break;
            }
            match ctx.store.get::<LockFile>(&key).await? {
                // Released in between: try again at once.
                None => continue,
                Some((theirs, etag)) if theirs.until <= now_secs() => {
                    if ctx.store.replace(&key, &mine, &etag).await? {
                        log(&format!("{project}: took over the publish lock deployment {} left", theirs.holder));
                        break;
                    }
                }
                Some((theirs, _)) => {
                    if !waiting {
                        if theirs.holder == DELETE_HOLDER {
                            log(&format!("{project}: waiting for the project's deletion to finish"));
                        } else {
                            log(&format!("{project}: waiting for deployment {} to finish publishing", theirs.holder));
                        }
                        waiting = true;
                    }
                    if Instant::now() > give_up {
                        bail!("{project}: deployment {} is still publishing; try again later", theirs.holder);
                    }
                    tokio::time::sleep(LOCK_POLL).await;
                }
            }
        }
        Ok(Self { key, holder: holder.to_owned() })
    }

    /// Best effort: a lock left behind expires anyway.
    pub(crate) async fn release(self, ctx: &Ctx) {
        if let Ok(Some((file, _))) = ctx.store.get::<LockFile>(&self.key).await
            && file.holder == self.holder
        {
            let _ = ctx.store.delete(&self.key).await;
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_the_accounts_memory_limit_until_it_rises() {
        let regions = |pairs: &[(&str, u32)]| pairs.iter().map(|(r, m)| ((*r).to_owned(), *m)).collect::<BTreeMap<_, _>>();
        // A new account: 640 asked, 512 given.
        let noted = memory_limits_after(&BTreeMap::new(), 640, &regions(&[("us-east-1", 512)])).unwrap();
        assert_eq!(noted, regions(&[("us-east-1", 512)]));
        // The same again changes nothing; less asked than the limit says nothing new either.
        assert_eq!(memory_limits_after(&noted, 640, &regions(&[("us-east-1", 512)])), None);
        assert_eq!(memory_limits_after(&noted, 256, &regions(&[("us-east-1", 256)])), None);
        // AWS raised it: 640 given where 512 was the most.
        assert_eq!(memory_limits_after(&noted, 640, &regions(&[("us-east-1", 640)])), Some(BTreeMap::new()));
        // Another region's limit is its own.
        assert_eq!(memory_limits_after(&noted, 1024, &regions(&[("us-west-2", 1024)])), None);
    }

    /// The order a deploy, a push and a redeploy all take a function's memory, maxDuration and
    /// concurrent compute from: a flag, then vercel.json, then the project's settings (the
    /// dashboard's Functions page), then the defaults; a per-file function's floor lifts them.
    #[test]
    fn function_settings_come_from_flags_then_vercel_json_then_the_project() {
        let none = Options::default();
        let project = |memory_mb, max_duration_s, concurrent| Project { memory_mb, max_duration_s, concurrent, ..Default::default() };
        // Nothing set anywhere: the defaults.
        assert_eq!(settings(&none, (None, None, None), None, None), (DEFAULT_MEMORY_MB, DEFAULT_MAX_DURATION_S, true));
        // A project that set nothing, or went back to the default: the defaults too, whatever
        // the deployment being redeployed ran with.
        assert_eq!(settings(&none, (None, None, None), None, Some(&project(None, None, None))), (DEFAULT_MEMORY_MB, DEFAULT_MAX_DURATION_S, true));
        // The project's settings.
        let saved = project(Some(1024), Some(60), Some(false));
        assert_eq!(settings(&none, (None, None, None), None, Some(&saved)), (1024, 60, false));
        // vercel.json wins over them.
        assert_eq!(settings(&none, (Some(768), Some(30), Some(true)), None, Some(&saved)), (768, 30, true));
        // A flag wins over both.
        let flags = Options { memory: Some(3008), concurrent: Some(false), ..Default::default() };
        assert_eq!(settings(&flags, (Some(768), None, Some(true)), None, Some(&saved)), (3008, 60, false));
        // A function that asks for more lifts the shared one, never lowers it.
        let floor = Floor { memory_mb: Some(1769), max_duration_s: Some(120) };
        assert_eq!(settings(&none, (None, None, None), Some(floor), Some(&saved)), (1769, 120, false));
        assert_eq!(settings(&none, (None, None, None), Some(Floor { memory_mb: Some(512), max_duration_s: None }), Some(&saved)), (1024, 60, false));
    }

    #[test]
    fn a_published_deployment_keeps_its_place_in_the_list() {
        let summary = |id: &str, status| Summary { id: id.into(), target: Target::Production, created: now_rfc3339(), version: String::new(), status };
        // Newest first: b was started after a, and a is published first.
        let mut p = Project { deployments: vec![summary("bbbb2222", Status::Building), summary("aaaa1111", Status::Building)], ..Default::default() };
        list_published(&mut p, Summary { version: "7".into(), ..summary("aaaa1111", Status::Ready) });
        assert_eq!(p.deployments.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["bbbb2222", "aaaa1111"]);
        assert_eq!((p.deployments[1].status, p.deployments[1].version.as_str()), (Status::Ready, "7"));
        // One that was never listed as building goes first.
        list_published(&mut p, summary("cccc3333", Status::Ready));
        assert_eq!(p.deployments[0].id, "cccc3333");
        assert_eq!(p.deployments.len(), 3);
    }
}
