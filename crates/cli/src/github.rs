//! The install's GitHub App, and who owns the install.
//!
//! One app per install does two jobs: people sign in to the dashboard with it ("Continue with
//! GitHub"), and later it connects repositories for push deploys. `forkrail git setup` makes it
//! through GitHub's manifest flow: a page on this machine sends the browser to GitHub with the
//! whole app described, the person clicks Create, and GitHub sends the browser back here with a
//! code that is exchanged once for the app's id and keys. Nothing is typed, and no key is shown.
//!
//! Where the keys go (SSM, home region; names in `forkrail_common::install`):
//! - the client secret and the webhook secret: SecureStrings under `/forkrail/web/`, which the
//!   dashboard's role may decrypt and no other role of the install;
//! - the private key, which can ask GitHub for repository tokens: a SecureString outside that
//!   prefix, so the dashboard can't read it;
//! - the app itself (id, client id, addresses) and the install's owners: plain JSON.
//!
//! The owners are GitHub accounts that can always sign in to the dashboard as owners. Only someone
//! with the AWS account can change the list (`forkrail owners`), so it is also the way back in
//! when the dashboard's own members table is wrong or lost. Accounts are kept by GitHub's numeric
//! id: a username can be given up and taken by someone else, an id can't.
//!
//! The app is public, because GitHub lets only an app's owner authorize a private one, and a
//! teammate must be able to sign in. Anyone on GitHub can therefore try; the dashboard lets in
//! owners and members only. It asks at creation for what push deploys will need (read code, write
//! commit statuses, deployments and pull request comments), since GitHub has no API to add a
//! permission later and every user would have to approve again.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use forkrail_common::install::{
    GITHUB_APP_PARAM, GITHUB_CLIENT_SECRET_PARAM, GITHUB_PRIVATE_KEY_PARAM, GITHUB_WEBHOOK_SECRET_PARAM, OWNERS_PARAM,
};
use forkrail_common::random_hex;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::deploy::{Ctx, log};
use crate::fetch::{Http, Status};
use crate::secrets::Secrets;
use crate::state::now_ms;

/// The dashboard's routes the app points at (web/src/routes).
pub const CALLBACK_PATH: &str = "/auth/github/callback";
pub const WEBHOOK_PATH: &str = "/api/github/webhook";
/// Where GitHub sends someone who has just installed the app on their repositories.
pub const INSTALLED_PATH: &str = "/settings/git";
/// GitHub's limit on an app's name.
const MAX_NAME: usize = 34;
/// How long the command waits for the click on GitHub's page (GitHub's code lasts an hour).
const WAIT: Duration = Duration::from_secs(15 * 60);

/// GitHub's addresses; a stand-in's for tests (`FORKRAIL_GITHUB_URL`, `FORKRAIL_GITHUB_API_URL`).
pub(crate) fn github() -> String {
    std::env::var("FORKRAIL_GITHUB_URL").unwrap_or_else(|_| "https://github.com".into())
}

pub(crate) fn api() -> String {
    std::env::var("FORKRAIL_GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into())
}

/// A GitHub account: a person, or an organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: u64,
    pub login: String,
    /// `User` or `Organization`.
    #[serde(default, rename = "type")]
    pub kind: String,
}

/// The install's app, as `/forkrail/github/app` keeps it. Nothing here is secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct App {
    pub id: u64,
    pub slug: String,
    pub name: String,
    pub client_id: String,
    pub html_url: String,
    pub owner: Account,
    /// Where GitHub sends someone who has signed in: the first of `callbacks`, the one the
    /// dashboard names when it starts a sign-in (GitHub accepts only registered addresses).
    pub callback: String,
    pub callbacks: Vec<String>,
    pub webhook: String,
    #[serde(default)]
    pub created_ms: u64,
}

/// One of the install's owners, as `/forkrail/web/owners` keeps them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub id: u64,
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub added_ms: u64,
}

/// What GitHub answers when a manifest's code is exchanged.
#[derive(Serialize, Deserialize)]
struct Conversion {
    id: u64,
    slug: String,
    name: String,
    client_id: String,
    client_secret: String,
    #[serde(default)]
    webhook_secret: Option<String>,
    pem: String,
    html_url: String,
    owner: Account,
}

#[derive(Serialize, Deserialize)]
struct Profile {
    id: u64,
    login: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    kind: String,
}

#[derive(Default)]
pub struct Setup {
    /// Create the app under this organization instead of the signed-in person's account.
    pub org: Option<String>,
    /// The install's first owner, by GitHub username. Needed with `org`; otherwise the person who
    /// creates the app is.
    pub owner: Option<String>,
    /// The app's name on GitHub.
    pub name: Option<String>,
    /// More addresses the dashboard is (or will be) served at: each becomes a callback.
    pub callbacks: Vec<String>,
    /// Make a new app although the install has one.
    pub replace: bool,
    /// The installer's setup page opens GitHub from its button, so nothing opens by itself here.
    pub opened_by_page: bool,
}

/// An app GitHub has just made, with its keys, before they're stored: what [`make`] returns and
/// [`keep`] stores. `forkrail install` makes the app before anything else exists and keeps this in
/// a file only its user can read until the install has somewhere to store it.
#[derive(Serialize, Deserialize)]
pub struct Made {
    conversion: Conversion,
    dashboard: String,
    callbacks: Vec<String>,
    first_owner: Option<Profile>,
}

impl Made {
    pub fn name(&self) -> &str {
        &self.conversion.name
    }
    pub fn html_url(&self) -> &str {
        &self.conversion.html_url
    }
}

/// `forkrail git setup`.
pub async fn setup(ctx: &Ctx, setup: Setup) -> Result<()> {
    let settings = ctx.store.settings().await?;
    let dashboard = settings
        .dashboard
        .clone()
        .context("the install has no dashboard address yet: set it first with `forkrail dashboard https://...`")?;
    let secrets = Secrets::new(ctx);
    if let Some(app) = read_app(&secrets).await?
        && !setup.replace
    {
        bail!(
            "this install already has a GitHub App: {} ({})\n`forkrail git status` shows it; --replace makes a new one (delete the old one on GitHub afterwards)",
            app.name,
            app.html_url
        );
    }
    if setup.org.is_some() && setup.owner.is_none() && read_owners(&secrets).await?.is_empty() {
        bail!("with --org, name the install's first owner too: --owner YOUR_GITHUB_USERNAME");
    }
    let made = make(&dashboard, &settings.base_domain, &setup, &|_| {}).await?;
    keep(ctx, made).await?;
    println!("\nNext: open {dashboard} and choose Continue with GitHub.");
    Ok(())
}

/// `forkrail git setup --from FILE`: stores an app [`make`] made earlier (the installer's).
pub async fn setup_from(ctx: &Ctx, path: &std::path::Path) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let made: Made = serde_json::from_str(&text).with_context(|| format!("{} isn't a GitHub App the installer made", path.display()))?;
    keep(ctx, made).await
}

/// Makes the app on GitHub for a dashboard at `dashboard`: a page on this machine sends the browser
/// to GitHub with the app filled in, and waits for the click on Create. Needs nothing of AWS.
/// `show` gets that page's address once it's open.
pub async fn make(dashboard: &str, base_domain: &str, setup: &Setup, show: &dyn Fn(&str)) -> Result<Made> {
    let callbacks = callbacks(dashboard, &setup.callbacks)?;
    let name = app_name(setup.name.as_deref(), base_domain)?;
    let http = Http::new()?;
    let first_owner = match &setup.owner {
        Some(login) => Some(lookup(&http, login).await?),
        None => None,
    };

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("opening a local port for GitHub's answer")?;
    let port = listener.local_addr()?.port();
    let state = random_hex(16);
    let manifest = manifest(&name, dashboard, &format!("http://127.0.0.1:{port}/created"), &callbacks);
    let action = match &setup.org {
        Some(org) => format!("{}/organizations/{org}/settings/apps/new?state={state}", github()),
        None => format!("{}/settings/apps/new?state={state}", github()),
    };
    // The page holds the state, which is all `/created` asks for: it's served only to whoever has
    // the key in its address (the browser this opens, or the setup page's button), and only by this
    // address, so neither another process here nor a web page that rebinds a name to 127.0.0.1 can
    // read it.
    let key = random_hex(16);
    let mut codes = serve(listener, Local { page: page(&action, &manifest), state, key: key.clone(), host: format!("127.0.0.1:{port}") });
    let local = format!("http://127.0.0.1:{port}/?k={key}");
    if setup.opened_by_page {
        log(&format!("creating the GitHub App \"{name}\": the setup page's button opens GitHub"));
    } else {
        log(&format!("creating the GitHub App \"{name}\": click Create on the page that opens"));
        if !open_browser(&local) {
            log("couldn't open a browser here");
        }
        eprintln!("    if no page opened, go to {local}");
    }
    show(&local);
    let code = tokio::time::timeout(WAIT, codes.recv())
        .await
        .map_err(|_| anyhow::anyhow!("no answer from GitHub in {} minutes; nothing was changed", WAIT.as_secs() / 60))?
        .context("the local page closed before GitHub answered")?;
    let conversion = convert(&http, &code).await?;
    Ok(Made { conversion, dashboard: dashboard.trim_end_matches('/').to_owned(), callbacks, first_owner })
}

/// Stores a made app's keys and record in SSM, and makes its first owner the install's, unless
/// the install has owners: the named one, else whoever made the app.
pub async fn keep(ctx: &Ctx, made: Made) -> Result<()> {
    let Made { conversion: converted, dashboard, callbacks, first_owner } = made;
    let secrets = Secrets::new(ctx);
    let http = Http::new()?;
    let mut owners = read_owners(&secrets).await?;
    // Keys first, the app's record last: the dashboard offers GitHub sign-in once the record is there.
    secrets.put_install(GITHUB_PRIVATE_KEY_PARAM, &converted.pem, true, "forkrail's GitHub App: private key").await?;
    secrets.put_install(GITHUB_CLIENT_SECRET_PARAM, &converted.client_secret, true, "forkrail's GitHub App: client secret").await?;
    let webhook_secret = converted.webhook_secret.clone().context("GitHub made the app without a webhook secret")?;
    secrets.put_install(GITHUB_WEBHOOK_SECRET_PARAM, &webhook_secret, true, "forkrail's GitHub App: webhook secret").await?;
    let app = App {
        id: converted.id,
        slug: converted.slug,
        name: converted.name,
        client_id: converted.client_id,
        html_url: converted.html_url,
        owner: converted.owner,
        callback: callbacks[0].clone(),
        callbacks,
        webhook: format!("{dashboard}{WEBHOOK_PATH}"),
        created_ms: now_ms(),
    };
    secrets.put_install(GITHUB_APP_PARAM, &serde_json::to_string(&app)?, false, "forkrail's GitHub App").await?;
    log(&format!("created {} ({}); its keys are in SSM", app.name, app.html_url));

    // Who signs in first: the named owner, else whoever made the app, unless the install has owners.
    let first_owner = match first_owner {
        Some(profile) => Some(profile),
        None if owners.is_empty() && app.owner.kind == "User" => lookup(&http, &app.owner.login).await.ok().or(Some(Profile {
            id: app.owner.id,
            login: app.owner.login.clone(),
            name: None,
            kind: app.owner.kind.clone(),
        })),
        None => None,
    };
    if let Some(profile) = first_owner {
        add_owner(&mut owners, &profile)?;
        write_owners(&secrets, &owners).await?;
    }
    print_owners(&owners);
    Ok(())
}

/// `forkrail git status`.
pub async fn status(ctx: &Ctx) -> Result<()> {
    let secrets = Secrets::new(ctx);
    let Some(app) = read_app(&secrets).await? else {
        println!("No GitHub App yet: `forkrail git setup` makes one.");
        return Ok(());
    };
    println!("{}  {}", app.name, app.html_url);
    println!("  app id {}, client id {}, owned by {} ({})", app.id, app.client_id, app.owner.login, app.owner.kind.to_lowercase());
    println!("  sign-in returns to:");
    for callback in &app.callbacks {
        println!("    {callback}");
    }
    println!("  webhook: {}", app.webhook);
    for (what, name) in
        [("private key", GITHUB_PRIVATE_KEY_PARAM), ("client secret", GITHUB_CLIENT_SECRET_PARAM), ("webhook secret", GITHUB_WEBHOOK_SECRET_PARAM)]
    {
        println!("  {what}: {}", if secrets.exists(name).await? { format!("in SSM at {name}") } else { format!("MISSING from SSM ({name})") });
    }
    // Asked of GitHub as the app, which needs its private key: someone who can't read it still
    // sees the rest.
    if let Err(e) = crate::connect::print_accounts(ctx).await {
        println!("  installed on: unknown ({e:#})");
    }
    print_owners(&read_owners(&secrets).await?);
    Ok(())
}

/// `forkrail owners ls`.
pub async fn owners_list(ctx: &Ctx) -> Result<()> {
    print_owners(&read_owners(&Secrets::new(ctx)).await?);
    Ok(())
}

/// `forkrail owners add LOGIN`.
pub async fn owners_add(ctx: &Ctx, login: &str) -> Result<()> {
    let secrets = Secrets::new(ctx);
    let mut owners = read_owners(&secrets).await?;
    let profile = lookup(&Http::new()?, login).await?;
    if !add_owner(&mut owners, &profile)? {
        log(&format!("{} is already an owner", profile.login));
        return Ok(());
    }
    write_owners(&secrets, &owners).await?;
    log(&format!("{} can sign in to the dashboard as an owner (within a minute)", profile.login));
    Ok(())
}

/// `forkrail owners rm LOGIN`.
pub async fn owners_remove(ctx: &Ctx, login: &str) -> Result<()> {
    let secrets = Secrets::new(ctx);
    let mut owners = read_owners(&secrets).await?;
    remove_owner(&mut owners, login)?;
    write_owners(&secrets, &owners).await?;
    log(&format!("{login} is no longer one of the install's owners (a role given in the dashboard's Members stays until removed there)"));
    Ok(())
}

fn print_owners(owners: &[Owner]) {
    if owners.is_empty() {
        println!("Owners: none yet. `forkrail owners add GITHUB_USERNAME` lets someone sign in.");
        return;
    }
    println!("Owners (always let in; change with `forkrail owners add|rm`):");
    for owner in owners {
        match &owner.name {
            Some(name) => println!("  {}  {name}", owner.login),
            None => println!("  {}", owner.login),
        }
    }
}

/// Add `profile` to the owners; false if that account is one already (its login is refreshed).
fn add_owner(owners: &mut Vec<Owner>, profile: &Profile) -> Result<bool> {
    if profile.kind == "Organization" {
        bail!("{} is an organization: an owner is a person's account", profile.login);
    }
    if let Some(known) = owners.iter_mut().find(|o| o.id == profile.id) {
        known.login = profile.login.clone();
        return Ok(false);
    }
    owners.push(Owner { id: profile.id, login: profile.login.clone(), name: profile.name.clone(), added_ms: now_ms() });
    Ok(true)
}

/// Remove the owner with this username. The last one stays: without an owner nobody could add
/// the next.
fn remove_owner(owners: &mut Vec<Owner>, login: &str) -> Result<()> {
    let Some(at) = owners.iter().position(|o| o.login.eq_ignore_ascii_case(login)) else {
        bail!("{login} isn't one of the install's owners");
    };
    if owners.len() == 1 {
        bail!("{login} is the install's only owner: add another first (`forkrail owners add`)");
    }
    owners.remove(at);
    Ok(())
}

pub(crate) async fn read_app(secrets: &Secrets) -> Result<Option<App>> {
    match secrets.get_install(GITHUB_APP_PARAM).await? {
        Some(text) => Ok(Some(serde_json::from_str(&text).with_context(|| format!("{GITHUB_APP_PARAM} isn't an app's record"))?)),
        None => Ok(None),
    }
}

async fn read_owners(secrets: &Secrets) -> Result<Vec<Owner>> {
    match secrets.get_install(OWNERS_PARAM).await? {
        Some(text) => serde_json::from_str(&text).with_context(|| format!("{OWNERS_PARAM} isn't a list of owners")),
        None => Ok(Vec::new()),
    }
}

async fn write_owners(secrets: &Secrets, owners: &[Owner]) -> Result<()> {
    secrets.put_install(OWNERS_PARAM, &serde_json::to_string(owners)?, false, "forkrail: the install's owners (GitHub accounts)").await
}

/// The account behind a GitHub username, from GitHub's public API.
async fn lookup(http: &Http, login: &str) -> Result<Profile> {
    let login = login.trim().trim_start_matches('@');
    if login.is_empty() || login.len() > 39 || !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("{login}: not a GitHub username");
    }
    let url = format!("{}/users/{login}", api());
    let body = match http.call(Method::GET, &url, &[("accept", "application/vnd.github+json")], None).await {
        Ok(body) => body,
        Err(e) if e.downcast_ref::<Status>().is_some_and(|s| s.0 == 404) => bail!("GitHub has no account named {login}"),
        Err(e) => return Err(e.context(format!("asking GitHub about {login}"))),
    };
    serde_json::from_slice(&body).with_context(|| format!("GitHub's answer about {login}"))
}

/// Exchange the code GitHub sent back for the app it made. Works once.
async fn convert(http: &Http, code: &str) -> Result<Conversion> {
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("GitHub sent back a code that doesn't look like one");
    }
    let url = format!("{}/app-manifests/{code}/conversions", api());
    let body = http
        .call(Method::POST, &url, &[("accept", "application/vnd.github+json")], Some(Bytes::new()))
        .await
        .context("asking GitHub for the new app's keys")?;
    serde_json::from_slice(&body).context("GitHub's answer for the new app")
}

/// The addresses GitHub may send a signed-in person back to: the dashboard's, then any others.
/// Each is an origin (`https://app.example.com`) or already a whole callback address.
fn callbacks(dashboard: &str, more: &[String]) -> Result<Vec<String>> {
    let mut all = Vec::new();
    for origin in std::iter::once(dashboard).chain(more.iter().map(String::as_str)) {
        let origin = origin.trim().trim_end_matches('/');
        let local = origin.starts_with("http://localhost") || origin.starts_with("http://127.0.0.1");
        if !origin.starts_with("https://") && !local {
            bail!("{origin}: a callback address starts with https:// (or http://localhost)");
        }
        let after_scheme = origin.split_once("://").map_or("", |(_, rest)| rest);
        let url = if after_scheme.contains('/') { origin.to_owned() } else { format!("{origin}{CALLBACK_PATH}") };
        if !url.ends_with(CALLBACK_PATH) {
            bail!("{origin}: the dashboard's callback path is {CALLBACK_PATH}");
        }
        if !all.contains(&url) {
            all.push(url);
        }
    }
    if all.len() > 10 {
        bail!("GitHub allows 10 callback addresses, not {}", all.len());
    }
    Ok(all)
}

fn app_name(given: Option<&str>, base_domain: &str) -> Result<String> {
    let name = given.map_or_else(|| format!("forkrail ({base_domain})"), |n| n.trim().to_owned());
    let name = if given.is_none() && name.chars().count() > MAX_NAME { "forkrail".to_owned() } else { name };
    if name.is_empty() || name.chars().count() > MAX_NAME {
        bail!("an app's name is 1 to {MAX_NAME} characters");
    }
    Ok(name)
}

/// The app, as GitHub's manifest flow takes it.
fn manifest(name: &str, dashboard: &str, redirect: &str, callbacks: &[String]) -> Value {
    json!({
        "name": name,
        "url": dashboard,
        "description": "Deploys to your own AWS account with forkrail: sign-in for the dashboard, and deploys from git.",
        "redirect_url": redirect,
        "callback_urls": callbacks,
        "hook_attributes": { "url": format!("{dashboard}{WEBHOOK_PATH}"), "active": true },
        "setup_url": format!("{dashboard}{INSTALLED_PATH}"),
        "setup_on_update": true,
        "public": true,
        "request_oauth_on_install": false,
        "default_permissions": {
            "metadata": "read",
            "contents": "read",
            "statuses": "write",
            "deployments": "write",
            "pull_requests": "write",
        },
        "default_events": ["push", "pull_request", "delete"],
    })
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// The local page: a form holding the manifest, posted to GitHub as soon as it loads.
fn page(action: &str, manifest: &Value) -> String {
    format!(
        r#"<!doctype html><meta charset="utf-8"><title>forkrail: create the GitHub App</title>
<body style="font: 16px system-ui, sans-serif; max-width: 34em; margin: 4em auto; padding: 0 1em">
<h1 style="font-size: 20px">Taking you to GitHub…</h1>
<p>GitHub will show the app forkrail needs, already filled in. Click <b>Create GitHub App</b> there.</p>
<p>If GitHub asks you to confirm it's you and then shows an empty form, come back to this page and click <b>Continue to GitHub</b>: it opens filled in again.</p>
<form id="f" method="post" action="{action}">
<input type="hidden" name="manifest" value="{manifest}">
<button type="submit" style="font: inherit; padding: .5em 1em">Continue to GitHub</button>
</form>
<script>document.getElementById('f').submit()</script>"#,
        action = escape(action),
        manifest = escape(&manifest.to_string()),
    )
}

const DONE: &str = r#"<!doctype html><meta charset="utf-8"><title>forkrail: GitHub App created</title>
<body style="font: 16px system-ui, sans-serif; max-width: 34em; margin: 4em auto; padding: 0 1em">
<h1 style="font-size: 20px">The GitHub App is created.</h1>
<p>You can close this tab: forkrail carries on by itself, in its setup page or your terminal.</p>"#;

/// What the local page server knows: the page, the state GitHub sends back, the key the page's
/// address carries, and the only `Host` it answers (`127.0.0.1:<port>`).
struct Local {
    page: String,
    state: String,
    key: String,
    host: String,
}

/// Serve the local page and wait for GitHub to send the browser back to `/created` with a code
/// and our `state`; the code comes out of the channel. Anything else gets a 403 or a 404.
fn serve(listener: TcpListener, local: Local) -> mpsc::Receiver<String> {
    let (codes, received) = mpsc::channel::<String>(1);
    let shared = Arc::new((local, codes));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            let shared = shared.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let shared = shared.clone();
                    async move { Ok::<_, Infallible>(answer(&request, &shared.0, &shared.1).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    });
    received
}

async fn answer(request: &Request<Incoming>, local: &Local, codes: &mpsc::Sender<String>) -> Response<Full<Bytes>> {
    let html = |status: StatusCode, body: &str| {
        Response::builder()
            .status(status)
            .header("content-type", "text/html; charset=utf-8")
            .header("cache-control", "no-store")
            .body(Full::new(Bytes::from(body.to_owned())))
            .expect("a response")
    };
    if request.method() != Method::GET {
        return html(StatusCode::METHOD_NOT_ALLOWED, "GET only");
    }
    let host = request.headers().get(hyper::header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !host.eq_ignore_ascii_case(&local.host) {
        return html(StatusCode::FORBIDDEN, "forbidden");
    }
    let query = request.uri().query().unwrap_or_default();
    let field = |name: &str| query.split('&').filter_map(|pair| pair.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| v.to_owned());
    match request.uri().path() {
        "/" if field("k").as_deref() == Some(local.key.as_str()) => html(StatusCode::OK, &local.page),
        "/" => html(StatusCode::FORBIDDEN, "Open the address forkrail printed, with its key."),
        "/created" => {
            match (field("code"), field("state")) {
                (Some(code), Some(theirs)) if theirs == local.state => {
                    let _ = codes.send(code).await;
                    html(StatusCode::OK, DONE)
                }
                _ => html(StatusCode::BAD_REQUEST, "This isn't the answer forkrail is waiting for. Run `forkrail git setup` again."),
            }
        }
        _ => html(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Open `url` in the person's browser; false if this machine has no way to.
pub(crate) fn open_browser(url: &str) -> bool {
    // On Windows the URL protocol handler opens the default browser, as the Run box would. Not
    // `cmd /C start`: cmd would split the address at its first `&`.
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        ("rundll32", &["url.dll,FileProtocolHandler"])
    } else {
        ("xdg-open", &[])
    };
    std::process::Command::new(program)
        .args(args)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;

    fn profile(id: u64, login: &str) -> Profile {
        Profile { id, login: login.into(), name: None, kind: "User".into() }
    }

    #[test]
    fn callbacks_are_whole_addresses_once_each() {
        let all = callbacks(
            "https://forkrail-web.example.com/",
            &["https://app.example.com".into(), "http://localhost:3040".into(), "https://app.example.com/auth/github/callback".into()],
        )
        .unwrap();
        assert_eq!(
            all,
            [
                "https://forkrail-web.example.com/auth/github/callback",
                "https://app.example.com/auth/github/callback",
                "http://localhost:3040/auth/github/callback",
            ]
        );
        assert!(callbacks("http://app.example.com", &[]).is_err(), "plain http only on localhost");
        assert!(callbacks("https://app.example.com/elsewhere", &[]).is_err());
    }

    #[test]
    fn the_manifest_asks_for_what_sign_in_and_push_deploys_need() {
        let callbacks = callbacks("https://dash.example.com", &[]).unwrap();
        let m = manifest("forkrail (example.com)", "https://dash.example.com", "http://127.0.0.1:5000/created", &callbacks);
        assert_eq!(m["public"], true, "a private app can be authorized only by its owner");
        assert_eq!(m["callback_urls"][0], "https://dash.example.com/auth/github/callback");
        assert_eq!(m["hook_attributes"]["url"], "https://dash.example.com/api/github/webhook");
        assert_eq!(m["default_permissions"]["contents"], "read");
        assert_eq!(m["default_permissions"]["statuses"], "write");
        assert_eq!(m["redirect_url"], "http://127.0.0.1:5000/created");
        // Nothing wider than read on code, and no organization or account permissions.
        let permissions = m["default_permissions"].as_object().unwrap();
        assert_eq!(permissions.len(), 5);
    }

    #[test]
    fn names_fit_githubs_limit() {
        assert_eq!(app_name(None, "example.com").unwrap(), "forkrail (example.com)");
        assert_eq!(app_name(None, "a-very-long-company-name.example.com").unwrap(), "forkrail");
        assert_eq!(app_name(Some(" Acme deploys "), "example.com").unwrap(), "Acme deploys");
        assert!(app_name(Some("a name that is far longer than GitHub allows"), "example.com").is_err());
    }

    #[test]
    fn the_page_escapes_the_manifest() {
        let html = page("https://github.com/settings/apps/new?state=ab", &json!({"name": "a \"b\" <c> & 'd'"}));
        assert!(html.contains("&quot;name&quot;"));
        assert!(html.contains("&lt;c&gt; &amp; &#39;d&#39;"));
        assert!(!html.contains("<c>"));
    }

    #[test]
    fn owners_are_kept_by_id_and_the_last_one_stays() {
        let mut owners = Vec::new();
        assert!(add_owner(&mut owners, &profile(7, "ada")).unwrap());
        // The same account under a new username is the same owner.
        assert!(!add_owner(&mut owners, &profile(7, "ada-l")).unwrap());
        assert_eq!((owners.len(), owners[0].login.as_str()), (1, "ada-l"));
        assert!(remove_owner(&mut owners, "ADA-L").is_err(), "the only owner stays");
        assert!(add_owner(&mut owners, &profile(8, "grace")).unwrap());
        remove_owner(&mut owners, "Ada-L").unwrap();
        assert_eq!(owners.iter().map(|o| o.id).collect::<Vec<_>>(), [8]);
        assert!(remove_owner(&mut owners, "nobody").is_err());
        let org = Profile { id: 9, login: "acme".into(), name: None, kind: "Organization".into() };
        assert!(add_owner(&mut owners, &org).is_err());
    }

    async fn get_as(port: u16, host: &str, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(format!("GET {path} HTTP/1.1\r\nhost: {host}\r\nconnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).await.unwrap();
        text
    }

    #[tokio::test]
    async fn the_local_page_takes_githubs_answer_only_with_our_state() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let here = format!("127.0.0.1:{port}");
        let mut codes = serve(listener, Local { page: "the page".into(), state: "s3cret".into(), key: "k3y".into(), host: here.clone() });
        let get = |path: &'static str| get_as(port, &here, path);
        assert!(get("/?k=k3y").await.ends_with("the page"));
        // Without the key, or by another name (a page that rebinds its name to 127.0.0.1): no page.
        assert!(get("/").await.starts_with("HTTP/1.1 403"));
        assert!(get("/?k=wrong").await.starts_with("HTTP/1.1 403"));
        assert!(get_as(port, &format!("rebound.example:{port}"), "/?k=k3y").await.starts_with("HTTP/1.1 403"));
        assert!(get_as(port, &format!("rebound.example:{port}"), "/created?code=abc&state=s3cret").await.starts_with("HTTP/1.1 403"));
        assert!(get("/created?code=abc&state=wrong").await.starts_with("HTTP/1.1 400"));
        assert!(get("/created?code=abc").await.starts_with("HTTP/1.1 400"));
        assert!(codes.try_recv().is_err(), "no code from an answer without our state");
        assert!(get("/created?code=abc123&state=s3cret").await.starts_with("HTTP/1.1 200"));
        assert_eq!(codes.recv().await.unwrap(), "abc123");
        assert!(get("/elsewhere").await.starts_with("HTTP/1.1 404"));
    }
}
