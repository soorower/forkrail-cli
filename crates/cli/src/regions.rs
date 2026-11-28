//! Regions. Two things live in regions besides the home region:
//!
//! - Routers. `forkrail regions add REGION` says a router runs there (`Settings::regions`): every
//!   deployment's static files, routing rules and the like go to that region's bucket as well, for
//!   that router to serve, and production and its rollback targets are copied there at once.
//! - Functions. Each project picks the regions its functions run in (`Project::regions`, or
//!   `vercel.json`'s `regions`), one by default, the home region, as Vercel's default is `iad1`.
//!   A deployment's Lambda version is published in each of them (`replicas` in the routing table
//!   beside its own region's). A router runs the version in its own region when there is one, else
//!   the nearest one's, invoked from afar (`Deployment::placement`): a region needs no router to run
//!   functions.
//!
//! The home region (the CLI's `--region`) keeps the state, the packages and every deployment's
//! static files. Lambda reads code only from a bucket in the function's own region, so each other
//! region gets a code bucket of its own, `<bucket>-<region>`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration, PublicAccessBlockConfiguration, Tag, Tagging};
use forkrail_common::regions::{FUNCTION_REGIONS, function_region};
use forkrail_common::routes::Routes;

use crate::deploy::{Ctx, KEEP_PRODUCTION, log, project_record, record};
use crate::lambda::{TAG_KEY, TAG_VALUE};
use crate::state::{self, DeploymentRecord, Project, ROUTES_KEY, SETTINGS_KEY, Settings};

/// Whether `region` looks like an AWS region name: `us-west-1`, `eu-central-1`, `us-gov-west-1`.
pub fn check_region(region: &str) -> Result<()> {
    let parts: Vec<&str> = region.split('-').collect();
    let lower = |p: &&str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase());
    let ok = parts.len() >= 3
        && parts[0].len() == 2
        && parts[..parts.len() - 1].iter().all(lower)
        && parts.last().is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !ok {
        bail!("{region:?} isn't an AWS region name, such as us-west-1");
    }
    Ok(())
}

/// Region names as given (AWS's, `us-east-1`, or Vercel's, `iad1`), as AWS names them, in the
/// order given and each once. Refused: none, or one that isn't a region functions may run in
/// (`forkrail_common::regions::FUNCTION_REGIONS`).
pub fn parse<S: AsRef<str>>(names: &[S]) -> Result<Vec<String>> {
    if names.is_empty() {
        bail!("name at least one region, such as us-east-1 (iad1)");
    }
    let mut out: Vec<String> = Vec::new();
    for name in names {
        let name = name.as_ref();
        let Some(region) = function_region(name) else {
            let known: Vec<String> = FUNCTION_REGIONS.iter().map(|r| format!("{} ({})", r.aws, r.vercel)).collect();
            bail!("{name:?} isn't a region functions can run in; these are: {}", known.join(", "));
        };
        if !out.iter().any(|r| r == region.aws) {
            out.push(region.aws.to_owned());
        }
    }
    Ok(out)
}

/// The regions a deployment's functions run in: `vercel.json`'s `regions` (`pinned`), else the
/// project's setting, else where its production's function runs now (`production`, from
/// `running_in`): a project stays where it is until its setting says otherwise, so the projects
/// that ran in every router's region before function regions keep doing so. A new project, with
/// no production yet, gets the home region alone, as Vercel's default is iad1. Its own region
/// (the routing table entry's `region`) first: the home region when it's among them, where the
/// state and the packages are, else the first named.
pub fn function_regions(home: &str, pinned: Option<&[String]>, project: Option<&Project>, production: &[String]) -> Vec<String> {
    let chosen: Vec<String> = match pinned.filter(|r| !r.is_empty()) {
        Some(regions) => regions.to_vec(),
        None => match project.map(|p| p.regions.as_slice()).filter(|r| !r.is_empty()) {
            Some(regions) => regions.to_vec(),
            None if !production.is_empty() => production.to_vec(),
            None => vec![home.to_owned()],
        },
    };
    let mut ordered: Vec<String> = Vec::with_capacity(chosen.len());
    if chosen.iter().any(|r| r == home) {
        ordered.push(home.to_owned());
    }
    for region in chosen {
        if !ordered.contains(&region) {
            ordered.push(region);
        }
    }
    ordered
}

/// Where a deployment's function runs now: its own region first, then its replicas'. Empty for
/// one with no function (static files alone).
pub fn running_in(home: &str, d: &forkrail_common::routes::Deployment) -> Vec<String> {
    let mut regions: Vec<String> = Vec::new();
    if d.function.is_empty() {
        return regions;
    }
    for (region, _) in d.versions(home) {
        if !regions.iter().any(|r| r == region) {
            regions.push(region.to_owned());
        }
    }
    regions
}

/// Where project `p`'s production runs its function now, by the routing table (`running_in`).
pub fn production_regions(home: &str, routes: &Routes, p: &Project) -> Vec<String> {
    p.production.as_ref().and_then(|id| routes.deployments.get(id)).map(|d| running_in(home, d)).unwrap_or_default()
}

/// The regions with a router: the home region first, then the others (`Settings::regions`).
/// Static files, routing rules and the like go to each of them.
pub fn router_regions(home: &str, settings: &Settings) -> Vec<String> {
    let mut regions = vec![home.to_owned()];
    regions.extend(settings.regions.iter().filter(|r| *r != home).cloned());
    regions
}

/// How a project's function regions read: `us-east-1 (iad1, Washington, D.C.)`, comma-separated.
pub fn describe(regions: &[String]) -> String {
    regions
        .iter()
        .map(|r| match function_region(r) {
            Some(f) => format!("{} ({}, {})", f.aws, f.vercel, f.city),
            None => r.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What functions in `region` need that isn't there yet: Lambda there reads packages from the
/// region's own code bucket, and each function's environment is encrypted with the install's env
/// var key in its region. Empty when ready; the home region always is.
pub async fn missing(ctx: &Ctx, region: &str) -> Vec<String> {
    if region == ctx.install.home_region {
        return Vec::new();
    }
    let bucket = ctx.install.code_bucket(region);
    let (head, key) = tokio::join!(ctx.s3_in(region).head_bucket().bucket(&bucket).send(), ctx.env_key(region));
    let mut missing = Vec::new();
    match head {
        Ok(_) => {}
        Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found()) => missing.push(format!("its code bucket s3://{bucket}")),
        Err(e) => missing.push(format!("its code bucket s3://{bucket} ({})", DisplayErrorContext(&e))),
    }
    if key.is_err() {
        missing.push(format!("the env var key there ({})", forkrail_common::install::ENV_KEY_ALIAS));
    }
    missing
}

/// Sets up the regions functions can't run in yet (`missing`): each gets its code bucket and env
/// var key, made here (the control function does this when a project picks a region in the
/// dashboard). A region that can't be set up is refused, saying why.
pub async fn check_ready(ctx: &Ctx, regions: &[String]) -> Result<()> {
    let checks = regions.iter().filter(|r| **r != ctx.install.home_region).map(|r| async move { (r, missing(ctx, r).await) });
    let unready: Vec<&String> = futures_util::future::join_all(checks).await.into_iter().filter(|(_, missing)| !missing.is_empty()).map(|(region, _)| region).collect();
    let setups = unready.iter().map(|region| async move { (region, prepare(ctx, region).await) });
    let failed: Vec<String> = futures_util::future::join_all(setups)
        .await
        .into_iter()
        .filter_map(|(region, result)| result.err().map(|e| format!("{region}: {e:#}")))
        .collect();
    if failed.is_empty() {
        return Ok(());
    }
    bail!("functions can't run there yet, as setting {} up failed: {}", if failed.len() == 1 { "it" } else { "them" }, failed.join("; "))
}

/// What functions in `region` need, made where missing: its code bucket and the env var key.
pub async fn prepare(ctx: &Ctx, region: &str) -> Result<()> {
    let opt_in = function_region(region).is_some_and(|r| r.opt_in);
    let made = async {
        let bucket = ensure_code_bucket(ctx, region).await?;
        let key = ensure_env_key(ctx, region).await?;
        Ok::<_, anyhow::Error>((bucket, key))
    }
    .await;
    match made {
        Ok((bucket, _)) => {
            log(&format!("{region}: set up for functions (code bucket s3://{bucket}, env var key {})", forkrail_common::install::ENV_KEY_ALIAS));
            Ok(())
        }
        Err(e) if opt_in => Err(e.context(format!("{region} is an opt-in region: turn it on for the AWS account first (Account, AWS Regions)"))),
        Err(e) => Err(e),
    }
}

/// The env var key in `region` ([`forkrail_common::install::ENV_KEY_ALIAS`]), made if it isn't
/// there as deploy/install/secrets.yaml makes the home region's: the same policy (the account's
/// IAM decides, but the routers, the analytics box, the builder and the screenshot function may
/// never decrypt), yearly rotation. It's tagged [`ENV_KEY_TAG`], so the control function may set
/// up only keys like it and `forkrail uninstall` finds every one. AWS charges about $1 a month for
/// a key.
pub async fn ensure_env_key(ctx: &Ctx, region: &str) -> Result<String> {
    use aws_sdk_kms::types::Tag as KmsTag;
    use forkrail_common::install::ENV_KEY_ALIAS;
    let config = aws_sdk_kms::config::Builder::from(&ctx.config).region(aws_config::Region::new(region.to_owned())).build();
    let kms = aws_sdk_kms::Client::from_conf(config);
    match kms.describe_key().key_id(ENV_KEY_ALIAS).send().await {
        Ok(out) => return out.key_metadata.and_then(|k| k.arn).with_context(|| format!("{ENV_KEY_ALIAS} in {region} has no ARN")),
        Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found_exception()) => {}
        Err(e) => bail!("reading {ENV_KEY_ALIAS} in {region}: {}", aws_sdk_kms::error::DisplayErrorContext(&e)),
    }
    let (partition, account) = (&ctx.install.partition, &ctx.install.account);
    let never: Vec<String> = ["forkrail-router", "forkrail-analytics", "forkrail-builder", "forkrail-screenshot"].iter().map(|r| format!("arn:{partition}:iam::{account}:role/{r}")).collect();
    let policy = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [
            {"Sid": "AccountUsesIamPolicies", "Effect": "Allow", "Principal": {"AWS": format!("arn:{partition}:iam::{account}:root")}, "Action": "kms:*", "Resource": "*"},
            {"Sid": "NeverDecrypt", "Effect": "Deny", "Principal": "*", "Action": ["kms:Decrypt", "kms:ReEncryptFrom", "kms:CreateGrant"], "Resource": "*", "Condition": {"ArnLike": {"aws:PrincipalArn": never}}},
        ],
    });
    log(&format!("{region}: making the env var key"));
    let made = kms
        .create_key()
        .description("forkrail env var values")
        .policy(policy.to_string())
        // KMS's check that the caller could change this policy later counts the caller's own IAM
        // rights, and the control function has no right to change key policies. The policy's
        // first statement leaves the key to the account's IAM, as the default policy does, so the
        // account's administrators always can.
        .bypass_policy_lockout_safety_check(true)
        .tags(KmsTag::builder().tag_key(TAG_KEY).tag_value(TAG_VALUE).build()?)
        .tags(KmsTag::builder().tag_key(ENV_KEY_TAG).tag_value("env").build()?)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("making the env var key in {region}: {}", aws_sdk_kms::error::DisplayErrorContext(&e)))?;
    let id = made.key_metadata().map(|m| m.key_id().to_owned()).context("KMS answered without the new key")?;
    kms.enable_key_rotation()
        .key_id(&id)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("turning on the env var key's rotation in {region}: {}", aws_sdk_kms::error::DisplayErrorContext(&e)))?;
    match kms.create_alias().alias_name(ENV_KEY_ALIAS).target_key_id(&id).send().await {
        Ok(_) => {}
        // Another setup of the same region named its key first: that one is the region's key.
        // This one is never used; it's deleted now where these credentials may, and
        // `forkrail uninstall` finds it by its tag otherwise.
        Err(e) if e.as_service_error().is_some_and(|e| e.is_already_exists_exception()) => {
            log(&format!("{region}: another setup made the env var key first; using that one"));
            let _ = kms.schedule_key_deletion().key_id(&id).pending_window_in_days(7).send().await;
        }
        Err(e) => bail!("naming the env var key in {region}: {}", aws_sdk_kms::error::DisplayErrorContext(&e)),
    }
    ctx.env_key(region).await
}

/// The tag that marks an env var key the region setup made, which the control function may set up.
pub const ENV_KEY_TAG: &str = "forkrail-key";

/// `forkrail regions setup REGION`: make what functions there need, its code bucket and env var
/// key ([`prepare`]). Picking the region for a project does the same.
pub async fn setup(ctx: &Ctx, region: &str) -> Result<()> {
    let region = parse(&[region])?.remove(0);
    if region == ctx.install.home_region {
        log(&format!("{region} is the home region: functions run there already"));
        return Ok(());
    }
    prepare(ctx, &region).await?;
    log(&format!("{region}: ready for functions; a project runs there once it picks it (`forkrail project NAME --regions {region}`) and deploys"));
    Ok(())
}

/// Every region functions may run in, as the dashboard lists them: whether it has a router,
/// whether functions can run there yet (`missing`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RegionView {
    pub region: &'static str,
    pub vercel: &'static str,
    pub city: &'static str,
    pub country: &'static str,
    pub opt_in: bool,
    pub home: bool,
    pub router: bool,
    /// What functions there need first; empty when ready.
    pub missing: Vec<String>,
}

pub async fn views(ctx: &Ctx) -> Result<Vec<RegionView>> {
    let settings = ctx.store.settings().await?;
    let routers = router_regions(&ctx.install.home_region, &settings);
    let each = FUNCTION_REGIONS.iter().map(|r| {
        let routers = &routers;
        async move {
            RegionView {
                region: r.aws,
                vercel: r.vercel,
                city: r.city,
                country: r.country,
                opt_in: r.opt_in,
                home: r.aws == ctx.install.home_region,
                router: routers.iter().any(|x| x == r.aws),
                missing: missing(ctx, r.aws).await,
            }
        }
    });
    Ok(futures_util::future::join_all(each).await)
}

/// The bucket Lambda in `region` reads packages from, created private and tagged if it isn't
/// there. One that exists without the project tag is refused, and so is one of that name in
/// another account (`expected_bucket_owner`: names are global and this one is easy to guess, and
/// Lambda takes code from another account's bucket).
pub async fn ensure_code_bucket(ctx: &Ctx, region: &str) -> Result<String> {
    let bucket = ctx.install.code_bucket(region);
    let s3 = ctx.s3_in(region);
    let account = &ctx.install.account;
    match s3.head_bucket().bucket(&bucket).expected_bucket_owner(account).send().await {
        Ok(_) => {
            let tagged = match s3.get_bucket_tagging().bucket(&bucket).expected_bucket_owner(account).send().await {
                Ok(out) => out.tag_set().iter().any(|t| t.key() == TAG_KEY && t.value() == TAG_VALUE),
                Err(_) => false,
            };
            if !tagged {
                bail!("s3://{bucket} exists but isn't tagged {TAG_KEY}={TAG_VALUE}; not using it");
            }
            return Ok(bucket);
        }
        Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found()) => {}
        Err(e) => bail!("checking s3://{bucket}: {}", DisplayErrorContext(&e)),
    }
    log(&format!("creating s3://{bucket} for packages in {region}"));
    let mut create = s3.create_bucket().bucket(&bucket);
    // S3's rule, not the install's: us-east-1 refuses a location constraint naming itself.
    if region != "us-east-1" {
        create = create.create_bucket_configuration(
            CreateBucketConfiguration::builder().location_constraint(BucketLocationConstraint::from(region)).build(),
        );
    }
    create.send().await.map_err(|e| anyhow::anyhow!("creating s3://{bucket}: {}", DisplayErrorContext(&e)))?;
    s3.put_bucket_tagging()
        .bucket(&bucket)
        .tagging(Tagging::builder().tag_set(Tag::builder().key(TAG_KEY).value(TAG_VALUE).build()?).build()?)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("tagging s3://{bucket}: {}", DisplayErrorContext(&e)))?;
    s3.put_public_access_block()
        .bucket(&bucket)
        .public_access_block_configuration(
            PublicAccessBlockConfiguration::builder()
                .block_public_acls(true)
                .ignore_public_acls(true)
                .block_public_policy(true)
                .restrict_public_buckets(true)
                .build(),
        )
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("blocking public access to s3://{bucket}: {}", DisplayErrorContext(&e)))?;
    Ok(bucket)
}

/// Put package `key` in `region`'s code bucket, from `local` if given, else copied from the home
/// bucket, unless it's there already (keys name the package's hash). Returns the bucket.
pub async fn stage_package(ctx: &Ctx, region: &str, key: &str, local: Option<&Path>) -> Result<String> {
    let bucket = ensure_code_bucket(ctx, region).await?;
    let s3 = ctx.s3_in(region);
    match s3.head_object().bucket(&bucket).key(key).send().await {
        Ok(_) => return Ok(bucket),
        Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found()) => {}
        Err(e) => bail!("checking s3://{bucket}/{key}: {}", DisplayErrorContext(&e)),
    }
    match local {
        Some(path) => state::upload(&s3, &bucket, key, path).await?,
        None => {
            s3.copy_object()
                .bucket(&bucket)
                .key(key)
                .copy_source(format!("{}/{key}", ctx.store.bucket))
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("copying {key} to s3://{bucket}: {}", DisplayErrorContext(&e)))?;
        }
    }
    Ok(bucket)
}

/// Copy deployment `id`'s files to `region`, for a router there: its routing rules, static files,
/// image settings, Next.js router config and cache seed. Its function isn't published there.
pub async fn copy_files(ctx: &Ctx, id: &str, region: &str) -> Result<()> {
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let entry = routes.deployments.get(id).with_context(|| format!("deployment {id} isn't in the routing table"))?;
    files_to(ctx, id, entry, region).await
}

async fn files_to(ctx: &Ctx, id: &str, entry: &forkrail_common::routes::Deployment, region: &str) -> Result<()> {
    if region == ctx.install.home_region {
        return Ok(());
    }
    if let Some(rules) = &entry.rules {
        crate::statics::upload_rules(ctx, region, None, rules).await?;
    }
    if let Some(manifest) = &entry.static_files {
        crate::statics::ensure(ctx, region, manifest).await?;
    }
    if let Some(images) = &entry.images {
        crate::statics::upload_object(ctx, region, None, &forkrail_common::statics::images_key(images), "application/json").await?;
    }
    if let Some(next) = &entry.next {
        crate::statics::upload_object(ctx, region, None, &forkrail_common::next::config_key(&next.config), "application/json").await?;
        // Its cache seed (nextseed.rs): where a router there reads its prerendered pages from, the
        // package having left them out.
        let rec = record(ctx, id).await?;
        if rec.next.as_ref().is_some_and(|n| n.seed.is_some()) {
            crate::nextseed::carry(ctx, id, id, &[region]).await.with_context(|| format!("deployment {id}'s Next.js cache seed"))?;
        }
    }
    Ok(())
}

/// Publish deployment `id` in `region`: the package it was built from, with the configuration of
/// its version in its own region, checked to be the same code, and its files for a router there.
/// The version is added to its entry in the routing table, which the routers pick up within
/// seconds, and to its record. Returns the version, or `None` if it already has one there.
pub async fn replicate(ctx: &Ctx, id: &str, region: &str) -> Result<Option<String>> {
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let entry = routes.deployments.get(id).with_context(|| format!("deployment {id} isn't in the routing table"))?;
    let home = entry.home(&ctx.install.home_region).to_owned();
    if home == region || entry.replicas.contains_key(region) {
        return Ok(None);
    }
    // Its routing rules and static files are what a router there reads first.
    files_to(ctx, id, entry, region).await?;
    if !entry.has_function() {
        // A static site: its files in the region are all it needs; the router there serves them.
        return Ok(None);
    }
    let rec = record(ctx, id).await?;
    let (mut cfg, code_sha256) = ctx.lambda_in(&home).version_config(&entry.function, &entry.qualifier).await?;
    // Its env vars are in a sealed file whose layer version is gone (lambda.rs): there's nothing to
    // copy it from. The router there runs it from the home region meanwhile.
    if cfg.env.contains_key(forkrail_common::envfile::KEY_VAR) {
        bail!("its env vars didn't fit Lambda's environment, so they're in a sealed file that can't be copied; the router in {region} runs it from {home}, and a redeploy publishes it in every region");
    }
    cfg.kms_key_arn = ctx.env_key(region).await?;
    let bucket = stage_package(ctx, region, &rec.package_key, None).await?;
    let (version, _) = ctx
        .lambda_in(region)
        .publish_package(
            &entry.function,
            &entry.project,
            &cfg,
            (&bucket, &rec.package_key),
            &code_sha256,
            &format!("forkrail deployment {id}"),
        )
        .await?;
    let (region_owned, version_owned) = (region.to_owned(), version.clone());
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            // Removed meanwhile: the version stays unused, as a failed deploy's would.
            if let Some(d) = routes.deployments.get_mut(id) {
                d.region.get_or_insert_with(|| home.clone());
                d.replicas.insert(region_owned.clone(), version_owned.clone());
            }
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    ctx.store
        .update::<DeploymentRecord, _>(&state::deployment_key(id), |current| {
            let mut r = current.with_context(|| format!("no deployment {id}"))?;
            r.region.get_or_insert_with(|| home.clone());
            r.replicas.insert(region_owned.clone(), version_owned.clone());
            Ok(r)
        })
        .await?;
    Ok(Some(version))
}

/// A router runs in `region` from now on: every deploy sends its static files and routing rules
/// there too, and those of every deployment production may use (production and its rollback
/// targets, and with `all` the previews too) go there now. Functions run there only for projects
/// that pick the region (`forkrail project NAME --regions`); elsewhere the router there runs each
/// deployment's version in the nearest region it has one.
pub async fn add(ctx: &Ctx, region: &str, all: bool) -> Result<()> {
    check_region(region)?;
    if region == ctx.install.home_region {
        bail!("{region} is the home region: it has a router already");
    }
    ensure_code_bucket(ctx, region).await?;
    // First the setting, so a deploy that starts meanwhile sends its files there too.
    ctx.store
        .update::<Settings, _>(SETTINGS_KEY, |current| {
            let mut s = current.unwrap_or_default();
            if !s.regions.iter().any(|r| r == region) {
                s.regions.push(region.to_owned());
            }
            Ok(s)
        })
        .await?;

    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let projects: BTreeSet<&str> = routes.deployments.values().map(|d| d.project.as_str()).collect();
    let mut queue = VecDeque::new();
    let mut wanted = 0;
    for project in projects {
        let p = project_record(ctx, project).await?;
        let keep = if all { routes.deployments.keys().cloned().collect() } else { retained_production(&p) };
        let ids: Vec<String> = routes.deployments.keys().filter(|id| routes.deployments[*id].project == project && keep.contains(*id)).cloned().collect();
        wanted += ids.len();
        if !ids.is_empty() {
            queue.push_back((project.to_owned(), ids));
        }
    }
    log(&format!("{region}: sending the files of {wanted} deployments of {} projects", queue.len()));
    let started = Instant::now();
    let failed = publish_queue(ctx, region, queue, What::Files).await;
    log(&format!(
        "{region}: {} of {wanted} sent in {:.0} s; its router serves them, and deploys send their files there from now on. \
         Functions run there for the projects that pick it (`forkrail project NAME --regions`)",
        wanted - failed.len(),
        started.elapsed().as_secs_f64()
    ));
    if !failed.is_empty() {
        bail!("{} failed ({}); run `forkrail regions add {region}` again to retry them", failed.len(), failed.join(", "));
    }
    Ok(())
}

/// Publish just the deployments `ids` in `region`, functions and files, whatever regions their
/// project picks: a preview to try there, say.
pub async fn copy(ctx: &Ctx, region: &str, ids: &[String]) -> Result<()> {
    check_region(region)?;
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let mut by_project: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for id in ids {
        let d = routes.deployments.get(id).with_context(|| format!("deployment {id} isn't in the routing table"))?;
        by_project.entry(d.project.clone()).or_default().push(id.clone());
    }
    let failed = publish_queue(ctx, region, by_project.into_iter().collect(), What::Function).await;
    if !failed.is_empty() {
        bail!("{} failed: {}", failed.len(), failed.join(", "));
    }
    Ok(())
}

/// What `publish_queue` sends to a region.
#[derive(Clone, Copy)]
enum What {
    /// A deployment's files, for a router there.
    Files,
    /// Its function too.
    Function,
}

/// Send each project's deployments to `region`, six projects at once and each project's one after
/// another, since they share the function's `$LATEST`. Returns the ids that failed.
async fn publish_queue(ctx: &Ctx, region: &str, queue: VecDeque<(String, Vec<String>)>, what: What) -> Vec<String> {
    let (queue, failed) = (&Mutex::new(queue), &Mutex::new(Vec::new()));
    let worker = || async move {
        loop {
            let Some((project, ids)) = queue.lock().unwrap().pop_front() else { break };
            for id in ids {
                let began = Instant::now();
                let done = match what {
                    What::Files => copy_files(ctx, &id, region).await.map(|()| None),
                    What::Function => replicate(ctx, &id, region).await,
                };
                match done {
                    Ok(Some(version)) => {
                        log(&format!("{project} {id}: {region} version {version}, {:.0} s", began.elapsed().as_secs_f64()))
                    }
                    Ok(None) => log(&format!("{project} {id}: in {region}")),
                    Err(e) => {
                        eprintln!("error: {project} {id}: {e:#}");
                        failed.lock().unwrap().push(id);
                    }
                }
            }
        }
    };
    tokio::join!(worker(), worker(), worker(), worker(), worker(), worker());
    failed.lock().unwrap().clone()
}

/// Production and its rollback targets, as pruning keeps them.
fn retained_production(p: &state::Project) -> BTreeSet<String> {
    let mut keep: BTreeSet<String> = p.production.iter().cloned().collect();
    keep.extend(p.production_history.iter().take(KEEP_PRODUCTION).cloned());
    keep.extend(
        p.deployments.iter().filter(|d| d.target == state::Target::Production).take(KEEP_PRODUCTION).map(|d| d.id.clone()),
    );
    keep
}

/// No router runs in `region` any more: deploys stop sending their files there. Deployments of
/// projects that don't pick it as a function region stop running there (their versions there
/// date from when every deployment went to every router's region); the functions and versions
/// there are left as they are.
pub async fn remove(ctx: &Ctx, region: &str) -> Result<()> {
    check_region(region)?;
    ctx.store
        .update::<Settings, _>(SETTINGS_KEY, |current| {
            let mut s = current.context("no settings")?;
            s.regions.retain(|r| r != region);
            Ok(s)
        })
        .await?;
    let picked: BTreeSet<String> = crate::project::all(ctx).await?.into_iter().filter(|p| p.regions.iter().any(|r| r == region)).map(|p| p.name).collect();
    let mut removed = 0;
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            removed = 0;
            for d in routes.deployments.values_mut().filter(|d| !picked.contains(&d.project)) {
                removed += usize::from(d.replicas.remove(region).is_some());
            }
            Ok(routes)
        })
        .await?;
    log(&format!(
        "{region}: no router there from now on; {removed} deployments of projects that don't pick it stop running there, \
         and {} projects that pick it keep their functions there. The functions there are left as they are",
        picked.len()
    ));
    Ok(())
}

/// Every region with a router or functions: whether it has a router, how many deployments in the
/// routing table have a version there (and of them production), and how many projects pick it.
pub async fn list(ctx: &Ctx) -> Result<()> {
    let settings = ctx.store.settings().await?;
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let projects = crate::project::all(ctx).await?;
    let home = &ctx.install.home_region;
    let production: BTreeSet<&str> = routes
        .deployments
        .iter()
        .filter(|(id, d)| routes.hosts.get(&format!("{}.{}", d.project, settings.base_domain)) == Some(*id))
        .map(|(id, _)| id.as_str())
        .collect();
    let mut by_region: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    for (id, d) in &routes.deployments {
        for (region, _) in d.versions(home) {
            let counts = by_region.entry(region.to_owned()).or_default();
            counts.0 += 1;
            counts.1 += usize::from(production.contains(id.as_str()));
        }
    }
    for p in &projects {
        for region in function_regions(home, None, Some(p), &production_regions(home, &routes, p)) {
            by_region.entry(region).or_default().2 += 1;
        }
    }
    let routers = router_regions(home, &settings);
    let mut regions = routers.clone();
    regions.extend(by_region.keys().filter(|r| !routers.contains(r)).cloned());
    println!("{:<16} {:<7} {:>12} {:>11} {:>9}", "REGION", "ROUTER", "DEPLOYMENTS", "PRODUCTION", "PROJECTS");
    let total = (routes.deployments.len(), production.len());
    for region in &regions {
        let (all, prod, picked) = by_region.get(region).copied().unwrap_or_default();
        let router = if region == home { "home" } else if routers.contains(region) { "yes" } else { "-" };
        let place = function_region(region).map(|f| format!("  {}, {}", f.city, f.country)).unwrap_or_default();
        println!("{region:<16} {router:<7} {:>12} {:>11} {picked:>9}{place}", format!("{all}/{}", total.0), format!("{prod}/{}", total.1));
    }
    println!("\nProjects run their functions where their production runs ({home} for a new one) unless they pick regions (`forkrail project NAME --regions fra1,iad1`, or vercel.json's `regions`).");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_region_names() {
        assert_eq!(parse(&["iad1", "fra1", "us-east-1", "AP-SOUTH-1"]).unwrap(), ["us-east-1", "eu-central-1", "ap-south-1"]);
        assert!(parse::<&str>(&[]).unwrap_err().to_string().contains("at least one"));
        let err = parse(&["iad1", "mars1"]).unwrap_err().to_string();
        assert!(err.contains("\"mars1\" isn't a region") && err.contains("eu-central-1 (fra1)"), "{err}");
    }

    #[test]
    fn where_functions_run() {
        let home = "us-east-1";
        let project = |regions: &[&str]| Project { regions: regions.iter().map(|r| r.to_string()).collect(), ..Default::default() };
        let none: &[String] = &[];
        let both = ["us-east-1".to_owned(), "us-west-1".to_owned()];
        // A new project, nothing set: the home region alone.
        assert_eq!(function_regions(home, None, None, none), ["us-east-1"]);
        assert_eq!(function_regions(home, None, Some(&project(&[])), none), ["us-east-1"]);
        // Nothing set, production running somewhere: it stays there (the projects from before
        // function regions keep every router's region).
        assert_eq!(function_regions(home, None, Some(&project(&[])), &both), ["us-east-1", "us-west-1"]);
        assert_eq!(function_regions(home, None, Some(&project(&[])), &["eu-central-1".to_owned()]), ["eu-central-1"]);
        // The project's, home first when it's one of them, whatever production runs.
        assert_eq!(function_regions(home, None, Some(&project(&["eu-central-1", "us-east-1"])), &both), ["us-east-1", "eu-central-1"]);
        assert_eq!(function_regions(home, None, Some(&project(&["ap-south-1", "eu-central-1"])), none), ["ap-south-1", "eu-central-1"]);
        assert_eq!(function_regions(home, None, Some(&project(&["us-east-1"])), &both), ["us-east-1"]);
        // vercel.json's win over the project's.
        let pinned = vec!["us-west-1".to_owned()];
        assert_eq!(function_regions(home, Some(&pinned), Some(&project(&["eu-central-1"])), &both), ["us-west-1"]);
        assert_eq!(function_regions(home, Some(&[]), Some(&project(&["eu-central-1"])), &both), ["eu-central-1"]);
    }

    #[test]
    fn regions_production_runs_in() {
        let d = |region: Option<&str>, replicas: &[&str]| forkrail_common::routes::Deployment {
            project: "api".into(),
            function: "forkrail-app-api".into(),
            qualifier: "3".into(),
            region: region.map(str::to_owned),
            replicas: replicas.iter().map(|r| ((*r).to_owned(), "1".to_owned())).collect(),
            ..serde_json::from_value(serde_json::json!({"project": "api"})).unwrap()
        };
        // Its own region first (home for an entry from before regions were recorded), then the rest.
        assert_eq!(running_in("us-east-1", &d(None, &["us-west-1"])), ["us-east-1", "us-west-1"]);
        assert_eq!(running_in("us-east-1", &d(Some("us-east-1"), &["us-west-1"])), ["us-east-1", "us-west-1"]);
        assert_eq!(running_in("us-east-1", &d(Some("us-east-1"), &[])), ["us-east-1"]);
        assert_eq!(running_in("us-east-1", &d(Some("eu-central-1"), &[])), ["eu-central-1"]);
        // Static files alone: no function to place.
        let site = forkrail_common::routes::Deployment { function: String::new(), qualifier: String::new(), ..d(None, &["us-west-1"]) };
        assert!(running_in("us-east-1", &site).is_empty());
        // A project's production, by the routing table.
        let mut routes = Routes::default();
        routes.deployments.insert("a1b2c3d4".into(), d(None, &["us-west-1"]));
        let p = |production: Option<&str>| Project { production: production.map(str::to_owned), ..Default::default() };
        assert_eq!(production_regions("us-east-1", &routes, &p(Some("a1b2c3d4"))), ["us-east-1", "us-west-1"]);
        assert!(production_regions("us-east-1", &routes, &p(None)).is_empty());
        assert!(production_regions("us-east-1", &routes, &p(Some("gone0000"))).is_empty());
    }

    #[test]
    fn routers_and_descriptions() {
        let settings = Settings { regions: vec!["us-west-1".into(), "us-east-1".into()], ..Default::default() };
        assert_eq!(router_regions("us-east-1", &settings), ["us-east-1", "us-west-1"]);
        assert_eq!(describe(&["us-east-1".into(), "eu-central-1".into()]), "us-east-1 (iad1, Washington, D.C.), eu-central-1 (fra1, Frankfurt)");
    }

    #[test]
    fn region_names() {
        for good in ["us-west-1", "eu-central-1", "ap-southeast-2", "us-gov-west-1"] {
            assert!(check_region(good).is_ok(), "{good}");
        }
        for bad in ["", "us", "us-west", "US-west-1", "us-west-x", "us--1", "usa-west-1", "us-west-1/"] {
            assert!(check_region(bad).is_err(), "{bad}");
        }
    }
}
