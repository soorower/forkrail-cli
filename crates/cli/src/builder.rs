//! The builder's side of a remote build (remote.rs): what one invocation of `forkrail-builder`
//! (crates/builder) does with a `Job`. It reads its order and source through the job's presigned
//! URLs, builds exactly as `forkrail build` does, with its lines going to the dashboard live, and
//! writes its output and its log's copy the same way. It holds no AWS rights of its own.
//!
//! An execution environment builds one deployment at a time, but builds one after another, of any
//! project. Before each, everything a previous build left is gone: every process it started (they
//! could read the next build's order otherwise), and, when the project changes, every file in the
//! scratch folder (a cache another project's build wrote into could put its code in this one's
//! package). Builds of the same project keep their caches, as a laptop's do.
//!
//! A builder that starts fresh, as most do, has none of that, so a build first restores the
//! project's build cache from S3 (cache.rs: its `node_modules`, or its installed Python
//! requirements) and, once it has built, writes it back if it changed. A Next.js build's
//! `.next/cache` has an archive of its own, written after every build while the output is.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::build::{self, Options};
use crate::buildlog::{self, BuildLog, Mask};
use crate::cache::{self, Kind, Plan, Restored};
use crate::fetch::Http;
use crate::framework::Install;
use crate::package;
use crate::remote::{CacheUrls, Ending, Job, Order, Outcome, Timings};
use crate::report::{self, Echo, Endpoint, Report};
use crate::state::{DeploymentRecord, Failure, Status, Target, now_ms};

/// The builder's scratch folder: Lambda's `/tmp`, unless `FORKRAIL_BUILDER_ROOT` names another
/// (a test's own folder).
pub const ROOT_ENV: &str = "FORKRAIL_BUILDER_ROOT";
/// How often a build looks at its record for a cancel.
const CANCEL_POLL: Duration = Duration::from_secs(2);
/// A build stops this long before the invocation's deadline, to write its output and log.
const DEADLINE_MARGIN: Duration = Duration::from_secs(60);
/// How long stopped commands get to end on SIGTERM, then after SIGKILL before the build is given up on.
const TERM_GRACE: Duration = Duration::from_secs(3);
const STOP_WAIT: Duration = Duration::from_secs(15);
/// The build cache is written only with this long left of the invocation, and given up on this
/// long before its end: a build that's done isn't lost to keeping its cache.
const KEEP_NEEDS: Duration = Duration::from_secs(90);
const KEEP_MARGIN: Duration = Duration::from_secs(30);
/// Why a cache isn't unpacked when the builder has its contents from the build before.
const HERE_ALREADY: &str = "here already";

pub struct Builder {
    http: Http,
    root: PathBuf,
    bootstrap: PathBuf,
    python: String,
    /// What the first line of each log says this builder is.
    about: String,
    /// What a build cache must have been written by to be used here: this system, Node's major
    /// version and Python's minor.
    platform: String,
    /// The project and target the scratch folder's files belong to.
    last_build: Mutex<Option<(String, Target)>>,
}

impl Builder {
    /// The builder in this environment. `root` must be a folder of its own: everything in it is
    /// deleted between builds of different projects or targets.
    pub fn new(root: PathBuf, bootstrap: PathBuf, python: String) -> Result<Self> {
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let shared = ["/", "/tmp", "/private/tmp", "/var/tmp"].iter().map(PathBuf::from).chain(std::env::var_os("HOME").map(PathBuf::from));
        if !in_lambda() && shared.map(|p| canonical(&p)).any(|p| p == canonical(&root)) {
            bail!("{} is emptied between builds: outside Lambda, set {ROOT_ENV} to a folder of the builder's own", root.display());
        }
        let memory = std::env::var("AWS_LAMBDA_FUNCTION_MEMORY_SIZE").map(|m| format!("{m} MB, ")).unwrap_or_default();
        let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
        let node = version("node", &["--version"]).trim_start_matches('v').to_owned();
        let py = version(&python, &["--version"]).trim_start_matches("Python ").to_owned();
        let about = format!("Building in forkrail's builder: {memory}{os} {arch}, Node {node}, Python {py}");
        let platform = format!("{os} {arch}, Node {}, Python {}", parts(&node, 1), parts(&py, 2));
        Ok(Self { http: Http::new()?, root, bootstrap, python, about, platform, last_build: Mutex::new(None) })
    }

    /// Build `job` before Unix millisecond `deadline_ms`.
    pub async fn run(&self, job: Job, deadline_ms: u64) -> Outcome {
        let mut timings = Timings { queued_ms: now_ms().saturating_sub(job.sent_ms), ..Default::default() };
        let fetching = Instant::now();
        // The order first: it holds the values the log must hide, and the target the scratch
        // folder is readied for.
        let order = self.order(&job).await;
        if let Err(e) = self.clean(&job.project, order.as_ref().ok().map(|o| o.request.target)) {
            return early(Failure { step: Some(buildlog::PREPARING.into()), message: format!("cleaning the builder: {e:#}") }, timings);
        }
        let mask = order.as_ref().map(|o| Mask::new(&o.request.env)).unwrap_or_default();
        let endpoint = job.log_endpoint.clone().map(|url| Endpoint { url, key: job.key.clone() });
        // A build sent again on the large builder goes on after the first build's lines.
        let (after, after_bytes) = match (&job.again, &endpoint) {
            (Some(_), Some(endpoint)) => self.log_so_far(endpoint).await,
            _ => (0, 0),
        };
        let report = Report::start_after(endpoint, mask, Echo::Masked, after, after_bytes);
        if let Some(why) = &job.again {
            report.info(why);
        }
        report.step(buildlog::PREPARING);
        report.info(&self.about);
        let work = self.root.join("work");
        let ended = async {
            let order = order?;
            if self.canceled(&job).await {
                return Ok(Ending::Canceled);
            }
            let source = work.join("source.zip");
            let size = self.http.get_to_file(&job.source_url, &source).await.context("reading the source")?.context("the source is gone")?;
            let src = work.join("src");
            let (from, to) = (source.clone(), src.clone());
            tokio::task::spawn_blocking(move || crate::remote::unzip(&from, &to)).await.context("unzipping the source")??;
            timings.fetch_ms = fetching.elapsed().as_millis() as u64;
            report.info(&format!("source: {:.1} MB, read in {:.1} s", size as f64 / 1e6, fetching.elapsed().as_secs_f64()));
            // A push's build asks the project's ignoreCommand before anything more is fetched.
            let mut order = order;
            if order.request.pushed {
                let (dir, request, log) = (src.clone(), order.request.clone(), report.clone());
                match self.guarded(&job, deadline_ms, move || build::ask_first(&dir, &request, &*log)).await {
                    Ok(Some(())) => order.request.pushed = false,
                    Ok(None) => return Ok(Ending::Canceled),
                    Err(e) if e.downcast_ref::<build::Ignored>().is_some() => return Ok(Ending::Skipped),
                    Err(e) => return Err(e),
                }
            }
            let restoring = Instant::now();
            let keeps = self.restore(&job, &order, &src, &report).await;
            timings.restore_ms = restoring.elapsed().as_millis() as u64;

            let building = Instant::now();
            let out = work.join("out");
            let ending = self.build(&job, &order, &src, &out, report.clone(), deadline_ms).await?;
            timings.build_ms = building.elapsed().as_millis() as u64;
            if ending != Ending::Built {
                return Ok(ending);
            }
            // Nothing the build left running (a detached child of `npm run build`) gets to change
            // the output while it's packed and written.
            kill_others();
            let uploading = Instant::now();
            let zip = work.join("out.zip");
            let (from, to) = (out.clone(), zip.clone());
            let output = async {
                let bytes = tokio::task::spawn_blocking(move || zip_folder(&from, &to)).await.context("zipping the output")??;
                self.http.put_file(&job.output_url, &zip).await.context("writing the output")?;
                Ok::<_, anyhow::Error>((bytes, uploading.elapsed()))
            };
            // A Next build's `.next/cache` is kept after every build, packed and written while the
            // output is, so publishing (which starts when this invocation ends) waits on it little.
            let next = async {
                if let Some(keep) = keeps.next {
                    self.keep(keep, deadline_ms, &report).await;
                }
            };
            let (written, ()) = tokio::join!(output, next);
            let (bytes, took) = written?;
            timings.upload_ms = uploading.elapsed().as_millis() as u64;
            report.info(&format!(
                "built in {:.1} s; output {:.1} MB written in {:.1} s",
                building.elapsed().as_secs_f64(),
                bytes as f64 / 1e6,
                took.as_secs_f64()
            ));
            if let Some(keep) = keeps.deps.filter(|k| !k.current) {
                let keeping = Instant::now();
                self.keep(keep, deadline_ms, &report).await;
                timings.keep_ms = keeping.elapsed().as_millis() as u64;
            }
            Ok::<_, anyhow::Error>(Ending::Built)
        }
        .await;
        let mut out_of_memory = false;
        let (ending, error) = match ended {
            Ok(ending) => (ending, None),
            Err(e) => {
                out_of_memory = e.chain().any(|c| c.is::<build::OutOfMemory>());
                let message = report.mask(&format!("{e:#}"));
                report.info(&format!("Error: {message}"));
                (Ending::Failed, Some(Failure { step: report.step_now(), message }))
            }
        };
        if ending == Ending::Canceled {
            report.info("Canceled");
        }
        let closed = report.close().await;
        // Going on from the first build's lines, a copy of this build's alone would hide them:
        // ClickHouse has them all, and the dashboard reads them there.
        let log_kept = after == 0
            && match report::gzip_lines(&closed.lines) {
                Ok(gz) => self.http.put(&job.log_url, Bytes::from(gz)).await.is_ok(),
                Err(_) => false,
            };
        // Whatever the build started is stopped before anything else can run here.
        kill_others();
        let _ = std::fs::remove_dir_all(&work);
        Outcome { ending, error, lines: closed.lines.last().map_or(after, |l| l.seq), bytes: closed.bytes, cut: closed.cut, log_kept, timings, out_of_memory }
    }

    /// How far the deployment's log has got, as the dashboard counts it: its last line's number and
    /// its size. (0, 0) when it can't say (no such count yet), and this build's log starts over.
    async fn log_so_far(&self, endpoint: &Endpoint) -> (u32, u64) {
        let auth = format!("Bearer {}", endpoint.key);
        let Ok(body) = self.http.call(hyper::Method::GET, &endpoint.url, &[("authorization", &auth)], None).await else {
            return (0, 0);
        };
        let Ok(so_far) = serde_json::from_slice::<serde_json::Value>(&body) else { return (0, 0) };
        let seq = so_far.get("seq").and_then(serde_json::Value::as_u64).and_then(|s| u32::try_from(s).ok()).unwrap_or(0);
        (seq, if seq == 0 { 0 } else { so_far.get("bytes").and_then(serde_json::Value::as_u64).unwrap_or(0) })
    }

    /// The build itself, until it ends, is canceled, or runs out of time.
    async fn build(&self, job: &Job, order: &Order, src: &Path, out: &Path, report: Arc<Report>, deadline_ms: u64) -> Result<Ending> {
        let opts = Options {
            package: package::Options { bootstrap: self.bootstrap.clone(), python: self.python.clone(), no_cache: order.no_cache },
            prebuilt: false,
            // Its node_modules is an earlier build's, or isn't there: the install command decides.
            install: Install::Always,
        };
        let (request, src, out) = (order.request.clone(), src.to_path_buf(), out.to_path_buf());
        match self.guarded(job, deadline_ms, move || build::build(&src, &request, &opts, &out, &*report).map(|_| ())).await {
            Ok(Some(())) => Ok(Ending::Built),
            Ok(None) => Ok(Ending::Canceled),
            // The project's ignoreCommand said this commit needs no build.
            Err(e) if e.downcast_ref::<build::Ignored>().is_some() => Ok(Ending::Skipped),
            Err(e) => Err(e),
        }
    }

    /// Run `work`, which runs the project's commands, on a blocking thread: until it ends (what it
    /// made), the deployment is canceled (`None`), or the builder's time runs out (an error).
    /// Either of the last two stops the project's commands.
    async fn guarded<T, F>(&self, job: &Job, deadline_ms: u64, work: F) -> Result<Option<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let mut task = tokio::task::spawn_blocking(work);
        let left = Duration::from_millis(deadline_ms.saturating_sub(now_ms())).saturating_sub(DEADLINE_MARGIN);
        let stop = tokio::select! {
            done = &mut task => return done.context("the build stopped")?.map(Some),
            _ = self.watch(job) => Ok(None),
            _ = tokio::time::sleep(left) => Err(anyhow::anyhow!(
                "the build didn't finish within {} minutes, the builder's limit (Lambda's 15 minutes); build with --local for a longer one",
                (left.as_secs() + 30) / 60
            )),
        };
        buildlog::stop_commands();
        // Its commands get a moment to end on SIGTERM; then whatever ignored it, or left its process
        // group and still holds the build's output open, is killed. The build then returns their
        // failure, which isn't the story.
        if tokio::time::timeout(TERM_GRACE, &mut task).await.is_err() {
            kill_others();
            let _ = tokio::time::timeout(STOP_WAIT, task).await;
        }
        stop
    }

    /// Bring back what the project's earlier builds kept, if this build may use it. Returns what to
    /// do about the caches once the build is done: nothing for a project that keeps nothing or a
    /// job without a cache. Nothing here fails a build: it says why and builds without.
    async fn restore(&self, job: &Job, order: &Order, src: &Path, report: &Report) -> Keeps {
        let Some(urls) = job.cache.as_ref() else { return Keeps::default() };
        let Some(mut plan) = cache::plan_as(src, order.request.framework.as_deref()) else { return Keeps::default() };
        // A Next build's `.next/cache` goes in an archive of its own when the job names one; a job
        // from an older forkrail doesn't, and it stays with the dependencies, as it was.
        let next = job.next_cache.as_ref().and_then(|urls| plan.split_next().map(|plan| (urls, plan)));
        if order.no_cache {
            report.info("build cache: not used (--no-cache); a new one is kept");
        }
        // That one first: the dependencies' archive, if it was written before the two were apart,
        // holds an older `.next/cache`, which a restored one keeps out.
        let next = match next {
            Some((urls, plan)) => Some(self.restore_one(urls, plan, order, report).await),
            None => None,
        };
        Keeps { deps: Some(self.restore_one(urls, plan, order, report).await), next }
    }

    /// `restore` for one archive: the first of `urls.read` that's there, if `plan` accepts it.
    async fn restore_one(&self, urls: &CacheUrls, plan: Plan, order: &Order, report: &Report) -> Keep {
        let mut keep = Keep { plan: plan.clone(), url: urls.write.clone(), current: false };
        if order.no_cache {
            return keep;
        }
        let what = kept_name(plan.kind);
        let archive = self.root.join("work").join(format!("{}.tar.zst", file_name(plan.kind)));
        let staging = self.root.join("work").join(format!("{}-in", file_name(plan.kind)));
        let mut said = false;
        for (i, url) in urls.read.iter().enumerate() {
            let started = Instant::now();
            // The second is production's, for a preview without a cache of its own.
            let whose = if i == 0 { "" } else { "production's " };
            let size = match self.http.get_to_file(url, &archive).await {
                Ok(Some(size)) => size,
                Ok(None) => continue,
                Err(e) => {
                    report.info(&format!("warning: the build cache couldn't be read: {e:#}"));
                    return keep;
                }
            };
            let (from, into, platform, plan) = (archive.clone(), staging.clone(), self.platform.clone(), plan.clone());
            let restored = tokio::task::spawn_blocking(move || {
                let present = plan.present();
                cache::restore(&from, &plan.base, &into, |m| {
                    plan.accepts(m, &platform)?;
                    if present { Err(HERE_ALREADY.to_owned()) } else { Ok(()) }
                })
            })
            .await;
            match restored {
                Ok(Ok(Restored::Done(manifest, counts))) => {
                    keep.current = manifest.key == keep.plan.key;
                    report.info(&format!(
                        "build cache: {whose}{what} restored ({} files, {:.1} MB, from {:.1} MB) in {:.1} s",
                        counts.files,
                        counts.bytes as f64 / 1e6,
                        size as f64 / 1e6,
                        started.elapsed().as_secs_f64()
                    ));
                    return keep;
                }
                Ok(Ok(Restored::Refused(manifest, why))) if why == HERE_ALREADY => {
                    keep.current = manifest.key == keep.plan.key;
                    return keep;
                }
                Ok(Ok(Restored::Refused(_, why))) => {
                    report.info(&format!("build cache: {whose}{what} not used: {why}"));
                    said = true;
                }
                Ok(Err(e)) => {
                    report.info(&format!("warning: the build cache couldn't be restored: {e:#}"));
                    return keep;
                }
                Err(e) => {
                    report.info(&format!("warning: restoring the build cache stopped: {e}"));
                    return keep;
                }
            }
        }
        // A production build starts only from what production builds kept, however many previews
        // were built: say so, or its full install looks like the cache not working. The
        // dependencies' line says it for `.next/cache` too.
        if !said && plan.kind != Kind::Next {
            report.info(match order.request.target {
                Target::Production => "build cache: none yet from a production build (a preview's isn't used for production); this build keeps one",
                Target::Preview => "build cache: none yet for this project; this build keeps one",
            });
        }
        keep
    }

    /// Write the project's build cache as this build left it. Failing to is a line in the log,
    /// never the build's failure.
    async fn keep(&self, keep: Keep, deadline_ms: u64, report: &Report) {
        let left = Duration::from_millis(deadline_ms.saturating_sub(now_ms()));
        if left < KEEP_NEEDS {
            report.info("build cache: not kept, the builder's time is nearly up");
            return;
        }
        let started = Instant::now();
        let what = kept_name(keep.plan.kind);
        let archive = self.root.join("work").join(format!("{}-out.tar.zst", file_name(keep.plan.kind)));
        let platform = self.platform.clone();
        let kept = async {
            let (plan, to) = (keep.plan.clone(), archive.clone());
            let saved = tokio::task::spawn_blocking(move || {
                let dirs = plan.dirs()?;
                if dirs.is_empty() {
                    return Ok(None);
                }
                let key = plan.holds_actions_key(&dirs);
                cache::save(&to, &plan.manifest(&platform, dirs), &plan.base).map(|counts| Some((counts, key)))
            })
            .await
            .context("packing stopped")??;
            let Some((counts, key)) = saved else { return Ok(None) };
            let size = tokio::fs::metadata(&archive).await?.len();
            if size > cache::MAX_BYTES {
                bail!("{:.0} MB packed is over the {} GB a cache may be", size as f64 / 1e6, cache::MAX_BYTES >> 30);
            }
            self.http.put_file(&keep.url, &archive).await.context("writing it")?;
            Ok::<_, anyhow::Error>(Some((counts, size, key)))
        };
        match tokio::time::timeout(left - KEEP_MARGIN, kept).await {
            Ok(Ok(Some((counts, size, key)))) => report.info(&format!(
                "build cache: {what} kept for the next build ({} files, {:.1} MB, packed to {:.1} MB{}) in {:.1} s",
                counts.files,
                counts.bytes as f64 / 1e6,
                size as f64 / 1e6,
                if key { ", with Next's Server Actions key" } else { "" },
                started.elapsed().as_secs_f64()
            )),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => report.info(&format!("warning: the build cache wasn't kept: {e:#}")),
            Err(_) => report.info("warning: the build cache wasn't kept: the builder's time ran out"),
        }
    }

    async fn order(&self, job: &Job) -> Result<Order> {
        let bytes = self.http.get(&job.order_url).await.context("reading the build's order")?.context("the build's order is gone")?;
        serde_json::from_slice(&bytes).context("the build's order")
    }

    /// Whether the deployment's record says canceled.
    async fn canceled(&self, job: &Job) -> bool {
        match self.http.get(&job.record_url).await {
            Ok(Some(bytes)) => serde_json::from_slice::<DeploymentRecord>(&bytes).is_ok_and(|r| r.status == Status::Canceled),
            _ => false,
        }
    }

    /// Ends once the deployment is canceled.
    async fn watch(&self, job: &Job) {
        loop {
            tokio::time::sleep(CANCEL_POLL).await;
            if self.canceled(job).await {
                return;
            }
        }
    }

    /// Ready the scratch folder for a build of `project` for `target`: emptied unless the last
    /// build here was the same project's for the same target, and every process a build left
    /// stopped. What a build leaves in `home`, `cache` and `tmp` is kept for the next one, so a
    /// preview's (any branch's code, and a developer's) never meets a production build, whose
    /// environment holds production's values; as with the build caches in S3 (cache.rs). A
    /// build whose order couldn't be read (`None`) starts from an empty folder too.
    fn clean(&self, project: &str, target: Option<Target>) -> Result<()> {
        kill_others();
        let mut last = self.last_build.lock().expect("builder lock");
        let this = target.map(|t| (project.to_owned(), t));
        if this.is_none() || *last != this {
            for entry in std::fs::read_dir(&self.root).with_context(|| format!("reading {}", self.root.display()))? {
                let path = entry?.path();
                let removed = if path.is_dir() && !path.is_symlink() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                removed.with_context(|| format!("removing {}", path.display()))?;
            }
            *last = this;
        }
        let _ = std::fs::remove_dir_all(self.root.join("work"));
        for dir in ["work", "home", "cache", "tmp"] {
            std::fs::create_dir_all(self.root.join(dir))?;
        }
        Ok(())
    }
}

/// What a build does about the project's caches once it has built: its dependencies', and a Next
/// build's `.next/cache`.
#[derive(Default)]
struct Keeps {
    deps: Option<Keep>,
    next: Option<Keep>,
}

/// What a build does about one of the project's caches once it has built.
struct Keep {
    plan: Plan,
    /// Where the cache is written.
    url: String,
    /// The cache as it's stored already holds what this build installs: nothing to write.
    current: bool,
}

fn kept_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Node => "node_modules",
        Kind::Python => "Python requirements",
        Kind::Go => "Go's module and build caches",
        Kind::Rust => "Cargo's registry and compiled crates",
        Kind::Next => ".next/cache",
    }
}

/// The name of an archive's files in the scratch folder.
fn file_name(kind: Kind) -> &'static str {
    if kind == Kind::Next { "next-cache" } else { "cache" }
}

/// The first `n` dot-separated parts of a version: `24.22.2` is `24`, or `24.22`.
fn parts(version: &str, n: usize) -> String {
    version.split('.').take(n).collect::<Vec<_>>().join(".")
}

/// Running as a Lambda function on Linux.
fn in_lambda() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("AWS_LAMBDA_RUNTIME_API").is_some() && std::env::var_os("AWS_LAMBDA_FUNCTION_NAME").is_some()
}

/// Stop every process but this one that this user may signal: whatever a build started, however
/// it hid. Only in Lambda, where this user is the builder's alone; anywhere else it would be
/// everything the user runs.
fn kill_others() {
    if in_lambda() {
        buildlog::signal(-1, buildlog::SIGKILL);
    }
}

fn early(failure: Failure, timings: Timings) -> Outcome {
    Outcome { ending: Ending::Failed, error: Some(failure), lines: 0, bytes: 0, cut: false, log_kept: false, timings, out_of_memory: false }
}

/// A command's version, or `?`.
fn version(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "?".to_owned())
}

/// Zip the folder `dir` into `path`, stored: the package and static files are compressed already.
/// The zip's size.
fn zip_folder(dir: &Path, path: &Path) -> Result<u64> {
    let mut zip = ZipWriter::new(std::io::BufWriter::new(std::fs::File::create(path)?));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored).last_modified_time(DateTime::default()).large_file(true);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&next)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let name = path.strip_prefix(dir).expect("under dir").to_string_lossy().replace('\\', "/");
                zip.start_file(name, options)?;
                std::io::copy(&mut std::fs::File::open(&path)?, &mut zip)?;
            }
        }
    }
    zip.finish()?;
    Ok(std::fs::metadata(path)?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_zips_and_unzips_whole() {
        let dir = package::TempDir::new("zip-folder").unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(out.join("static/manifests")).unwrap();
        std::fs::write(out.join("build.json"), b"{}").unwrap();
        std::fs::write(out.join("static/manifests/m.json"), b"[]").unwrap();
        let zip = dir.path().join("out.zip");
        zip_folder(&out, &zip).unwrap();
        let back = dir.path().join("back");
        crate::remote::unzip(&zip, &back).unwrap();
        assert_eq!(std::fs::read(back.join("static/manifests/m.json")).unwrap(), b"[]");
        assert_eq!(std::fs::read(back.join("build.json")).unwrap(), b"{}");
    }

    /// A stand-in for the bucket behind a job's presigned URLs: GET answers what was PUT, or 404.
    /// Also counts the PUTs to each path.
    #[derive(Clone, Default)]
    struct Bucket(Arc<Mutex<std::collections::HashMap<String, (Bytes, u32)>>>);

    impl Bucket {
        async fn serve(&self) -> String {
            use http_body_util::{BodyExt, Full};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let bucket = self.clone();
            tokio::spawn(async move {
                loop {
                    let (tcp, _) = listener.accept().await.unwrap();
                    let bucket = bucket.clone();
                    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                        let bucket = bucket.clone();
                        async move {
                            let path = req.uri().path().to_owned();
                            let answer = if req.method() == hyper::Method::PUT {
                                let body = req.into_body().collect().await.unwrap().to_bytes();
                                let mut objects = bucket.0.lock().unwrap();
                                let puts = objects.get(&path).map_or(0, |(_, n)| *n);
                                objects.insert(path, (body, puts + 1));
                                hyper::Response::new(Full::new(Bytes::new()))
                            } else {
                                match bucket.0.lock().unwrap().get(&path) {
                                    Some((body, _)) => hyper::Response::new(Full::new(body.clone())),
                                    None => hyper::Response::builder().status(404).body(Full::new(Bytes::new())).unwrap(),
                                }
                            };
                            Ok::<_, std::convert::Infallible>(answer)
                        }
                    });
                    tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tcp), service));
                }
            });
            url
        }

        fn puts(&self, path: &str) -> u32 {
            self.0.lock().unwrap().get(path).map_or(0, |(_, n)| *n)
        }
    }

    const NEXT_PACKAGE: &str = r#"{"scripts": {"build": "next build"}, "dependencies": {"next": "16.4.6"}}"#;

    /// A preview's job for project `shop`, its caches at `url`; `next` false for a job from a
    /// forkrail older than the Next archive.
    fn job(url: &str, next: bool) -> Job {
        let (own, production) = (format!("{url}/cache/shop/preview"), format!("{url}/cache/shop/production"));
        let urls = |name: &str| serde_json::json!({"read": [format!("{own}{name}"), format!("{production}{name}")], "write": format!("{own}{name}")});
        let mut job = serde_json::json!({
            "id": "a1b2c3d4", "project": "shop", "key": "k", "order_url": "o", "source_url": "s", "record_url": "r",
            "output_url": "u", "log_url": "l", "sent_ms": 1, "cache": urls(".tar.zst")
        });
        if next {
            job["next_cache"] = urls(".next.tar.zst");
        }
        serde_json::from_value(job).unwrap()
    }

    fn order() -> Order {
        serde_json::from_value(serde_json::json!({"request": {"id": "a1b2c3d4", "project": "shop", "target": "preview", "base_domain": "example.dev"}, "publish": {}})).unwrap()
    }

    /// A fresh checkout of the Next project, as the builder unzips it.
    fn checkout(builder: &Builder) -> PathBuf {
        let _ = std::fs::remove_dir_all(builder.root.join("work"));
        builder.clean("shop", Some(Target::Preview)).unwrap();
        let src = builder.root.join("work/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("package.json"), NEXT_PACKAGE).unwrap();
        src
    }

    /// What `next build` leaves: its dist folder, and in `.next/cache` the files given.
    fn next_build(src: &Path, cache: &[(&str, &str)]) {
        for (name, text) in [("node_modules/next/package.json", "{}"), (".next/BUILD_ID", "b1"), (".next/required-server-files.json", "{}")].iter().chain(cache) {
            let path = src.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_next_build_keeps_its_cache_after_every_build() {
        let tmp = package::TempDir::new("builder-next-cache").unwrap();
        let builder = Builder::new(tmp.path().join("root"), "bootstrap".into(), "python3".into()).unwrap();
        let bucket = Bucket::default();
        let url = bucket.serve().await;
        let order = order();
        let deadline = now_ms() + 15 * 60 * 1000;
        let (deps_path, next_path) = ("/cache/shop/preview.tar.zst", "/cache/shop/preview.next.tar.zst");
        let report = Report::start(None, Mask::default(), Echo::Masked);

        // The first build has nothing to start from, and keeps both.
        let src = checkout(&builder);
        let keeps = builder.restore(&job(&url, true), &order, &src, &report).await;
        next_build(&src, &[(".next/cache/.rscinfo", r#"{"encryption.key":"k1"}"#), (".next/cache/webpack/x.pack", "x")]);
        builder.keep(keeps.next.expect("a Next build keeps its cache"), deadline, &report).await;
        builder.keep(keeps.deps.unwrap(), deadline, &report).await;
        assert_eq!((bucket.puts(deps_path), bucket.puts(next_path)), (1, 1));

        // The second, in a fresh checkout, gets `.next/cache` back before it builds, the key in it,
        // and keeps it again though its dependencies haven't changed.
        let src = checkout(&builder);
        let keeps = builder.restore(&job(&url, true), &order, &src, &report).await;
        assert_eq!(std::fs::read_to_string(src.join(".next/cache/.rscinfo")).unwrap(), r#"{"encryption.key":"k1"}"#);
        assert!(src.join("node_modules/next/package.json").is_file());
        assert!(keeps.deps.as_ref().unwrap().current, "the dependencies' archive is as they are");
        next_build(&src, &[(".next/cache/images/a.webp", "img")]);
        builder.keep(keeps.next.unwrap(), deadline, &report).await;
        assert_eq!((bucket.puts(deps_path), bucket.puts(next_path)), (1, 2));

        // A build that leaves no `.next/cache` writes nothing.
        let src = checkout(&builder);
        let keeps = builder.restore(&job(&url, true), &order, &src, &report).await;
        std::fs::remove_dir_all(src.join(".next/cache")).unwrap();
        next_build(&src, &[]);
        builder.keep(keeps.next.unwrap(), deadline, &report).await;
        assert_eq!(bucket.puts(next_path), 2);

        // Both say what they did, with their sizes and times, as the dependencies' lines do.
        let lines: Vec<String> = report.close().await.lines.into_iter().map(|l| l.text).collect();
        let has = |start: &str, n: usize| lines.iter().filter(|l| l.starts_with(start) && l.contains(" files, ") && l.contains(" MB") && l.ends_with(" s")).count() == n;
        assert!(has("build cache: .next/cache kept for the next build (2 files, 0.0 MB, packed to 0.0 MB, with Next's Server Actions key) in ", 1), "{lines:#?}");
        assert!(has("build cache: .next/cache kept for the next build (3 files, ", 1), "{lines:#?}");
        assert!(has("build cache: .next/cache restored (", 2) && has("build cache: node_modules restored (", 2), "{lines:#?}");
        assert!(has("build cache: node_modules kept for the next build (1 files, ", 1), "{lines:#?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_from_before_the_next_archive_keeps_next_cache_with_the_dependencies() {
        let tmp = package::TempDir::new("builder-next-older").unwrap();
        let builder = Builder::new(tmp.path().join("root"), "bootstrap".into(), "python3".into()).unwrap();
        let bucket = Bucket::default();
        let url = bucket.serve().await;
        let order = order();
        let report = Report::start(None, Mask::default(), Echo::Masked);
        let src = checkout(&builder);
        let keeps = builder.restore(&job(&url, false), &order, &src, &report).await;
        assert!(keeps.next.is_none());
        next_build(&src, &[(".next/cache/.rscinfo", "{}")]);
        builder.keep(keeps.deps.unwrap(), now_ms() + 15 * 60 * 1000, &report).await;
        let src = checkout(&builder);
        builder.restore(&job(&url, false), &order, &src, &report).await;
        assert!(src.join(".next/cache/.rscinfo").is_file() && bucket.puts("/cache/shop/preview.next.tar.zst") == 0);
    }

    /// A stand-in for the dashboard's build-log endpoint: answers how far the log has got (`so_far`,
    /// or 404 without one) and keeps the lines it's sent.
    async fn dashboard(so_far: Option<(u32, u64)>) -> (String, Arc<Mutex<Vec<report::Line>>>) {
        use http_body_util::{BodyExt, Full};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/builds/a1b2c3d4/log", listener.local_addr().unwrap());
        let got: Arc<Mutex<Vec<report::Line>>> = Arc::default();
        let lines = got.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let lines = lines.clone();
                let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let lines = lines.clone();
                    async move {
                        assert_eq!(req.headers()["authorization"], "Bearer k");
                        let answer = if req.method() == hyper::Method::GET {
                            match so_far {
                                Some((seq, bytes)) => hyper::Response::new(Full::new(Bytes::from(serde_json::json!({"seq": seq, "bytes": bytes}).to_string()))),
                                None => hyper::Response::builder().status(404).body(Full::new(Bytes::new())).unwrap(),
                            }
                        } else {
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let batch: serde_json::Value = serde_json::from_slice(&body).unwrap();
                            lines.lock().unwrap().extend(serde_json::from_value::<Vec<report::Line>>(batch["lines"].clone()).unwrap());
                            hyper::Response::new(Full::new(Bytes::new()))
                        };
                        Ok::<_, std::convert::Infallible>(answer)
                    }
                });
                tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tcp), service));
            }
        });
        (url, got)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_build_sent_again_goes_on_after_the_first_builds_lines() {
        let tmp = package::TempDir::new("builder-again").unwrap();
        let builder = Builder::new(tmp.path().join("root"), "bootstrap".into(), "python3".into()).unwrap();
        let bucket = Bucket::default();
        let url = bucket.serve().await;
        bucket.0.lock().unwrap().insert("/o".into(), (Bytes::from(serde_json::to_vec(&order()).unwrap()), 0));
        // No source, so each build fails soon after it starts: its log is what's looked at.
        let run = |endpoint: String, again: bool| {
            let mut job = job(&url, true);
            (job.order_url, job.source_url, job.record_url, job.log_url, job.output_url) = (format!("{url}/o"), format!("{url}/s"), format!("{url}/r"), format!("{url}/l"), format!("{url}/u"));
            job.log_endpoint = Some(endpoint);
            job.again = again.then(|| "The build ran out of the builder's memory: building it again on the large builder".to_owned());
            builder.run(job, now_ms() + 15 * 60 * 1000)
        };

        // The first build wrote 37 lines before it ran out: the second's follow, and its copy in
        // S3 isn't written, since it would hide the first's.
        let (endpoint, lines) = dashboard(Some((37, 1_200))).await;
        let outcome = run(endpoint, true).await;
        let got = lines.lock().unwrap().clone();
        assert_eq!(outcome.ending, Ending::Failed);
        assert_eq!((got[0].seq, got[0].text.as_str()), (38, "The build ran out of the builder's memory: building it again on the large builder"));
        assert!(got.windows(2).all(|w| w[1].seq == w[0].seq + 1), "{got:#?}");
        assert_eq!(outcome.lines, got.last().unwrap().seq);
        assert!(outcome.bytes > 1_200 && !outcome.log_kept && bucket.puts("/l") == 0);
        assert!(!outcome.out_of_memory, "a missing source isn't memory");

        // A dashboard that can't say how far it got: the log starts over and keeps its own copy.
        let (endpoint, lines) = dashboard(None).await;
        let outcome = run(endpoint, true).await;
        assert_eq!(lines.lock().unwrap()[0].seq, 1);
        assert!(outcome.log_kept && bucket.puts("/l") == 1);

        // A first build never asks.
        let (endpoint, lines) = dashboard(Some((37, 1_200))).await;
        run(endpoint, false).await;
        assert_eq!(lines.lock().unwrap()[0].seq, 1);
    }

    #[test]
    fn a_preview_leaves_nothing_for_production() {
        let dir = package::TempDir::new("scratch-targets").unwrap();
        let builder = Builder::new(dir.path().join("root"), "bootstrap".into(), "python3".into()).unwrap();
        let planted = builder.root.join("home/.npmrc");
        builder.clean("shop", Some(Target::Preview)).unwrap();
        std::fs::write(&planted, "registry=https://elsewhere.example/").unwrap();
        builder.clean("shop", Some(Target::Preview)).unwrap();
        assert!(planted.exists(), "kept for the next build of the same project and target");
        builder.clean("shop", Some(Target::Production)).unwrap();
        assert!(!planted.exists(), "a production build starts from an empty folder");
        std::fs::write(&planted, "x").unwrap();
        builder.clean("shop", None).unwrap();
        assert!(!planted.exists(), "so does one whose order couldn't be read");
        std::fs::write(&planted, "x").unwrap();
        builder.clean("shop", Some(Target::Production)).unwrap();
        assert!(!planted.exists(), "and the one after it");
        builder.clean("shop", Some(Target::Production)).unwrap();
        assert!(builder.root.join("home").is_dir() && builder.root.join("work").is_dir());
    }

    #[test]
    fn tmp_is_refused_outside_lambda() {
        if !in_lambda() {
            let err = Builder::new("/tmp".into(), "bootstrap".into(), "python3".into()).err().unwrap();
            assert!(err.to_string().contains(ROOT_ENV));
        }
    }
}
