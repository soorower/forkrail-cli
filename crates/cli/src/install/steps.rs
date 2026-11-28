//! The install's steps, in order. Each is skipped when the record says it's done, and each leaves
//! in the record what later steps need. Every AWS call goes through [`Cx::call`]: tried twice,
//! then a [`Problem`].

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Instant;

use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration, Tag as S3Tag, Tagging};
use aws_sdk_ssm::types::{ParameterTier, ParameterType, Tag as SsmTag};
use forkrail_common::install::{CHALLENGE_KEY_PARAM, CLOUDFLARE_TOKEN_PARAM, PEER_KEY_PARAM, TAG_KEY, TAG_VALUE};

use super::boxes;
use super::cloudflare::Cloudflare;
use super::problem::{Problem, aws_text, twice};
use super::release::{self as rel, Release};
use super::record::{RECORD_KEY, Record};
use super::{Cx, Event, now_ms, stacks};

/// Every step, by id and title, in the order they run.
pub const ALL: &[(&str, &str)] = &[
    ("bucket", "Storage for your apps"),
    ("keys", "Encryption keys"),
    ("roles", "Access rules"),
    ("secrets", "Your Cloudflare token, stored safely"),
    ("release", "Copying forkrail into your account"),
    ("services", "Builds, deploys and alerts"),
    ("analytics", "The log server"),
    ("router", "The router, your apps' front door"),
    ("domain", "Your domain and its certificate"),
    ("dashboard", "Your dashboard"),
    ("protection", "Spending limit and alerts"),
    ("github", "GitHub sign-in"),
];

/// What a step does, in a sentence or two for the person watching.
pub fn about(id: &str) -> &'static str {
    match id {
        "bucket" => "A private place in your account for your apps' files, builds and logs.",
        "keys" => "Keys that lock your secrets and environment variables, so only forkrail can read them.",
        "roles" => "Each part of forkrail gets its own access rules, limited to what that part needs.",
        "secrets" => "Your Cloudflare token is saved encrypted. Only the router can read it.",
        "release" => "Your account downloads forkrail's programs and app-building tools (about 1 GB) straight from its public release, so none of it goes through your internet connection.",
        "services" => "Sets up the services that build and deploy your apps, run scheduled jobs and send you alerts.",
        "analytics" => "Starts a small server that keeps your apps' logs and visitor stats.",
        "router" => "Starts the server that receives every visit to your apps and passes it to the right one.",
        "domain" => "Points your domain at the router and gets a free HTTPS certificate for it.",
        "dashboard" => "Puts your dashboard online at dashboard.<your domain>.",
        "protection" => "Sets a monthly spending limit. You get an email at 80% and at 100%, and apps pause at the limit.",
        "github" => "Saves the GitHub App you created, so you can sign in to your dashboard with GitHub.",
        _ => "",
    }
}

/// About how long a step takes, in seconds: for the page's progress and time left.
pub fn estimate(id: &str) -> u64 {
    match id {
        "bucket" | "secrets" | "protection" | "github" => 10,
        "keys" => 60,
        "roles" => 180,
        // The account's own download of the release (about 1 GB) and its copy into the registry.
        "release" => 180,
        // About ten stacks, side by side: the slowest takes about two minutes.
        "services" => 180,
        "analytics" | "router" => 300,
        "domain" => 120,
        "dashboard" => 180,
        _ => 60,
    }
}

/// The steps a step needs finished before it starts. Every step whose needs are finished runs at
/// once: the log server is set up while the release is copied and the services are made.
pub fn needs(id: &str) -> &'static [&'static str] {
    match id {
        "secrets" => &["keys"],
        "release" => &["bucket"],
        "services" => &["release", "roles", "secrets"],
        "analytics" => &["bucket", "roles", "secrets"],
        "router" => &["services", "analytics"],
        "domain" => &["router"],
        "dashboard" => &["domain"],
        "protection" => &["services"],
        "github" => &["dashboard"],
        _ => &[],
    }
}

pub const INSTALL_KEY_ALIAS: &str = "alias/forkrail-install";
/// The ClickHouse passwords (deploy/aws/analytics.sh): the routers' Vector, the dashboard, build logs.
pub const CLICKHOUSE_PARAMS: [&str; 3] = ["/forkrail/clickhouse/ingest", "/forkrail/clickhouse/reader", "/forkrail/clickhouse/buildlog"];

type Outputs = BTreeMap<String, String>;

fn title(id: &str) -> &'static str {
    ALL.iter().find(|(i, _)| *i == id).map_or("A step", |(_, t)| t)
}

impl Cx {
    /// One AWS call for step `step`: tried twice, then a [`Problem`].
    pub async fn call<T, E, F, Fut>(&self, step: &str, action: &str, call: F) -> Result<T, Problem>
    where
        E: std::fmt::Display,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        twice(title(step), action, self.region(), call).await
    }

    /// Applies stack forkrail-`name` for `step`, its functions' memory fitted to what the account
    /// allows. A refusal that names a lower limit is remembered, so the second try fits it.
    pub(super) async fn stack(&self, step: &str, name: &str, parameters: &[(&str, String)]) -> Result<stacks::Outputs, Problem> {
        let config = self.config.clone();
        let action = format!("setting up {} (stack forkrail-{name})", stacks::purpose(name));
        self.call(step, &action, || {
            let fitted = fit_memory(name, parameters, self.memory_cap.get());
            let config = config.clone();
            async move {
                let fitted: Vec<(&str, String)> = fitted.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                let result = stacks::apply(&config, name, &fitted).await;
                if let Err(said) = &result
                    && let Some(cap) = super::checks::memory_limit(said)
                {
                    self.memory_cap.set(Some(cap));
                    self.report.note(format!("     This new AWS account lets each function use up to {cap} MB for now, so forkrail's functions are made to fit."));
                }
                result
            }
        })
        .await
    }

    /// Writes the record to the bucket, once there is one.
    async fn save(&self) -> Result<(), Problem> {
        if !self.record.done("bucket") {
            return Ok(());
        }
        let body = serde_json::to_vec_pretty(&self.record).expect("a record serializes");
        let s3 = aws_sdk_s3::Client::new(&self.config);
        let bucket = self.bucket();
        self.call("bucket", "saving the installer's record", || {
            s3.put_object().bucket(&bucket).key(RECORD_KEY).content_type("application/json").body(body.clone().into()).send()
        })
        .await
        .map(drop)
    }
}

/// Runs the steps that aren't done, each as soon as what it [`needs`] is, and saves the record as
/// each finishes. A failure is reported at once, and no step starts after it; the steps already
/// running finish (and are recorded), so the next run carries on from there.
pub async fn run(cx: &mut Cx) -> Result<(), Problem> {
    use futures_util::StreamExt as _;
    cx.report.send(Event::Plan(ALL.to_vec()));
    // A run that continues with a newer release (a newer folder, or a newer CLI's download) copies
    // it again before the steps that use it: a stopped install takes a fixed release, never the
    // one it first copied.
    if cx.record.done("release") && COPIED_FOR.iter().any(|s| !cx.record.done(s)) {
        let newer = match &cx.release {
            Some(dir) => Release::open(dir).ok().filter(|r| release_again(&cx.record, &r.manifest.commit)).map(|r| format!("This release ({})", short(&r.manifest.commit))),
            // Offline, the copied one does.
            None => match super::download::published().await {
                Ok((_, sha256)) if published_again(&cx.record, &sha256) => Some(format!("forkrail {}'s published release", super::download::version())),
                _ => None,
            },
        };
        if let Some(newer) = newer {
            cx.report.note(format!("     {newer} is newer than the one copied before, so it's copied again."));
            cx.record.steps.remove("release");
        }
    }
    for &(id, _) in ALL {
        if cx.record.done(id) {
            cx.report.send(Event::Finished { id, skipped: true, secs: 0 });
        }
    }

    let mut running = futures_util::stream::FuturesUnordered::new();
    let mut started: Vec<&str> = Vec::new();
    let mut failed: Option<Problem> = None;
    loop {
        if failed.is_none() {
            for &(id, _) in ALL {
                let ready = !cx.record.done(id)
                    && !started.contains(&id)
                    && needs(id).iter().all(|n| cx.record.done(n));
                if !ready {
                    continue;
                }
                started.push(id);
                cx.report.send(Event::Started(id));
                // The record as it is now: what this step needs is in it.
                let mut step = cx.clone();
                step.report = cx.report.for_step(id);
                running.push(async move {
                    let at = Instant::now();
                    let result = one(&step, id).await;
                    (id, at.elapsed().as_secs(), result)
                });
            }
        }
        tokio::select! {
            Some((id, secs, result)) = running.next() => match result {
                Ok(outputs) => {
                    cx.record.memory_cap_mb = cx.memory_cap.get();
                    cx.record.finish(id, now_ms(), outputs);
                    match cx.save().await {
                        Ok(()) => cx.report.send(Event::Finished { id, skipped: false, secs }),
                        Err(problem) => stop(cx, &mut failed, problem),
                    }
                }
                Err(problem) => stop(cx, &mut failed, problem),
            },
            else => break,
        }
    }
    if let Some(problem) = failed {
        return Err(problem);
    }
    if let Some(&(id, _)) = ALL.iter().find(|(id, _)| !cx.record.done(id)) {
        // Only a list of needs that goes round in a circle leaves a step waiting.
        let problem = problem(id, "waiting for the steps before it", "a step never became ready", "The installer's own order of steps is broken.", "Open an issue: this is a bug in forkrail.");
        cx.report.send(Event::Failed(problem.clone()));
        return Err(problem);
    }
    cx.record.finished_ms = Some(now_ms());
    cx.save().await.inspect_err(|problem| cx.report.send(Event::Failed(problem.clone())))
}

/// The first failure: shown at once, while the steps already running finish.
fn stop(cx: &Cx, failed: &mut Option<Problem>, problem: Problem) {
    if failed.is_none() {
        cx.report.send(Event::Failed(problem.clone()));
        *failed = Some(problem);
    }
}

/// Step `id`'s work.
async fn one(cx: &Cx, id: &str) -> Result<Outputs, Problem> {
    match id {
        "bucket" => bucket(cx).await,
        "keys" => keys(cx).await,
        "roles" => roles(cx).await,
        "secrets" => secrets(cx).await,
        "release" => release(cx).await,
        "services" => services(cx).await,
        "analytics" => analytics(cx).await,
        "router" => router(cx).await,
        "domain" => domain(cx).await,
        "dashboard" => dashboard(cx).await,
        "protection" => protection(cx).await,
        "github" => github(cx).await,
        other => unreachable!("no step {other}"),
    }
}

/// The steps that take what the release step copied: until they've all run, a newer release is
/// copied again.
const COPIED_FOR: [&str; 3] = ["services", "analytics", "router"];

/// The install's bucket, `forkrail-artifacts-<account>`, in the home region: state, packages,
/// static files and logs. Private (S3's default for a new bucket), encrypted at rest (also its
/// default), tagged.
async fn bucket(cx: &Cx) -> Result<Outputs, Problem> {
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let bucket = cx.bucket();
    let region = cx.region().to_owned();
    let created = cx
        .call("bucket", &format!("creating bucket {bucket}"), || {
            let mut create = s3.create_bucket().bucket(&bucket);
            if region != "us-east-1" {
                create = create.create_bucket_configuration(CreateBucketConfiguration::builder().location_constraint(BucketLocationConstraint::from(region.as_str())).build());
            }
            async move {
                match create.send().await {
                    Ok(_) => Ok(()),
                    // Made by an earlier run that stopped before recording it.
                    Err(e) if aws_text(&e).contains("BucketAlreadyOwnedByYou") => Ok(()),
                    Err(e) => Err(aws_text(&e)),
                }
            }
        })
        .await;
    created?;
    let tagging = Tagging::builder().tag_set(S3Tag::builder().key(TAG_KEY).value(TAG_VALUE).build().expect("tag")).build().expect("tagging");
    cx.call("bucket", "tagging the bucket", || s3.put_bucket_tagging().bucket(&bucket).tagging(tagging.clone()).send().map_err_text()).await?;
    Ok(Outputs::from([("name".to_owned(), bucket)]))
}

/// The keys env var values and the install's own secrets are encrypted with
/// (deploy/install/secrets.yaml, install-secrets.yaml).
async fn keys(cx: &Cx) -> Result<Outputs, Problem> {
    let mut outputs = Outputs::new();
    let (env, install) = tokio::join!(cx.stack("keys", "secrets", &[]), cx.stack("keys", "install-secrets", &[]));
    let (env, install) = (env?, install?);
    outputs.extend(env.get("KeyArn").map(|k| ("env_key".to_owned(), k.clone())));
    outputs.extend(install.get("KeyArn").map(|k| ("install_key".to_owned(), k.clone())));
    Ok(outputs)
}

/// The routers', the analytics box's, the dashboard's and workers' roles
/// (deploy/install/install-roles.yaml).
async fn roles(cx: &Cx) -> Result<Outputs, Problem> {
    cx.stack("roles", "install-roles", &[]).await
}

/// The Cloudflare token, the routers' challenge and peer keys, and the ClickHouse passwords, as
/// SecureStrings under the install key. Made once; the token is replaced when a new one is given.
async fn secrets(cx: &Cx) -> Result<Outputs, Problem> {
    let ssm = aws_sdk_ssm::Client::new(&cx.config);
    if let Some(token) = &cx.token {
        put_secret(cx, &ssm, CLOUDFLARE_TOKEN_PARAM, token.trim(), true).await?;
    }
    for name in [CHALLENGE_KEY_PARAM, PEER_KEY_PARAM] {
        put_secret(cx, &ssm, name, &forkrail_common::random_hex(32), false).await?;
    }
    for name in CLICKHOUSE_PARAMS {
        put_secret(cx, &ssm, name, &forkrail_common::random_hex(24), false).await?;
    }
    Ok(Outputs::new())
}

/// A new alert email for an install whose alerts topic is made already: subscribed to it, and AWS
/// mails it to confirm. The earlier address stays subscribed until it unsubscribes.
pub async fn subscribe_alerts(cx: &Cx, email: &str) -> Result<(), Problem> {
    let Some(topic) = cx.record.output("services", "alerts_topic").map(str::to_owned) else { return Ok(()) };
    let sns = aws_sdk_sns::Client::new(&cx.config);
    cx.call("services", "subscribing your email to alerts", || sns.subscribe().topic_arn(&topic).protocol("email").endpoint(email).send().map_err_text()).await?;
    cx.report.note(format!("AWS sent {email} a message to confirm alerts: click its link."));
    Ok(())
}

/// One SecureString under the install key: made with `value` if missing; replaced only when
/// `replace` (a new value for something given, never a key others already hold).
async fn put_secret(cx: &Cx, ssm: &aws_sdk_ssm::Client, name: &str, value: &str, replace: bool) -> Result<(), Problem> {
    let tag = SsmTag::builder().key(TAG_KEY).value(TAG_VALUE).build().expect("tag");
    cx.call("secrets", &format!("storing {name}"), || {
        let put = ssm.put_parameter().name(name).value(value).r#type(ParameterType::SecureString).key_id(INSTALL_KEY_ALIAS).tier(ParameterTier::Standard);
        let tag = tag.clone();
        async move {
            match put.clone().tags(tag).send().await {
                Ok(_) => Ok(()),
                Err(e) if aws_text(&e).contains("ParameterAlreadyExists") => {
                    if replace {
                        put.overwrite(true).send().await.map(drop).map_err(|e| aws_text(&e))
                    } else {
                        Ok(())
                    }
                }
                Err(e) => Err(aws_text(&e)),
            }
        }
    })
    .await
}

/// An SDK call's error as its full text, for [`Cx::call`].
pub(super) trait MapErrText<T> {
    fn map_err_text(self) -> impl Future<Output = Result<T, String>>;
}

impl<T, E, F> MapErrText<T> for F
where
    F: Future<Output = Result<T, E>>,
    E: std::error::Error,
{
    fn map_err_text(self) -> impl Future<Output = Result<T, String>> {
        async move { self.await.map_err(|e| aws_text(&e)) }
    }
}

fn problem(step: &str, action: &str, said: impl Into<String>, cause: &str, fix: &str) -> Problem {
    Problem { step: title(step).to_owned(), action: action.to_owned(), said: said.into(), cause: cause.to_owned(), fix: fix.to_owned(), link: None }
}

fn open_release(cx: &Cx, step: &str) -> Result<Release, Problem> {
    let Some(dir) = &cx.release else {
        return Err(problem(step, "finding forkrail's release", "no release folder", "forkrail's release wasn't downloaded.", "Run `forkrail install` again."));
    };
    Release::open(dir).map_err(|e| problem(step, "reading the release folder", format!("{e:#}"), "The release folder is incomplete.", "Make it again with deploy/aws/bundle.sh DIR."))
}

/// The registry, then the release's files: the router's binary (as `forkrail-router`, which a
/// router downloads at every start), the functions' zips under their content's keys, Vector, and
/// the builder's image. A release folder (`--release`, FORKRAIL_RELEASE) is uploaded from here;
/// otherwise the account copies this version's published release itself ([`super::copier`]), and
/// only when it can't is the release downloaded here and uploaded.
async fn release(cx: &Cx) -> Result<Outputs, Problem> {
    let registry = cx.stack("release", "registry", &[]).await?;
    let repository_uri = registry.get("RepositoryUri").cloned().unwrap_or_else(|| format!("{}.dkr.ecr.{}.amazonaws.com/forkrail-builder", cx.account(), cx.region()));
    if cx.release.is_some() {
        return upload_folder(cx, &open_release(cx, "release")?, &repository_uri).await;
    }
    let version = super::download::version();
    let (url, sha256) = super::download::published().await.map_err(|e| {
        problem(
            "release",
            &format!("finding forkrail {version}'s release"),
            format!("{e:#}"),
            "forkrail's release couldn't be found.",
            "Check this computer's internet connection, then run `forkrail install` again. It carries on from here.",
        )
    })?;
    let mut outputs = match super::copier::copy(cx, &url, version, &sha256).await {
        Ok(copied) => from_copy(cx, &copied, &repository_uri).await?,
        Err(why) => {
            cx.report.note(format!("     Your account couldn't download forkrail itself ({why}), so this computer downloads it and copies it up instead."));
            let dir = super::download::fetch(&cx.report, &url, &sha256).await.map_err(|e| {
                problem(
                    "release",
                    &format!("downloading forkrail {version}"),
                    format!("{e:#}"),
                    "forkrail's release couldn't be downloaded.",
                    "Check this computer's internet connection, then run `forkrail install` again. It carries on from here.",
                )
            })?;
            let release = Release::open(&dir).map_err(|e| problem("release", "reading the downloaded release", format!("{e:#}"), "The download is incomplete.", "Run `forkrail install` again."))?;
            upload_folder(cx, &release, &repository_uri).await?
        }
    };
    outputs.insert("release_sha256".into(), sha256);
    Ok(outputs)
}

/// What the account copied, where the steps after take it from: the files go from where the copy
/// put them to their keys (named by content, as an upload names them), in the bucket itself.
async fn from_copy(cx: &Cx, copied: &super::copier::Copied, repository_uri: &str) -> Result<Outputs, Problem> {
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let bucket = cx.bucket();
    let broken = |what: String| problem("release", "reading the copied release", what, "forkrail's published release is incomplete.", "Open an issue: this is a bug in forkrail's release.");
    let manifest_key = copied.key(rel::MANIFEST);
    let body = cx
        .call("release", "reading the release's manifest", || {
            let s3 = s3.clone();
            let (bucket, key) = (bucket.clone(), manifest_key.clone());
            async move {
                let out = s3.get_object().bucket(&bucket).key(&key).send().await.map_err(|e| aws_text(&e))?;
                out.body.collect().await.map(|b| b.into_bytes()).map_err(|e| e.to_string())
            }
        })
        .await?;
    let manifest: rel::Manifest = serde_json::from_slice(&body).map_err(|e| broken(format!("{}: {e}", rel::MANIFEST)))?;
    let sha = |file: &str| copied.files.get(file).map(|f| f.sha256.clone()).ok_or_else(|| broken(format!("it has no {file}")));
    let web_built = manifest.web_built.clone().ok_or_else(|| broken("it has no built dashboard (web_built)".into()))?;
    sha(&web_built)?;

    let mut outputs = Outputs::new();
    outputs.insert("version".into(), manifest.version.clone());
    outputs.insert("commit".into(), manifest.commit.clone());
    let router_sha = sha(&manifest.router)?;
    let mut copies = vec![(manifest.router.clone(), "forkrail-router".to_owned(), Some(router_sha.clone()))];
    for (name, file) in [("control", &manifest.control), ("notify", &manifest.notify), ("guard", &manifest.guard), ("image", &manifest.image), ("screenshot", &manifest.screenshot)] {
        let key = rel::zip_key(name, &sha(file)?);
        outputs.insert(format!("{name}_key"), key.clone());
        copies.push((file.clone(), key, None));
    }
    let vector_key = format!("tools/{}", manifest.vector);
    outputs.insert("vector_sha256".into(), sha(&manifest.vector)?);
    copies.push((manifest.vector.clone(), vector_key.clone(), None));
    outputs.insert("vector_key".into(), vector_key);

    futures_util::future::try_join_all(copies.iter().map(|(file, key, router_sha)| {
        let (s3, bucket, from) = (&s3, &bucket, copied.key(file));
        let action = format!("putting {file} in place");
        async move {
            cx.call("release", &action, || {
                let from = from.clone();
                async move {
            let current = s3.head_object().bucket(bucket).key(key).send().await.ok();
            match router_sha {
                // The router's own key isn't named by content: replaced when it differs.
                Some(sha) => {
                    if current.and_then(|h| h.metadata().and_then(|m| m.get("sha256").cloned())).as_deref() == Some(sha.as_str()) {
                        return Ok(());
                    }
                }
                None if current.is_some() => return Ok(()),
                None => {}
            }
            let mut copy = s3.copy_object().bucket(bucket).key(key).copy_source(format!("{bucket}/{from}"));
            if let Some(sha) = router_sha {
                copy = copy.metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace).metadata("sha256", sha);
            }
            copy.send().await.map(drop).map_err(|e| aws_text(&e))
                }
            })
            .await
        }
    }))
    .await?;
    // Only the dashboard's build stays where the copy put it, for the dashboard step.
    for (file, _, _) in &copies {
        let _ = s3.delete_object().bucket(&bucket).key(copied.key(file)).send().await;
    }
    outputs.insert("router_sha256".into(), router_sha);
    outputs.insert("web_built_key".into(), copied.key(&web_built));
    outputs.insert("builder_image".into(), format!("{repository_uri}@{}", copied.image_digest));
    Ok(outputs)
}

/// The release folder `release`, uploaded from here, everything side by side: the files to the
/// bucket, the builder's image to ECR.
async fn upload_folder(cx: &Cx, release: &Release, repository_uri: &str) -> Result<Outputs, Problem> {
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let bucket = cx.bucket();
    let mut outputs = Outputs::new();
    outputs.insert("version".into(), release.manifest.version.clone());
    outputs.insert("commit".into(), release.manifest.commit.clone());

    let router = release.path(&release.manifest.router);
    let router_sha = rel::sha256_file(&router).map_err(|e| problem("release", "reading the router's binary", format!("{e:#}"), "The release folder is incomplete.", "Make it again."))?;
    let router_up = cx.call("release", "uploading the router's binary", || {
        let s3 = s3.clone();
        let (bucket, router, sha) = (bucket.clone(), router.clone(), router_sha.clone());
        async move {
            // The router's own key isn't named by content: replaced when it differs.
            let current = s3.head_object().bucket(&bucket).key("forkrail-router").send().await.ok().and_then(|h| h.metadata().and_then(|m| m.get("sha256").cloned()));
            if current.as_deref() == Some(sha.as_str()) {
                return Ok(());
            }
            let body = aws_sdk_s3::primitives::ByteStream::from_path(&router).await.map_err(|e| e.to_string())?;
            s3.put_object().bucket(&bucket).key("forkrail-router").metadata("sha256", &sha).body(body).send().await.map(drop).map_err(|e| aws_text(&e))
        }
    });

    let mut zips = Vec::new();
    for (name, file) in [
        ("control", &release.manifest.control),
        ("notify", &release.manifest.notify),
        ("guard", &release.manifest.guard),
        ("image", &release.manifest.image),
        ("screenshot", &release.manifest.screenshot),
    ] {
        let path = release.path(file);
        let sha = rel::sha256_file(&path).map_err(|e| problem("release", &format!("reading {file}"), format!("{e:#}"), "The release folder is incomplete.", "Make it again."))?;
        zips.push((name, file.clone(), path, rel::zip_key(name, &sha)));
    }
    let zips_up = futures_util::future::try_join_all(zips.iter().map(|(_, file, path, key)| {
        let (s3, bucket) = (&s3, &bucket);
        async move { cx.call("release", &format!("uploading {file}"), || rel::upload(s3, bucket, key, path, &[])).await }
    }));

    let vector = release.path(&release.manifest.vector);
    let vector_key = format!("tools/{}", release.manifest.vector);
    let vector_sha = rel::sha256_file(&vector).map_err(|e| problem("release", "reading Vector", format!("{e:#}"), "The release folder is incomplete.", "Make it again."))?;
    let vector_up = cx.call("release", "uploading Vector", || rel::upload(&s3, &bucket, &vector_key, &vector, &[]));

    let ecr = aws_sdk_ecr::Client::new(&cx.config);
    let layout = release.path(&release.manifest.builder);
    let tag = format!("install-{}", release.manifest.version);
    let report = cx.report.clone();
    let progress = move |line: String| report.note(format!("     {line}"));
    let image_up = cx.call("release", "copying the builder's image", || rel::push_image(&ecr, "forkrail-builder", &layout, &tag, &progress));

    let (_, _, _, digest) = tokio::try_join!(router_up, zips_up, vector_up, image_up)?;
    outputs.insert("router_sha256".into(), router_sha);
    for (name, _, _, key) in zips {
        outputs.insert(format!("{name}_key"), key);
    }
    outputs.insert("vector_key".into(), vector_key);
    outputs.insert("vector_sha256".into(), vector_sha);
    outputs.insert("builder_image".into(), format!("{repository_uri}@{digest}"));
    Ok(outputs)
}

/// The install's administrators, for the tables' and the schedules queue's policies: the
/// account's root user and whoever runs the install (an IAM user, or the role an SSO or
/// assumed-role session acts as, path and all). In those policies' conditions
/// (`aws:PrincipalArn`) the root's ARN is the root user alone, not the whole account as it is in a
/// `Principal`: an administrator signed in through a role would be refused the tables, and the
/// install's own deploy of the dashboard with them.
async fn admins(cx: &Cx) -> String {
    let (partition, account) = (&cx.caller.partition, cx.account());
    let root = format!("arn:{partition}:iam::{account}:root");
    let iam = aws_sdk_iam::Client::new(&cx.config);
    match super::checks::principal(&iam, &cx.caller.arn).await {
        Ok(Some(arn)) => format!("{root},{arn}"),
        Ok(None) => root,
        // The role couldn't be looked up: its name, at any path.
        Err(_) => match cx.caller.arn.split(":assumed-role/").nth(1).and_then(|rest| rest.split('/').next()) {
            Some(role) => format!("{root},arn:{partition}:iam::{account}:role/{role},arn:{partition}:iam::{account}:role/*/{role}"),
            None => format!("{root},{}", cx.caller.arn),
        },
    }
}

pub const ALERTS_TOPIC: &str = "forkrail-alerts";

/// Optimized images are deleted this many days after they're made (deploy/aws/image.sh).
const IMAGE_DAYS: i32 = 31;

/// The bucket's rule that deletes optimized images (`images/`) after [`IMAGE_DAYS`], as image.sh
/// sets it. A bucket's lifecycle rules are replaced together, so any others are read and kept.
async fn images_expire(cx: &Cx, bucket: &str) -> Result<(), Problem> {
    use aws_sdk_s3::types::{BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration, LifecycleRule, LifecycleRuleFilter};
    const ID: &str = "forkrail-optimized-images";
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let current = cx
        .call("services", "reading the bucket's lifecycle rules", || {
            let s3 = s3.clone();
            async move {
                match s3.get_bucket_lifecycle_configuration().bucket(bucket).send().await {
                    Ok(out) => Ok(out.rules().to_vec()),
                    Err(e) if aws_text(&e).contains("NoSuchLifecycleConfiguration") => Ok(Vec::new()),
                    Err(e) => Err(aws_text(&e)),
                }
            }
        })
        .await?;
    let mut rules: Vec<LifecycleRule> = current.into_iter().filter(|r| r.id() != Some(ID)).collect();
    rules.push(
        LifecycleRule::builder()
            .id(ID)
            .status(ExpirationStatus::Enabled)
            .filter(LifecycleRuleFilter::builder().prefix("images/").build())
            .expiration(LifecycleExpiration::builder().days(IMAGE_DAYS).build())
            .build()
            .expect("a rule"),
    );
    let config = BucketLifecycleConfiguration::builder().set_rules(Some(rules)).build().expect("rules");
    cx.call("services", "deleting optimized images after a month", || s3.put_bucket_lifecycle_configuration().bucket(bucket).lifecycle_configuration(config.clone()).send().map_err_text()).await?;
    Ok(())
}

/// forkrail's own functions and their tables, queues and schedules, as stacks, in the order they
/// name each other: control and the notifier make roles the tables' policies name.
async fn services(cx: &Cx) -> Result<Outputs, Problem> {
    let key = |name: &str| cx.record.output("release", &format!("{name}_key")).unwrap_or_default().to_owned();
    let mut outputs = Outputs::new();

    // The alarms' topic, and the person's email on it (they confirm the subscription once).
    let sns = aws_sdk_sns::Client::new(&cx.config);
    let topic = cx
        .call("services", "creating the alerts topic", || {
            sns.create_topic()
                .name(ALERTS_TOPIC)
                .tags(aws_sdk_sns::types::Tag::builder().key(TAG_KEY).value(TAG_VALUE).build().expect("tag"))
                .send()
                .map_err_text()
        })
        .await?
        .topic_arn()
        .unwrap_or_default()
        .to_owned();
    if let Some(email) = &cx.record.email {
        cx.call("services", "subscribing your email to alerts", || sns.subscribe().topic_arn(&topic).protocol("email").endpoint(email).send().map_err_text()).await?;
        cx.report.note(format!("     AWS sent {email} a message to confirm alerts: click its link."));
    }
    outputs.insert("alerts_topic".into(), topic);

    let reserve = if cx.concurrency.unwrap_or(0) >= 100 { "1" } else { "0" };
    let bucket = cx.bucket();
    let admins = admins(cx).await;
    let control_parameters = [("CodeKey", key("control")), ("ReservedConcurrency", "0".into())];
    let builder_parameters = [("ImageUri", cx.record.output("release", "builder_image").unwrap_or_default().to_owned())];
    let notify_parameters = [("CodeKey", key("notify")), ("DashboardUrl", cx.record.dashboard_url()), ("ReservedConcurrency", "0".into())];
    let admin_parameters = [("AdminPrincipals", admins)];
    let image_parameters = [("CodeKey", key("image")), ("Bucket", bucket.clone()), ("StoreBucket", bucket.clone()), ("StoreRegion", cx.region().to_owned())];
    let screenshot_parameters = [("CodeKey", key("screenshot"))];
    let guard_parameters = [("CodeKey", key("guard")), ("ReservedConcurrency", reserve.into())];
    // The stacks side by side: they name each other's roles and tables only in policies (which
    // don't need them to exist yet), except that the builder lets its topic call control, and
    // retention's schedule calls it, so those two wait for control.
    let control_first = async {
        let control = cx.stack("services", "control", &control_parameters).await?;
        let (retention, builder) = tokio::join!(cx.stack("services", "retention", &[]), cx.stack("services", "builder", &builder_parameters));
        retention?;
        builder?;
        Ok::<_, Problem>(control)
    };
    let optimizer = async {
        let optimizer = cx.stack("services", "image", &image_parameters).await?;
        images_expire(cx, &bucket).await?;
        Ok::<_, Problem>(optimizer)
    };
    let (control, notify, state, web, schedules, optimizer, screenshot, guard) = tokio::join!(
        control_first,
        cx.stack("services", "notify", &notify_parameters),
        cx.stack("services", "state", &admin_parameters),
        cx.stack("services", "web", &admin_parameters),
        cx.stack("services", "schedules", &admin_parameters),
        optimizer,
        cx.stack("services", "screenshot", &screenshot_parameters),
        cx.stack("services", "guard", &guard_parameters),
    );
    let (control, notify, schedules, optimizer) = (control?, notify?, schedules?, optimizer?);
    state?;
    web?;
    screenshot?;
    guard?;
    for (stack, made) in [("control", control), ("notify", notify), ("schedules", schedules), ("image", optimizer)] {
        outputs.extend(made.into_iter().map(|(k, v)| (format!("{stack}.{k}"), v)));
    }

    // Project files and deployment records in the state table from the start, with a copy in the
    // bucket (`forkrail state mode dynamodb`), as an install moved there by hand has them.
    let store = crate::table::StoreConfig { mode: crate::table::Mode::Dynamodb, table: forkrail_common::install::STATE_TABLE.to_owned(), s3_copy: true };
    let body = serde_json::to_vec_pretty(&store).expect("serializes");
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    cx.call("services", "keeping state in the table", || {
        s3.put_object().bucket(&bucket).key(forkrail_common::install::STORE_KEY).content_type("application/json").body(body.clone().into()).send().map_err_text()
    })
    .await?;
    Ok(outputs)
}

/// The stacks whose functions ask for more memory than a new account may allow: the parameter,
/// and what it asks for (the template's default).
const MEMORY: &[(&str, &str, u32)] = &[
    ("control", "MemorySize", 1024),
    ("image", "MemorySize", 2048),
    ("screenshot", "MemorySize", 2048),
    ("builder", "MemorySize", 4096),
    ("builder", "LargeMemorySize", 8192),
    ("install-fetch", "MemorySize", 1024),
];

/// `parameters` with the stack's memory fitted to `cap`: each function gets what it asks for or the
/// cap, whichever is less. The large builder exists only when it would have more than the builder.
fn fit_memory(name: &str, parameters: &[(&str, String)], cap: Option<u32>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = parameters.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect();
    let Some(cap) = cap else { return out };
    for &(stack, parameter, wanted) in MEMORY {
        if stack != name || wanted <= cap || parameters.iter().any(|(k, _)| *k == parameter) {
            continue;
        }
        let value = if parameter == "LargeMemorySize" { if cap > 4096 { cap } else { 0 } } else { cap };
        out.push((parameter.to_owned(), value.to_string()));
    }
    out
}

/// Whether to copy the release again: one was copied from another commit, and a step that takes
/// what it copied (the services, the log server, the router) hasn't run yet. Steps that ran keep
/// what they were made with.
fn release_again(record: &Record, commit: &str) -> bool {
    let copied = record.output("release", "commit").unwrap_or_default();
    record.done("release") && !commit.is_empty() && copied != commit && COPIED_FOR.iter().any(|s| !record.done(s))
}

/// Whether to copy the release again when the account copied the published one: it was published
/// again since (or, copied from a folder, for another version), and a step that takes what it
/// copied hasn't run yet.
fn published_again(record: &Record, sha256: &str) -> bool {
    let again = match record.output("release", "release_sha256") {
        Some(copied) => copied != sha256,
        None => record.output("release", "version").is_some_and(|v| v != super::download::version()),
    };
    record.done("release") && again && COPIED_FOR.iter().any(|s| !record.done(s))
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

/// The step's output `key`, or a problem saying the record lacks it.
fn need(cx: &Cx, step: &str, from: &str, key: &str) -> Result<String, Problem> {
    cx.record.output(from, key).map(str::to_owned).ok_or_else(|| {
        problem(
            step,
            "reading what earlier steps made",
            format!("the record has no {from}.{key}"),
            "The installer's record is missing something an earlier step should have left.",
            "Run `forkrail install` again; if it stops here again, open an issue.",
        )
    })
}

/// Stack forkrail-`name` for a machine: made with the newest image, then kept on the image it
/// has, since a newer one would replace the machine.
async fn machine(cx: &Cx, step: &str, name: &str, mut parameters: Vec<(&str, String)>) -> Result<stacks::Outputs, Problem> {
    let config = cx.config.clone();
    let exists = cx.call(step, "looking for the machine", || stacks::exists(&config, name)).await?;
    if !exists {
        let image = cx.call(step, "finding Amazon Linux 2023's newest image", || boxes::image(&config)).await?;
        parameters.push(("ImageId", image));
    }
    cx.stack(step, name, &parameters).await
}

/// Sets a machine up: `files` to the bucket, then `script` on it through SSM, with `env`.
#[allow(clippy::too_many_arguments)]
async fn set_up(cx: &Cx, step: &str, instance: &str, dir: &str, files: Vec<(&str, String)>, script: &str, env: &[(&str, String)], minutes: u64) -> Result<String, Problem> {
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let bucket = cx.bucket();
    cx.call(step, "copying the machine's setup files", || boxes::upload(&s3, &bucket, dir, &files)).await?;
    let config = cx.config.clone();
    cx.report.note(format!("     waiting for {instance} to start and check in (a minute or two)"));
    cx.call(step, &format!("waiting for {instance} to check in"), || boxes::wait_for_ssm(&config, instance)).await?;
    let command = boxes::command(&bucket, cx.region(), dir, script, env);
    cx.report.note(format!("     setting {instance} up"));
    let out = cx.call(step, &format!("setting {instance} up"), || boxes::run(&config, instance, &command, std::time::Duration::from_secs(minutes * 60))).await?;
    for line in out.lines().filter(|l| l.starts_with("---") || l.starts_with("router:") || l.starts_with("ClickHouse")) {
        cx.report.note(format!("     {}", line.trim_start_matches("--- ")));
    }
    Ok(out)
}

/// The network (security groups and Elastic IPs for both machines), then the analytics box:
/// ClickHouse, which keeps the routers' logs and what the dashboard shows of them.
async fn analytics(cx: &Cx) -> Result<Outputs, Problem> {
    let mut outputs = Outputs::new();
    let config = cx.config.clone();
    let place = cx.call("analytics", "finding the default network and a zone with both machine sizes", || boxes::placement(&config)).await?;
    outputs.insert("vpc".into(), place.vpc.clone());
    outputs.insert("zone".into(), place.zone.clone());
    outputs.insert("subnet".into(), place.subnet.clone());
    let network = cx.stack("analytics", "install-network", &[("VpcId", place.vpc.clone())]).await?;
    outputs.extend(network.iter().map(|(k, v)| (format!("network.{k}"), v.clone())));
    let get = |key: &str| network.get(key).cloned().unwrap_or_default();
    let machine = machine(
        cx,
        "analytics",
        "install-analytics",
        vec![
            ("Name", "forkrail-analytics".into()),
            ("InstanceType", boxes::ANALYTICS_TYPE.into()),
            ("DiskGb", "50".into()),
            ("SubnetId", place.subnet.clone()),
            ("SecurityGroupId", get("AnalyticsSecurityGroupId")),
            ("AllocationId", get("AnalyticsAllocationId")),
        ],
    )
    .await?;
    let instance = machine.get("InstanceId").cloned().unwrap_or_default();
    let private = machine.get("PrivateIp").cloned().unwrap_or_default();
    let public = get("AnalyticsAddress");
    let files = boxes::ANALYTICS_FILES.iter().map(|(n, b)| (*n, (*b).to_owned())).collect();
    let env = [("PUBLIC_IP", public.clone()), ("PRIVATE_IP", private.clone()), ("CLICKHOUSE_VERSION", boxes::CLICKHOUSE_VERSION.to_owned())];
    set_up(cx, "analytics", &instance, "analytics", files, "analytics.sh", &env, 20).await?;

    // Where the box is, for the routers' Vector, the dashboard and the CLI (as analytics.sh writes it).
    let endpoint = serde_json::json!({
        "region": cx.region(), "private_ip": private, "public_ip": public, "http_port": 8123, "https_port": 8443,
        "database": "forkrail", "ingest_user": "ingest", "reader_user": "reader",
    });
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let bucket = cx.bucket();
    let body = serde_json::to_vec_pretty(&endpoint).expect("json");
    cx.call("analytics", "saving where the box is", || {
        s3.put_object().bucket(&bucket).key("state/analytics/endpoint.json").content_type("application/json").body(body.clone().into()).send().map_err_text()
    })
    .await?;
    outputs.insert("instance".into(), instance);
    outputs.insert("private_ip".into(), private);
    outputs.insert("public_ip".into(), public);
    Ok(outputs)
}

/// The router's settings: the install's queues, the analytics box, the domain and its zone.
fn router_env(cx: &Cx) -> Result<String, Problem> {
    let bucket = cx.bucket();
    let region = cx.region();
    let analytics = need(cx, "router", "analytics", "private_ip")?;
    let events = need(cx, "router", "services", "notify.QueueUrl")?;
    let spend = need(cx, "router", "services", "control.SpendQueueUrl")?;
    let schedules = need(cx, "router", "services", "schedules.QueueUrl")?;
    let lines = [
        format!("AWS_REGION={region}"),
        format!("FORKRAIL_STATE_REGION={region}"),
        format!("FORKRAIL_HOME_REGION={region}"),
        format!("FORKRAIL_ROUTES=s3://{bucket}/state/routes.json"),
        // No catch-all function: hosts no project has get a 404.
        "FORKRAIL_DEFAULT_POOL=0".to_owned(),
        "RUST_LOG=info".to_owned(),
        format!("FORKRAIL_BASE_DOMAIN={}", cx.record.domain),
        "FORKRAIL_TLS_LISTEN=0.0.0.0:443".to_owned(),
        "FORKRAIL_REDIRECT_LISTEN=0.0.0.0:80".to_owned(),
        format!("FORKRAIL_TLS_CERTS=s3://{bucket}/state/certs"),
        format!("FORKRAIL_CLOUDFLARE_ZONE={}", cx.record.zone_id),
        "FORKRAIL_ACME_STAGING=0".to_owned(),
        format!("FORKRAIL_EVENTS_QUEUE={events}"),
        format!("FORKRAIL_SPEND_QUEUE={spend}"),
        format!("FORKRAIL_SCHEDULE_QUEUE={schedules}"),
        format!("FORKRAIL_EGRESS_TARGETS=analytics={analytics}:8443"),
        format!("FORKRAIL_CLICKHOUSE_URL=http://{analytics}:8123"),
    ];
    Ok(lines.join("\n") + "\n")
}

/// The router: its machine, then its settings, the log export, GeoIP, Vector, and the router
/// itself, which gets the domain's certificate from Let's Encrypt as it starts.
async fn router(cx: &Cx) -> Result<Outputs, Problem> {
    let mut outputs = Outputs::new();
    let env_file = router_env(cx)?;
    let machine = machine(
        cx,
        "router",
        "install-router",
        vec![
            ("Name", "forkrail-router".into()),
            ("InstanceType", boxes::ROUTER_TYPE.into()),
            ("DiskGb", "20".into()),
            ("SubnetId", need(cx, "router", "analytics", "subnet")?),
            ("SecurityGroupId", need(cx, "router", "analytics", "network.RouterSecurityGroupId")?),
            ("AllocationId", need(cx, "router", "analytics", "network.RouterAllocationId")?),
        ],
    )
    .await?;
    let instance = machine.get("InstanceId").cloned().unwrap_or_default();
    let mut files: Vec<(&str, String)> = boxes::ROUTER_FILES.iter().map(|(n, b)| (*n, (*b).to_owned())).collect();
    files.push(("router.env", env_file));
    let analytics = need(cx, "router", "analytics", "private_ip")?;
    let env = [
        ("VECTOR_KEY", need(cx, "router", "release", "vector_key")?),
        ("VECTOR_SHA256", need(cx, "router", "release", "vector_sha256")?),
        ("CLICKHOUSE_URL", format!("http://{analytics}:8123")),
    ];
    set_up(cx, "router", &instance, "router", files, "router.sh", &env, 15).await?;
    outputs.insert("instance".into(), instance);
    outputs.insert("public_ip".into(), need(cx, "router", "analytics", "network.RouterAddress")?);
    Ok(outputs)
}

/// The Cloudflare token: the one given this run, else the one kept in SSM.
async fn cloudflare_token(cx: &Cx, step: &str) -> Result<String, Problem> {
    if let Some(token) = &cx.token {
        return Ok(token.trim().to_owned());
    }
    let ssm = aws_sdk_ssm::Client::new(&cx.config);
    let out = cx.call(step, "reading the Cloudflare token", || ssm.get_parameter().name(CLOUDFLARE_TOKEN_PARAM).with_decryption(true).send().map_err_text()).await?;
    Ok(out.parameter().and_then(|p| p.value()).unwrap_or_default().to_owned())
}

/// The domain: the CLI's settings (projects at `<name>.<domain>`), the wildcard record pointing at
/// the router, and the certificate the router got for it.
async fn domain(cx: &Cx) -> Result<Outputs, Problem> {
    let mut outputs = Outputs::new();
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let bucket = cx.bucket();
    let router_ip = need(cx, "domain", "router", "public_ip")?;

    // The settings, kept as they are but for these two.
    let current = s3.get_object().bucket(&bucket).key(forkrail_common::install::SETTINGS_KEY).send().await;
    let mut settings: crate::state::Settings = match current {
        Ok(out) => match out.body.collect().await {
            Ok(bytes) => serde_json::from_slice(&bytes.into_bytes()).unwrap_or_default(),
            Err(_) => Default::default(),
        },
        Err(_) => Default::default(),
    };
    // The dashboard's address comes once it's deployed: builds send their logs there.
    settings.base_domain.clone_from(&cx.record.domain);
    // A new account's memory limit, which the dashboard shows; publishing keeps it current.
    if let Some(cap) = cx.memory_cap.get() {
        settings.memory_limits.insert(cx.region().to_owned(), cap);
    }
    let body = serde_json::to_vec_pretty(&settings).expect("json");
    cx.call("domain", "saving the domain in the install's settings", || {
        s3.put_object().bucket(&bucket).key(forkrail_common::install::SETTINGS_KEY).content_type("application/json").body(body.clone().into()).send().map_err_text()
    })
    .await?;

    let token = cloudflare_token(cx, "domain").await?;
    let cf = Cloudflare::new(&token).map_err(|e| problem("domain", "reaching Cloudflare", format!("{e:#}"), "This machine couldn't set up a connection to Cloudflare.", "Check its network, then run the installer again."))?;
    let name = format!("*.{}", cx.record.domain);
    let zone = cx.record.zone_id.clone();
    cx.call("domain", &format!("pointing {name} at the router ({router_ip}) in Cloudflare"), || {
        let (cf, zone, name, ip) = (&cf, zone.clone(), name.clone(), router_ip.clone());
        async move { cf.set_a(&zone, &name, &ip).await.map_err(|e| format!("{e:#}")) }
    })
    .await?;
    outputs.insert("record".into(), name);

    // The router asks Let's Encrypt for the wildcard as it starts (DNS-01 through Cloudflare).
    cx.report.note("     waiting for the domain's certificate (about a minute)");
    let started = Instant::now();
    loop {
        let listed = s3.list_objects_v2().bucket(&bucket).prefix("state/certs/").send().await;
        let found = listed.ok().is_some_and(|l| {
            l.contents().iter().filter_map(|o| o.key()).any(|k| k.ends_with("/cert.pem") && !k.contains("/staging/") && k.contains(cx.record.domain.as_str()))
        });
        if found {
            break;
        }
        if started.elapsed() > std::time::Duration::from_secs(10 * 60) {
            return Err(problem(
                "domain",
                "waiting for the domain's certificate",
                "no certificate in 10 minutes",
                "The router hasn't got a certificate for the domain from Let's Encrypt yet. Usually the Cloudflare token can't add the _acme-challenge record, or the zone isn't active yet.",
                "Check that the token has DNS edit on the zone and the zone is active in Cloudflare, then run the installer again.",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
    Ok(outputs)
}

/// This program again, as the install's administrator would run it: `forkrail --aws ARGS`.
async fn forkrail(cx: &Cx, step: &str, action: &str, args: &[&str]) -> Result<(), Problem> {
    let exe = std::env::current_exe().map_err(|e| problem(step, action, e.to_string(), "The installer couldn't find its own program.", "Run it again from its full path."))?;
    let mut command = tokio::process::Command::new(exe);
    command.arg("--aws").arg("--region").arg(cx.region());
    if let Some(profile) = &cx.profile {
        command.arg("--profile").arg(profile);
    }
    // This install's account and bucket, never the profile or bucket `forkrail link` saved for another.
    command.args(args).env("FORKRAIL_AWS", "1").env("FORKRAIL_BUCKET", cx.bucket()).env(crate::config::IGNORE_SAVED_ENV, "1").stdin(std::process::Stdio::inherit());
    let status = command.status().await.map_err(|e| problem(step, action, e.to_string(), "The installer couldn't run itself.", "Run `forkrail install` again."))?;
    if status.success() {
        Ok(())
    } else {
        Err(problem(
            step,
            action,
            format!("`forkrail {}` ended with {status}", args.join(" ")),
            "The command's own output above says what went wrong.",
            "Fix what it names, then run `forkrail install` again.",
        ))
    }
}

/// The dashboard's memory: what it asks for, or less where the account allows less.
const DASHBOARD_MEMORY_MB: u32 = 1024;

/// The dashboard, deployed like any site: the forkrail-web project with its role and the
/// analytics egress, served at `dashboard.<domain>`. A release built it already (`web_built`):
/// that build is published as it is, so nothing is built in the account, whose builder may be
/// too small for it at first (a new account's functions get 512 MB, and the build needs about
/// 1.5 GB). An older release has only the source, which the install's builder builds.
async fn dashboard(cx: &Cx) -> Result<Outputs, Problem> {
    let project = forkrail_common::install::DASHBOARD_PROJECT;
    let host = format!("dashboard.{}", cx.record.domain);
    let memory = cx.memory_cap.get().map_or(DASHBOARD_MEMORY_MB, |cap| cap.min(DASHBOARD_MEMORY_MB)).to_string();
    let scratch = std::env::temp_dir().join(format!("forkrail-web-{}", forkrail_common::random_hex(6)));
    let copied = cx.record.output("release", "web_built_key").filter(|_| cx.release.is_none()).map(str::to_owned);
    let (tarball, built, what) = match &copied {
        // The account copied the release: its built dashboard (a few MB) comes down from the bucket.
        Some(key) => {
            let tarball = scratch.with_extension("tar.gz");
            download_object(cx, key, &tarball).await?;
            (tarball, true, "the dashboard")
        }
        None => {
            let release = match &cx.release {
                Some(_) => open_release(cx, "dashboard")?,
                // Copied from a download here: it's kept.
                None => {
                    let dir = super::download::release(&cx.report).await.map_err(|e| {
                        problem(
                            "dashboard",
                            &format!("downloading forkrail {}", super::download::version()),
                            format!("{e:#}"),
                            "forkrail's release couldn't be downloaded.",
                            "Check this computer's internet connection, then run `forkrail install` again. It carries on from here.",
                        )
                    })?;
                    Release::open(&dir).map_err(|e| problem("dashboard", "reading the downloaded release", format!("{e:#}"), "The download is incomplete.", "Run `forkrail install` again."))?
                }
            };
            if let Some(built) = &release.manifest.web_built {
                (release.path(built), true, "the dashboard")
            } else if let Some(source) = &release.manifest.web {
                (release.path(source), false, "the dashboard's source")
            } else {
                // `Release::open` refuses a release with neither.
                return Err(problem("dashboard", "opening the release", "no web_built", "The release has no dashboard.", "Make the release folder again."));
            }
        }
    };
    let unpacked = unpack(&tarball, &scratch).map_err(|e| {
        problem("dashboard", &format!("unpacking {what}"), e.to_string(), &format!("The release's {} couldn't be read.", tarball.display()), "Make the release folder again.")
    });
    if copied.is_some() {
        let _ = std::fs::remove_file(&tarball);
    }
    let unpacked = unpacked?;
    let dir = unpacked.to_string_lossy().into_owned();

    forkrail(cx, "dashboard", "giving the dashboard its role and the analytics box", &["project", project, "--role", "forkrail-dashboard", "--egress", "analytics"]).await?;
    let deployed = if built {
        match fresh_build(&unpacked) {
            Ok(()) => forkrail(cx, "dashboard", "putting the dashboard online", &["publish", &dir, "--memory", &memory]).await,
            Err(e) => Err(problem("dashboard", "reading the dashboard's build", e.to_string(), "The release's built dashboard isn't a forkrail build.", "Make the release folder again.")),
        }
    } else {
        forkrail(cx, "dashboard", "building and deploying the dashboard", &["deploy", &dir, "--project", project, "--memory", &memory, "--prod"]).await
    };
    let _ = std::fs::remove_dir_all(&scratch);
    deployed?;
    forkrail(cx, "dashboard", &format!("serving the dashboard at {host}"), &["domains", "add", &host, project]).await?;
    // Builds send their log lines to it from now on, live.
    forkrail(cx, "dashboard", "saving the dashboard's address", &["dashboard", &cx.record.dashboard_url()]).await?;
    forkrail(cx, "dashboard", "giving the install its deployment-protection key", &["protection", "setup", "--login", &cx.record.dashboard_url()]).await?;
    // The team budget the dashboard shows and the routers keep to: $20 a month to start, growing
    // as it's used until someone sets one; the dashboard itself keeps running at it.
    forkrail(cx, "dashboard", "setting the team's monthly budget", &["spend", "start"]).await?;
    // What the account copied isn't needed any more.
    if let Some(key) = &copied
        && let Some((prefix, _)) = key.rsplit_once('/')
    {
        clear_prefix(cx, &format!("{prefix}/")).await;
    }
    Ok(Outputs::from([("url".to_owned(), cx.record.dashboard_url())]))
}

/// The install bucket's object `key`, into the file `into`.
async fn download_object(cx: &Cx, key: &str, into: &std::path::Path) -> Result<(), Problem> {
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let bucket = cx.bucket();
    cx.call("dashboard", "downloading the dashboard's build", || {
        let (s3, bucket) = (s3.clone(), bucket.clone());
        async move {
            let out = s3.get_object().bucket(&bucket).key(key).send().await.map_err(|e| aws_text(&e))?;
            let bytes = out.body.collect().await.map_err(|e| e.to_string())?.into_bytes();
            std::fs::write(into, &bytes).map_err(|e| format!("writing {}: {e}", into.display()))
        }
    })
    .await
}

/// Deletes what the install's bucket has under `prefix`, as far as it can.
async fn clear_prefix(cx: &Cx, prefix: &str) {
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let bucket = cx.bucket();
    let Ok(listed) = s3.list_objects_v2().bucket(&bucket).prefix(prefix).send().await else { return };
    for object in listed.contents() {
        if let Some(key) = object.key() {
            let _ = s3.delete_object().bucket(&bucket).key(key).send().await;
        }
    }
}

/// Unpacks a .tar.gz with one top folder into `into`: that folder.
fn unpack(tarball: &std::path::Path, into: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(into)?;
    let file = std::fs::File::open(tarball)?;
    tar::Archive::new(flate2::read::GzDecoder::new(file)).unpack(into)?;
    let top = std::fs::read_dir(into)?.filter_map(Result::ok).find(|e| e.path().is_dir()).map(|e| e.path());
    Ok(top.unwrap_or_else(|| into.to_owned()))
}

/// A release's built dashboard is one build for every install: each publishes it as a deployment
/// of its own, with a new id (an id is only in the build's build.json), made by the installer.
fn fresh_build(dir: &std::path::Path) -> anyhow::Result<()> {
    let path = dir.join("build.json");
    let mut build: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let object = build.as_object_mut().ok_or_else(|| anyhow::anyhow!("{} isn't an object", path.display()))?;
    object.insert("id".into(), forkrail_common::random_hex(4).into());
    object.insert("deployer".into(), "forkrail install".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&build)?)?;
    Ok(())
}

/// Spend protection: AWS's own budget on everything forkrail runs, which stops forkrail's functions
/// at its limit, and the cost allocation tag it counts by. Both live in us-east-1, where AWS keeps
/// budgets and billing, whatever the install's region.
async fn protection(cx: &Cx) -> Result<Outputs, Problem> {
    let mut outputs = Outputs::new();
    let billing = cx.config_in("us-east-1");
    // The tag first. A member account of an AWS organization can't turn it on (only the
    // organization's management account can): the budget then counts the whole account.
    let scope = match stacks::apply(&billing, "cost-tags", &[]).await {
        Ok(_) => "tag",
        Err(said) => {
            let _ = stacks::delete(&billing, "cost-tags").await;
            if said.contains("Linked account") || said.contains("AccessDenied") {
                cx.report.note("     This account belongs to an AWS organization, where only the organization's main account can turn on the tag forkrail's costs are counted by. So the spending limit counts everything this account spends.");
            } else {
                cx.report.note(format!("     forkrail's cost tag couldn't be turned on ({}), so the spending limit counts everything this account spends.", super::problem::brief(&said)));
            }
            "account"
        }
    };
    outputs.insert("cost_tags".into(), if scope == "tag" { "on" } else { "off" }.into());
    let email = cx.record.email.clone().unwrap_or_default();
    let topic = if email.is_empty() {
        // No one to email: the budget tells the alerts topic, and still stops the functions.
        let Some(topic) = cx.record.output("services", "alerts_topic").map(str::to_owned) else {
            cx.report.note("     No alert email and no alerts topic, so no spending limit yet. Run the installer again and add an email.");
            return Ok(outputs);
        };
        let_budgets_publish(cx, &topic).await?;
        cx.report.note("     No alert email, so the spending limit stops your apps at the limit without emailing anyone. Run the installer again to add one.");
        topic
    } else {
        String::new()
    };
    let dollars = std::env::var("FORKRAIL_INSTALL_BUDGET").ok().filter(|d| d.parse::<u32>().is_ok()).unwrap_or_else(|| BUDGET_DOLLARS.to_owned());
    let config = billing.clone();
    let params = [("Dollars", dollars.clone()), ("AlertEmail", email), ("AlertTopic", topic), ("Scope", scope.to_owned())];
    cx.call("protection", "applying the CloudFormation stack forkrail-spend-backstop (us-east-1)", || stacks::apply(&config, "spend-backstop", &params)).await?;
    outputs.insert("budget_dollars".into(), dollars);
    outputs.insert("budget_region".into(), "us-east-1".into());
    outputs.insert("budget_scope".into(), scope.into());
    Ok(outputs)
}

/// The alerts topic's policy, with AWS Budgets allowed to publish to it (as AWS's guide for budget
/// notifications gives it). What else the policy allows stays.
async fn let_budgets_publish(cx: &Cx, topic: &str) -> Result<(), Problem> {
    const SID: &str = "ForkrailBudgetsPublish";
    let sns = aws_sdk_sns::Client::new(&cx.config);
    let attributes = cx.call("protection", "reading the alerts topic's policy", || sns.get_topic_attributes().topic_arn(topic).send().map_err_text()).await?;
    let current = attributes.attributes().and_then(|a| a.get("Policy")).cloned().unwrap_or_default();
    let mut policy: serde_json::Value = serde_json::from_str(&current).unwrap_or_else(|_| serde_json::json!({"Version": "2012-10-17", "Statement": []}));
    let account = cx.account();
    let statement = serde_json::json!({
        "Sid": SID,
        "Effect": "Allow",
        "Principal": {"Service": "budgets.amazonaws.com"},
        "Action": "SNS:Publish",
        "Resource": topic,
        "Condition": {
            "StringEquals": {"aws:SourceAccount": account},
            "ArnLike": {"aws:SourceArn": format!("arn:{}:budgets::{account}:*", cx.caller.partition)},
        },
    });
    let Some(statements) = policy.get_mut("Statement").and_then(|s| s.as_array_mut()) else {
        return Err(problem("protection", "reading the alerts topic's policy", current, "The alerts topic's policy isn't one forkrail can read.", "Run `forkrail install` again; if it stops here again, open an issue."));
    };
    statements.retain(|s| s.get("Sid").and_then(|v| v.as_str()) != Some(SID));
    statements.push(statement);
    let text = policy.to_string();
    cx.call("protection", "letting AWS Budgets use the alerts topic", || sns.set_topic_attributes().topic_arn(topic).attribute_name("Policy").attribute_value(&text).send().map_err_text()).await?;
    Ok(())
}

/// The monthly budget for everything tagged project=forkrail: the install's fixed cost (about $55)
/// with room for traffic. `FORKRAIL_INSTALL_BUDGET` changes it.
pub const BUDGET_DOLLARS: &str = "150";

/// The install's GitHub App, which people sign in to the dashboard with. Made before the steps
/// (`make_github_app` in mod.rs) and kept in a file until now, when its keys go into SSM.
/// Without that file (a run with --yes, or one continued on another computer) it's made now:
/// `forkrail git setup` opens GitHub with the app filled in and waits for the click on Create.
async fn github(cx: &Cx) -> Result<Outputs, Problem> {
    let made = super::github_file(cx.account());
    if let Some(file) = made.as_ref().filter(|f| f.is_file()) {
        let path = file.to_string_lossy().into_owned();
        forkrail(cx, "github", "storing the GitHub App's keys", &["git", "setup", "--from", &path]).await?;
        let _ = std::fs::remove_file(file);
        return Ok(Outputs::new());
    }
    cx.report.send(Event::Open {
        url: String::new(),
        why: "Last step: a tab opens on GitHub with forkrail's app filled in. Click \"Create GitHub App\"; you'll sign in to the dashboard with it. If no tab opened, the terminal shows the address.".into(),
    });
    forkrail(cx, "github", "making the install's GitHub App", &["git", "setup"]).await?;
    Ok(Outputs::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_says_what_it_does() {
        for (id, title) in ALL {
            assert!(!about(id).is_empty(), "{id}");
            assert!(title.len() < 40, "{title}");
        }
    }

    #[test]
    fn functions_fit_the_accounts_memory() {
        let given = [("CodeKey", "k".to_owned())];
        let get = |out: &[(String, String)], key: &str| out.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(get(&fit_memory("control", &given, None), "MemorySize"), None);
        assert_eq!(get(&fit_memory("control", &given, Some(512)), "MemorySize").as_deref(), Some("512"));
        assert_eq!(get(&fit_memory("control", &given, Some(3008)), "MemorySize"), None);
        let builder = fit_memory("builder", &[], Some(3008));
        assert_eq!((get(&builder, "MemorySize").as_deref(), get(&builder, "LargeMemorySize").as_deref()), (Some("3008"), Some("0")));
        let builder = fit_memory("builder", &[], Some(6144));
        assert_eq!((get(&builder, "MemorySize"), get(&builder, "LargeMemorySize").as_deref()), (None, Some("6144")));
        assert_eq!(get(&fit_memory("control", &given, Some(512)), "CodeKey").as_deref(), Some("k"));
        // A 512 MB account: every function gets 512, and there's no large builder.
        let builder = fit_memory("builder", &[], Some(512));
        assert_eq!((get(&builder, "MemorySize").as_deref(), get(&builder, "LargeMemorySize").as_deref()), (Some("512"), Some("0")));
        assert_eq!(get(&fit_memory("screenshot", &[], Some(512)), "MemorySize").as_deref(), Some("512"));
    }

    #[test]
    fn a_newer_release_is_copied_again_until_its_parts_are_running() {
        let mut record = Record::default();
        assert!(!release_again(&record, "bbb"));
        record.finish("release", 1, Outputs::from([("commit".to_owned(), "aaa".to_owned())]));
        assert!(release_again(&record, "bbb"));
        assert!(!release_again(&record, "aaa"));
        assert!(!release_again(&record, ""));
        for step in ["services", "analytics", "router"] {
            record.finish(step, 2, Outputs::new());
        }
        assert!(!release_again(&record, "bbb"));
    }

    #[test]
    fn a_released_dashboard_is_published_as_a_new_deployment() {
        let dir = crate::package::TempDir::new("web-built-test").unwrap();
        let built = dir.path().join("forkrail-web-built");
        std::fs::create_dir_all(&built).unwrap();
        std::fs::write(built.join("build.json"), r#"{"format": 1, "id": "00000000", "project": "forkrail-web", "deployer": "forkrail release"}"#).unwrap();
        let tarball = dir.path().join("web-built.tar.gz");
        {
            let file = std::fs::File::create(&tarball).unwrap();
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(file, flate2::Compression::fast()));
            tar.append_dir_all("forkrail-web-built", &built).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let into = dir.path().join("unpacked");
        let top = unpack(&tarball, &into).unwrap();
        assert_eq!(top, into.join("forkrail-web-built"));
        fresh_build(&top).unwrap();
        let build: serde_json::Value = serde_json::from_slice(&std::fs::read(top.join("build.json")).unwrap()).unwrap();
        assert_ne!(build["id"], "00000000");
        assert_eq!(build["id"].as_str().map(str::len), Some(8));
        assert_eq!((build["project"].as_str(), build["deployer"].as_str()), (Some("forkrail-web"), Some("forkrail install")));
    }
}
