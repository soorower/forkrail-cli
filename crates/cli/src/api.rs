//! The CLI of someone signed in (`forkrail login`, login.rs): its commands through the install's
//! API, which the dashboard serves at `/api/v1` (web/src/server/api-v1.ts), with no AWS credentials
//! on this machine. The API checks the person's role for each thing asked and has the install's
//! control function do it under their name.
//!
//! A deploy: the project's files are zipped here as for any remote build, the API hands back an
//! address in S3 signed for that one zip, the zip goes straight there, and the build runs in the
//! install's builder, its log shown here as it's written.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use hyper::Method;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::buildlog::{Stream, print_line};
use crate::deploy::{self, check_project, log};
use crate::fetch::Http;
use crate::git;
use crate::login::Saved;
use crate::package::TempDir;
use crate::remote::{RemoteDeploy, UploadUrl};
use crate::report::Line;
use crate::source;
use crate::env::Pulled;
use crate::project::Change;
use crate::state::{self, DeploymentRecord, EnvNames, EnvTarget, Failure, Status, Target};
use crate::vercel;

/// How often a build's log is asked for while it's followed.
const FOLLOW_EVERY: Duration = Duration::from_millis(700);
/// A read is asked again this many times when the dashboard can't be reached or is restarting.
const READ_TRIES: u32 = 3;

pub struct Client {
    http: Http,
    /// `https://dashboard.example.com`.
    base: String,
    authorization: String,
}

/// What the API said when it refused: its status, its own words, and the rest of its answer.
#[derive(Debug)]
pub struct Refused {
    pub status: u16,
    pub code: Option<String>,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

impl Client {
    pub fn new(saved: &Saved) -> Result<Self> {
        Ok(Self { http: Http::new()?, base: saved.api.trim_end_matches('/').to_owned(), authorization: format!("Bearer {}", saved.token) })
    }

    /// The dashboard's address.
    pub fn base(&self) -> &str {
        &self.base
    }

    async fn once<T: DeserializeOwned>(&self, method: Method, path: &str, body: Option<&Value>) -> Result<T> {
        let url = format!("{}/api/v1/{path}", self.base);
        let headers = [("authorization", self.authorization.as_str()), ("content-type", "application/json"), ("accept", "application/json")];
        let (status, bytes) = self.http.exchange(method, &url, &headers, body.map(|b| Bytes::from(b.to_string()))).await.with_context(|| format!("reaching {}", self.base))?;
        if (200..300).contains(&status) {
            return serde_json::from_slice(&bytes).with_context(|| format!("{url} answered something this forkrail can't read: is one of them older?"));
        }
        let answer: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let message = match answer.get("error").and_then(Value::as_str) {
            Some(error) => error.to_owned(),
            // Not the API's own answer: the router's, when the dashboard is down or restarting.
            None => format!("{} answered HTTP {status}", self.base),
        };
        Err(Refused { status, code: answer.get("code").and_then(Value::as_str).map(str::to_owned), message }.into())
    }

    /// Read something: asked again if the dashboard couldn't be reached or answered 502 to 504.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.read(Method::GET, path, None).await
    }

    /// Read something whose question is too long for an address (a query and its parameters):
    /// a POST that changes nothing, so it's asked again like a GET.
    pub async fn ask<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T> {
        self.read(Method::POST, path, Some(&body)).await
    }

    async fn read<T: DeserializeOwned>(&self, method: Method, path: &str, body: Option<&Value>) -> Result<T> {
        let mut tried = 0;
        loop {
            tried += 1;
            match self.once(method.clone(), path, body).await {
                Ok(value) => return Ok(value),
                Err(e) => {
                    let passing = match e.downcast_ref::<Refused>() {
                        Some(refused) => matches!(refused.status, 502..=504),
                        None => true,
                    };
                    if !passing || tried >= READ_TRIES {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_millis(400 * u64::from(tried))).await;
                }
            }
        }
    }

    /// Ask for a change, once: one that may have happened isn't asked for again.
    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T> {
        self.once(Method::POST, path, Some(&body)).await
    }
}

/* ── who, and what there is ──────────────────────────────────── */

#[derive(Deserialize)]
struct Me {
    login: String,
    name: String,
    role: String,
    team: String,
    #[serde(default)]
    machine: String,
}

/// `forkrail whoami`.
pub async fn whoami(client: &Client) -> Result<()> {
    let me: Me = client.get("whoami").await?;
    let name = if me.name.is_empty() || me.name == me.login { String::new() } else { format!(" ({})", me.name) };
    println!("{}{name}, {} of {}", me.login, me.role, me.team);
    println!("signed in to {} from {}", client.base, if me.machine.is_empty() { "this terminal" } else { me.machine.as_str() });
    Ok(())
}

#[derive(Deserialize)]
struct Projects {
    projects: Vec<ProjectRow>,
}

#[derive(Deserialize)]
struct ProjectRow {
    name: String,
    production: Option<String>,
    deployments: usize,
    url: Option<String>,
}

#[derive(Deserialize)]
struct Deployments {
    /// The deployment serving production; none before the first.
    #[serde(default)]
    production: Option<String>,
    deployments: Vec<DeploymentRow>,
}

#[derive(Deserialize)]
struct DeploymentRow {
    id: String,
    target: Target,
    status: Status,
    created: String,
    #[serde(default)]
    version: String,
    url: String,
    #[serde(default)]
    production: bool,
}

/// `forkrail ls [PROJECT]`, as it prints with AWS credentials.
pub async fn ls(client: &Client, project: Option<&str>) -> Result<()> {
    let Some(project) = project else {
        let list: Projects = client.get("projects").await?;
        if list.projects.is_empty() {
            println!("no projects yet");
            return Ok(());
        }
        println!("{:<24} {:<10} {:>11}  URL", "PROJECT", "PRODUCTION", "DEPLOYMENTS");
        for p in list.projects {
            println!("{:<24} {:<10} {:>11}  {}", p.name, p.production.as_deref().unwrap_or("-"), p.deployments, p.url.unwrap_or_default());
        }
        return Ok(());
    };
    check_project(project)?;
    let list: Deployments = client.get(&format!("projects/{project}/deployments")).await?;
    println!("{:<10} {:<11} {:<9} {:<21} {:>7}  URL", "ID", "TARGET", "STATUS", "CREATED", "VERSION");
    for d in list.deployments {
        // Only a published deployment answers on its URL.
        if d.status != Status::Ready {
            println!("{:<10} {:<11} {:<9} {:<21} {:>7}  -", d.id, d.target.as_str(), d.status.as_str(), d.created, "-");
            continue;
        }
        let version = if d.version.is_empty() { "-" } else { d.version.as_str() };
        let production = if d.production { "  (production)" } else { "" };
        println!("{:<10} {:<11} {:<9} {:<21} {:>7}  {}{production}", d.id, d.target.as_str(), d.status.as_str(), d.created, version, d.url);
    }
    Ok(())
}

#[derive(Deserialize)]
struct Shown {
    record: Value,
    status: Status,
    #[serde(default)]
    serving: Vec<String>,
}

/// `forkrail inspect ID`.
pub async fn inspect(client: &Client, id: &str) -> Result<()> {
    let shown: Shown = client.get(&format!("deployments/{}", checked_id(id)?)).await?;
    // As this forkrail writes a record, so it reads like `inspect` with AWS credentials; one from
    // a newer forkrail that this one can't read is shown as it came.
    match serde_json::from_value::<DeploymentRecord>(shown.record.clone()) {
        Ok(record) => println!("{}", serde_json::to_string_pretty(&record)?),
        Err(_) => println!("{}", serde_json::to_string_pretty(&shown.record)?),
    }
    if shown.record.get("status").and_then(Value::as_str).is_some_and(|recorded| recorded != shown.status.as_str()) {
        println!("status now: {} ({})", shown.status.as_str(), state::STOPPED_REPORTING);
    }
    println!("serving: {}", if shown.serving.is_empty() { "nothing".to_owned() } else { shown.serving.join(", ") });
    Ok(())
}

/// A page of a deployment's build log, and how the deployment stands.
#[derive(Deserialize)]
struct LogPage {
    lines: Vec<Line>,
    #[serde(default)]
    more: bool,
    /// Every line is here: nothing more will come.
    #[serde(default)]
    complete: bool,
    /// Read from the copy kept when the build ended, which has every line.
    #[serde(default)]
    kept: bool,
    deployment: Standing,
}

#[derive(Deserialize)]
struct Standing {
    status: Status,
    /// Older dashboards don't say.
    #[serde(default)]
    target: Option<Target>,
    #[serde(default)]
    urls: Vec<String>,
    #[serde(default)]
    error: Option<Failure>,
    #[serde(default)]
    building_at: Option<String>,
    #[serde(default)]
    ended_at: Option<String>,
    #[serde(default)]
    has_log: bool,
}

/// `forkrail inspect ID --logs`: a deployment's build log, once it has ended.
pub async fn inspect_logs(client: &Client, id: &str) -> Result<()> {
    let id = checked_id(id)?;
    let mut lines: Vec<Line> = Vec::new();
    let standing = loop {
        let after = lines.last().map_or(0, |l| l.seq);
        let page: LogPage = client.get(&format!("deployments/{id}/log?after={after}")).await?;
        if page.deployment.status.in_progress() {
            bail!("deployment {id} is still building; the dashboard shows its log as it runs");
        }
        if !page.deployment.has_log {
            bail!("deployment {id} has no build log: it was made before forkrail kept them");
        }
        let got = page.lines.len();
        lines.extend(page.lines);
        if !page.more || got == 0 {
            break page.deployment;
        }
    };
    let start = lines.first().map_or(0, |l| l.t);
    for line in &lines {
        let at = line.t.saturating_sub(start) as f64 / 1000.0;
        match line.stream.as_str() {
            "step" => println!("{at:>7.1}s  == {}", crate::buildlog::printable(&line.text)),
            "forkrail" => println!("{at:>7.1}s  ==> {}", crate::buildlog::printable(&line.text)),
            _ => println!("{at:>7.1}s  {}", crate::buildlog::printable(&line.text)),
        }
    }
    if let Some(failure) = &standing.error {
        println!("error{}: {}", failure.step.as_deref().map(|s| format!(" in {s}")).unwrap_or_default(), failure.message);
    }
    Ok(())
}

fn checked_id(id: &str) -> Result<&str> {
    if id.len() != 8 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        bail!("{id:?} isn't a deployment id (8 hex digits, as `forkrail ls PROJECT` lists them)");
    }
    Ok(id)
}

/* ── deploying ───────────────────────────────────────────────── */

/// A build the API started.
#[derive(Deserialize)]
struct Started {
    id: String,
    project: String,
    #[serde(default)]
    dashboard: Option<String>,
    /// A redeploy that published a kept build again has no build to follow.
    #[serde(default)]
    building: bool,
}

/// `forkrail deploy` for someone signed in: upload the project in `dir`, build it in the install's
/// builder and show its log until it ends.
pub async fn deploy(client: &Client, args: RemoteDeploy) -> Result<()> {
    let dir = args.dir.canonicalize().with_context(|| format!("{}", args.dir.display()))?;
    // Read here too, so a vercel.json the builder would refuse fails before anything's uploaded.
    let spec = vercel::read(&dir)?;
    let project = match args.project {
        Some(name) => name,
        None => deploy::project_from_dir(&dir)?,
    };
    check_project(&project)?;
    if args.skip_domain && !args.prod {
        bail!("--skip-domain is for production deployments");
    }
    let target = if args.prod { Target::Production } else { Target::Preview };
    log(&format!("{project}: {}, {}, built in the install's builder", spec.describe(), target.as_str()));
    let upload = upload(client, &dir, &project).await?;
    let started: Started = client
        .post(
            "deployments",
            json!({
                "project": project,
                "production": args.prod,
                "upload": upload,
                "git": git::commit(&dir),
                "memory": args.memory,
                // Under the key every dashboard takes; a newer one also takes `concurrent`.
                "fluid": args.concurrent,
                "skip_domain": args.skip_domain,
                "same_source": args.same_source,
                "no_cache": args.no_cache,
            }),
        )
        .await?;
    watch(client, &started).await
}

/// Zip the project in `dir` and put it at the address the API signs for it, unless the same source
/// is there already.
async fn upload(client: &Client, dir: &Path, project: &str) -> Result<source::Archive> {
    let tmp = TempDir::new("upload")?;
    let path = tmp.path().join("source.zip");
    let archive = {
        let (dir, path) = (dir.to_path_buf(), path.clone());
        tokio::task::spawn_blocking(move || source::upload_archive(&dir, &path)).await.context("zipping the source")??
    };
    let zipped = std::fs::metadata(&path)?.len();
    let size = format!("{} files ({:.1} MB)", archive.files, zipped as f64 / 1e6);
    let place: UploadUrl = client.post("uploads", json!({ "project": project, "sha256": archive.sha256, "bytes": zipped })).await?;
    if place.exists {
        log(&format!("source: {size}, the same as uploaded before"));
        return Ok(archive);
    }
    let url = place.url.context("the API gave no address to upload to")?;
    let started = Instant::now();
    let body = Bytes::from(tokio::fs::read(&path).await.context("reading the zipped source")?);
    // The length goes with the body; the other headers are what the address was signed with.
    let headers: Vec<(&str, &str)> = place.headers.iter().filter(|(name, _)| !name.eq_ignore_ascii_case("content-length")).map(|(n, v)| (n.as_str(), v.as_str())).collect();
    client.http.call(Method::PUT, &url, &headers, Some(body)).await.context("uploading the source")?;
    log(&format!("source: {size} uploaded in {:.1} s", started.elapsed().as_secs_f64()));
    Ok(archive)
}

/// Say where a build can be seen, show its log until it ends, and say how it ended: its URLs on
/// stdout once ready, an error if it failed or was canceled.
async fn watch(client: &Client, started: &Started) -> Result<()> {
    let id = &started.id;
    match &started.dashboard {
        Some(page) => log(&format!("deployment {id} is building: {page}")),
        None => log(&format!("deployment {id} is building")),
    }
    let Some(standing) = follow(client, id).await? else { return Ok(()) };
    match standing.status {
        Status::Ready => {
            let took = match (standing.building_at.as_deref().and_then(state::parse_rfc3339_ms), standing.ended_at.as_deref().and_then(state::parse_rfc3339_ms)) {
                (Some(from), Some(to)) => format!(" in {:.1} s", to.saturating_sub(from) as f64 / 1000.0),
                _ => String::new(),
            };
            log(&format!("deployment {id} is ready{took}"));
            for url in &standing.urls {
                println!("{url}");
            }
            if standing.target == Some(Target::Preview)
                && let Ok(list) = client.get::<Deployments>(&format!("projects/{}/deployments", started.project)).await
            {
                log(&deploy::preview_ready_note(&started.project, id, list.production.is_some()));
            }
            Ok(())
        }
        Status::Error => {
            let error = standing.error.as_ref();
            let step = error.and_then(|e| e.step.as_deref()).map(|s| format!(" ({})", s.to_lowercase())).unwrap_or_default();
            bail!("deployment {id} failed{step}: {}", error.map_or("no reason recorded", |e| e.message.as_str()))
        }
        Status::Canceled => bail!("deployment {id} was canceled"),
        Status::Building | Status::Queued => Ok(()),
    }
}

/// Show a build's log as it's written until the deployment ends; how it stands then. `None` after
/// Ctrl-C, which stops the showing, not the build.
async fn follow(client: &Client, id: &str) -> Result<Option<Standing>> {
    let mut after = 0u32;
    let mut ended: Option<Instant> = None;
    let mut failures = 0u32;
    let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
    loop {
        match client.get::<LogPage>(&format!("deployments/{id}/log?after={after}")).await {
            Ok(page) => {
                failures = 0;
                for line in &page.lines {
                    if line.seq <= after {
                        continue;
                    }
                    // While it's written, in order only: a line that's late comes with the next
                    // answer, or from the kept copy at the end, which has them all.
                    if !page.kept && line.seq != after + 1 {
                        break;
                    }
                    print_line(Stream::parse(&line.stream), &crate::buildlog::printable(&line.text));
                    after = line.seq;
                }
                if page.more {
                    continue;
                }
                let status = page.deployment.status;
                if !status.in_progress() {
                    // Its last lines and its kept copy come a moment after it's recorded as ended.
                    let since = ended.get_or_insert_with(Instant::now).elapsed();
                    let all_shown = page.kept && page.complete;
                    if all_shown || (status == Status::Canceled && since > Duration::from_secs(2)) || since > Duration::from_secs(20) {
                        return Ok(Some(page.deployment));
                    }
                }
            }
            Err(e) => {
                failures += 1;
                if failures >= 5 || e.downcast_ref::<Refused>().is_some_and(|r| r.status < 500) {
                    return Err(e.context(format!("following deployment {id}, which goes on building; `forkrail inspect {id}` shows how it ends")));
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(FOLLOW_EVERY) => {}
            _ = &mut ctrl_c => {
                eprintln!("\nstopped showing the log; deployment {id} goes on building (`forkrail cancel {id}` stops it)");
                return Ok(None);
            }
        }
    }
}

/* ── changes ─────────────────────────────────────────────────── */

/// `forkrail promote ID`.
pub async fn promote(client: &Client, id: &str, allow_preview_env: bool) -> Result<()> {
    let id = checked_id(id)?;
    let mode = if allow_preview_env { "keep" } else { "refuse" };
    // A preview whose env vars differ from production's is refused in words that say what to do.
    let done: Value = client.post(&format!("deployments/{id}/promote"), json!({ "preview_env": mode })).await?;
    let text = |key: &str| done.get(key).and_then(Value::as_str).unwrap_or("?").to_owned();
    match done.get("kind").and_then(Value::as_str) {
        Some("republished") => {
            log(&format!("{}: published again for production as deployment {}", text("project"), text("id")));
            print_urls(&done);
        }
        _ => log(&format!("{}: production is deployment {}", text("project"), text("id"))),
    }
    Ok(())
}

fn print_urls(answer: &Value) {
    for url in answer.get("urls").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        println!("{url}");
    }
}

/// `forkrail rollback [PROJECT] [--to ID]`.
pub async fn rollback(client: &Client, project: &str, to: Option<&str>) -> Result<()> {
    check_project(project)?;
    let to = to.map(checked_id).transpose()?;
    let done: Value = client.post(&format!("projects/{project}/rollback"), json!({ "to": to })).await?;
    log(&format!("{project}: production is back at deployment {}", done.get("production").and_then(Value::as_str).unwrap_or("?")));
    Ok(())
}

/// `forkrail redeploy ID`.
pub async fn redeploy(client: &Client, id: &str, prod: bool, skip_domain: bool, no_cache: bool) -> Result<()> {
    let id = checked_id(id)?;
    let body = json!({ "production": prod, "skip_domain": skip_domain, "no_cache": no_cache });
    let (started, answer): (Started, Value) = {
        let answer: Value = client.post(&format!("deployments/{id}/redeploy"), body).await?;
        (serde_json::from_value(answer.clone()).context("reading the API's answer")?, answer)
    };
    if started.building {
        return watch(client, &started).await;
    }
    log(&format!("{}: deployment {id} published again as {}", started.project, started.id));
    print_urls(&answer);
    Ok(())
}

/// `forkrail cancel ID`.
pub async fn cancel(client: &Client, id: &str) -> Result<()> {
    let id = checked_id(id)?;
    let done: Value = client.post(&format!("deployments/{id}/cancel"), json!({})).await?;
    eprintln!("==> {}: deployment {id} canceled; the builder stops it within seconds", done.get("project").and_then(Value::as_str).unwrap_or("?"));
    Ok(())
}

/// `forkrail rm ID`.
pub async fn remove(client: &Client, id: &str) -> Result<()> {
    let id = checked_id(id)?;
    let done: Value = client.post(&format!("deployments/{id}/remove"), json!({})).await?;
    log(&format!("{}: deleted {id}; `forkrail restore {id}` brings it back for a while", done.get("project").and_then(Value::as_str).unwrap_or("?")));
    Ok(())
}

/// "Auto-assign custom domains", as `forkrail project` shows it.
pub fn auto_assign_words(on: bool) -> &'static str {
    if on { "on (production deployments go live as they're published)" } else { "off (production deployments wait for `forkrail promote`)" }
}

/// What turning auto-assign on or off does.
pub fn auto_assign_note(on: bool) -> String {
    if on {
        "production deployments go live as they're published again".to_owned()
    } else {
        "production deployments now wait: each is published at its own address, and `forkrail promote ID` puts it live".to_owned()
    }
}

/// `forkrail rm --project NAME`.
pub async fn delete_project(client: &Client, project: &str) -> Result<()> {
    check_project(project)?;
    let done: crate::project::Deleted = client.post(&format!("projects/{project}/delete"), json!({ "confirm": project })).await?;
    log(&format!(
        "deleted {project}: {} deployments, {} domains, its function in {}",
        done.deployments,
        done.domains.len(),
        if done.functions.is_empty() { "no region".to_owned() } else { done.functions.join(", ") }
    ));
    Ok(())
}

/* ── env vars and project settings ───────────────────────────── */

/// `forkrail env ls`: each environment's names.
pub async fn env_names(client: &Client, project: &str) -> Result<EnvNames> {
    check_project(project)?;
    #[derive(Deserialize)]
    struct Listed {
        env: EnvNames,
    }
    Ok(client.get::<Listed>(&format!("projects/{project}/env")).await?.env)
}

/// Where an env change or pull goes, as the API takes it: `targets` (or `target`), or a Git
/// branch's previews, which names no target, so that a dashboard from before branches refuses it
/// rather than change preview's own values.
fn env_scope(mut body: Value, field: &str, targets: Value, branch: Option<&str>) -> Value {
    match branch {
        Some(branch) => body["branch"] = json!(branch),
        None => body[field] = targets,
    }
    body
}

/// A dashboard from before branches refuses a branch's request for want of a target: say why.
fn older_dashboard(client: &Client, branch: Option<&str>, e: anyhow::Error) -> anyhow::Error {
    match e.downcast_ref::<Refused>() {
        Some(refused) if branch.is_some() && refused.status == 400 && refused.message.starts_with("target") => {
            anyhow::anyhow!("{} doesn't take a Git branch's env vars yet: it needs a newer dashboard", client.base())
        }
        _ => e,
    }
}

/// `forkrail env add`.
pub async fn env_set(client: &Client, project: &str, values: &BTreeMap<String, String>, targets: &[EnvTarget], branch: Option<&str>) -> Result<()> {
    check_project(project)?;
    let body = env_scope(json!({ "op": "set", "values": values }), "targets", json!(targets), branch);
    let _: Value = client.post(&format!("projects/{project}/env"), body).await.map_err(|e| older_dashboard(client, branch, e))?;
    Ok(())
}

/// `forkrail env rm`: whether it was set for any of `targets` (or for the branch).
pub async fn env_remove(client: &Client, project: &str, key: &str, targets: &[EnvTarget], branch: Option<&str>) -> Result<bool> {
    check_project(project)?;
    let body = env_scope(json!({ "op": "remove", "key": key }), "targets", json!(targets), branch);
    let done: Value = client.post(&format!("projects/{project}/env"), body).await.map_err(|e| older_dashboard(client, branch, e))?;
    Ok(done.get("removed").and_then(Value::as_bool).unwrap_or(false))
}

/// `forkrail env pull` and `run`: Development's values, or another environment's names; with a
/// branch, the names its previews get.
pub async fn env_pull(client: &Client, project: &str, target: EnvTarget, branch: Option<&str>) -> Result<Pulled> {
    check_project(project)?;
    let body = env_scope(json!({}), "target", json!(target), branch);
    client.ask(&format!("projects/{project}/env/pull"), body).await.map_err(|e| older_dashboard(client, branch, e))
}

/// A key that's there, as `Some` of its value (`Some(None)` for null); absent stays `None`.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// A project's settings, as the API shows them.
#[derive(Deserialize)]
struct ProjectSettings {
    memory_mb: Option<u32>,
    max_duration_s: Option<u64>,
    /// Concurrent compute, under the key every dashboard answers with.
    #[serde(rename = "fluid")]
    concurrent: Option<bool>,
    /// With concurrent compute on, requests per worker; absent (`None`) from a dashboard older
    /// than the setting, `Some(None)` for the router's default.
    #[serde(default, deserialize_with = "present")]
    max_inflight: Option<Option<u32>>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    egress: Vec<String>,
    #[serde(default)]
    framework: Option<String>,
    /// Absent from a dashboard older than the setting: on.
    #[serde(default)]
    auto_assign_domains: Option<bool>,
    /// Absent from a dashboard older than skew protection.
    #[serde(default)]
    skew: Option<crate::skew::View>,
    /// Absent from a dashboard older than keep-warm.
    #[serde(default)]
    keep_warm: Option<crate::warm::KeepView>,
    /// Absent from a dashboard older than Web Analytics.
    #[serde(default)]
    analytics: Option<bool>,
    /// The regions its functions run in, empty for the default; absent from a dashboard older
    /// than function regions.
    #[serde(default)]
    regions: Option<Vec<String>>,
    /// The install's home region, where functions run by default.
    #[serde(default)]
    home_region: Option<String>,
}

/// What a rename answers (control's `project_rename`).
#[derive(Debug, Deserialize)]
pub struct Renamed {
    pub project: String,
    pub name: String,
    pub previous: String,
}

/// `forkrail project NAME --rename NEW`, through the dashboard, which takes either name.
pub async fn rename_project(client: &Client, project: &str, name: &str) -> Result<Renamed> {
    check_project(project)?;
    check_project(name.trim())?;
    client.post(&format!("projects/{project}/rename"), json!({ "name": name.trim() })).await
}

/// `forkrail project NAME [--memory …]`: the function settings change; a role and egress targets
/// stay the install's administrator's (control never names a role for a function).
pub async fn project_settings(client: &Client, project: &str, change: &Change) -> Result<()> {
    check_project(project)?;
    if change.role.is_some() || change.egress.is_some() {
        bail!("a project's IAM role and egress targets are the install's administrator's to change, with its AWS credentials: add --aws");
    }
    change.check()?;
    let path = format!("projects/{project}/settings");
    // A change is answered with the settings as they are after it.
    let s: ProjectSettings = if change.is_empty() {
        client.get(&path).await?
    } else {
        let mut body = serde_json::Map::new();
        // Present as null: back to the default; absent: as it is.
        if let Some(v) = change.memory_mb {
            body.insert("memory_mb".into(), json!(v));
        }
        if let Some(v) = change.max_duration_s {
            body.insert("max_duration_s".into(), json!(v));
        }
        if let Some(v) = change.concurrent {
            body.insert("fluid".into(), json!(v));
        }
        if let Some(v) = change.max_inflight {
            body.insert("max_inflight".into(), json!(v));
        }
        if let Some(v) = &change.framework {
            body.insert("framework".into(), json!(v));
        }
        if let Some(v) = change.auto_assign_domains {
            body.insert("auto_assign_domains".into(), json!(v));
        }
        // Skew protection, as control's `settings` names it.
        let skew = &change.skew;
        if let Some(v) = skew.enabled {
            body.insert("skew_protection".into(), json!(v));
        }
        if let Some(v) = skew.max_age_s {
            body.insert("skew_max_age_s".into(), json!(v));
        }
        if let Some(v) = &skew.threshold {
            body.insert("skew_threshold".into(), json!(v));
        }
        if let Some(v) = &skew.allowed_domains {
            body.insert("skew_allowed_domains".into(), json!(v));
        }
        // Keep-warm, as control's `settings` names it: a number, "auto", or null for the default.
        if let Some(v) = change.keep_warm {
            body.insert("keep_warm".into(), json!(v));
        }
        if let Some(v) = change.analytics {
            body.insert("analytics".into(), json!(v));
        }
        // By the names given (AWS's or Vercel's); null for the default.
        if let Some(v) = &change.regions {
            body.insert("regions".into(), json!(v));
        }
        client.post(&path, Value::Object(body)).await?
    };
    if change.regions.is_some() && s.regions.is_none() {
        bail!("this dashboard doesn't know function regions yet, so they didn't change: it needs a newer release, or add --aws");
    }
    if change.max_inflight.is_some() && s.max_inflight.is_none() {
        bail!("this dashboard doesn't know requests per worker yet, so they didn't change: it needs a newer release, or add --aws");
    }
    if change.keep_warm.is_some() && s.keep_warm.is_none() {
        bail!("this dashboard doesn't know keep-warm yet, so nothing of it changed: it needs a newer release, or add --aws");
    }
    if change.analytics.is_some() && s.analytics.is_none() {
        bail!("this dashboard doesn't know Web Analytics yet, so nothing of it changed: it needs a newer release, or add --aws");
    }
    if !change.skew.is_empty() && s.skew.is_none() {
        bail!("this dashboard doesn't know skew protection yet, so nothing of it changed: it needs a newer release, or add --aws");
    }
    let or_default = |v: Option<String>, default: &str| v.unwrap_or_else(|| format!("default ({default})"));
    println!("memory:       {}", or_default(s.memory_mb.map(|m| format!("{m} MB")), "640 MB"));
    println!("maxDuration:  {}", or_default(s.max_duration_s.map(|s| format!("{s} s")), "300 s"));
    println!("concurrent:   {}", or_default(s.concurrent.map(|f| f.to_string()), "true"));
    if let Some(max_inflight) = s.max_inflight {
        println!("per worker:   {}", or_default(max_inflight.map(|n| format!("up to {n} requests at once")), "up to 10"));
    }
    if let Some(regions) = &s.regions {
        let home = s.home_region.clone().unwrap_or_else(|| forkrail_common::install::DEFAULT_HOME_REGION.to_owned());
        println!("regions:      {}", if regions.is_empty() { format!("default (where production runs; {} for a new project)", crate::regions::describe(&[home])) } else { crate::regions::describe(regions) });
    }
    println!("role:         {}", s.role.unwrap_or_else(|| "default (forkrail-worker)".to_owned()));
    println!("egress:       {}", if s.egress.is_empty() { "none".to_owned() } else { s.egress.join(", ") });
    println!("framework:    {}", s.framework.as_deref().unwrap_or("auto (found from the project's files)"));
    println!("auto-assign:  {}", auto_assign_words(s.auto_assign_domains.unwrap_or(true)));
    if let Some(skew) = &s.skew {
        println!("skew:         {}", crate::skew::describe(skew));
    }
    if let Some(keep) = &s.keep_warm {
        println!("keep warm:    {}", crate::warm::keep_words(keep.environments));
    }
    if let Some(on) = s.analytics {
        println!("analytics:    {}", crate::web_analytics::words(on));
    }
    if change.analytics.is_some() {
        log(crate::web_analytics::NOTE);
        if change.only_analytics() {
            return Ok(());
        }
    }
    if change.only_skew() {
        log("skew protection applies at once; builds and apps are told it's on (VERCEL_SKEW_PROTECTION_ENABLED) from the next deploy");
        return Ok(());
    }
    if change.only_keep_warm() {
        log(crate::warm::KEEP_NOTE);
        return Ok(());
    }
    if let Some(max_inflight) = change.max_inflight {
        log(&format!("{project}: deployments running concurrent compute take {}; the routers apply it within seconds", crate::project::inflight_words(max_inflight)));
        if change.only_max_inflight() {
            return Ok(());
        }
    }
    if change.auto_assign_domains.is_some() && change.framework.is_none() && change.memory_mb.is_none() && change.max_duration_s.is_none() && change.concurrent.is_none() && change.regions.is_none() {
        log(&auto_assign_note(s.auto_assign_domains.unwrap_or(true)));
        return Ok(());
    }
    if !change.is_empty() {
        log("takes effect on the next deploy or redeploy; a deployment's vercel.json wins over memory, maxDuration, concurrent compute (its `fluid`) and regions");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_read() {
        let page: LogPage = serde_json::from_value(json!({
            "lines": [{"seq": 1, "t": 1_700_000_000_000u64, "stream": "step", "text": "Building"}],
            "more": false, "complete": false, "kept": false, "status": "building",
            "deployment": {"id": "a1b2c3d4", "project": "shop", "target": "preview", "status": "building", "urls": [], "error": null, "building_at": "2026-11-12T10:00:00.000Z", "ended_at": null, "has_log": true},
        }))
        .unwrap();
        assert_eq!((page.lines.len(), page.deployment.status, page.deployment.has_log), (1, Status::Building, true));
        let failed: Standing = serde_json::from_value(json!({"status": "error", "error": {"step": "Building", "message": "exit 1"}})).unwrap();
        assert_eq!(failed.error.unwrap().step.as_deref(), Some("Building"));
        let started: Started = serde_json::from_value(json!({"id": "a1b2c3d4", "project": "shop", "target": "preview", "urls": ["https://shop-a1b2c3d4.example.dev"], "building": true, "dashboard": "https://d/projects/shop/deployments/a1b2c3d4"})).unwrap();
        assert!(started.building && started.dashboard.is_some());
        let place: UploadUrl = serde_json::from_value(json!({"exists": false, "url": "https://s3/x", "headers": [["content-length", "10"], ["x-amz-checksum-sha256", "abc="]]})).unwrap();
        assert_eq!(place.headers.len(), 2);
        assert!(serde_json::from_value::<UploadUrl>(json!({"exists": true})).unwrap().url.is_none());
    }

    #[test]
    fn a_branch_names_no_target() {
        let set = |branch| env_scope(json!({"op": "set"}), "targets", json!([EnvTarget::Preview]), branch);
        assert_eq!(set(None), json!({"op": "set", "targets": ["preview"]}));
        assert_eq!(set(Some("feature/logs")), json!({"op": "set", "branch": "feature/logs"}));
        assert_eq!(env_scope(json!({}), "target", json!(EnvTarget::Preview), Some("b")), json!({"branch": "b"}));
    }

    #[test]
    fn ids_are_checked_before_they_reach_a_url() {
        assert!(checked_id("a1b2c3d4").is_ok());
        for bad in ["", "a1b2c3d", "../whoami", "A1B2C3D4", "a1b2c3d4/log"] {
            assert!(checked_id(bad).is_err(), "{bad}");
        }
    }
}
