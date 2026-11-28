//! A project's connected GitHub repository, and the deployments its pushes make.
//!
//! The install's GitHub App (github.rs) signs people in; to reach code it must also be installed
//! on the account that owns a repository, for all of that account's repositories or the ones
//! chosen there. `Api::repos` lists what every installation reaches, and a project connects to one
//! repository (`connect`): its record keeps GitHub's id for it, the installation, and which branch
//! is production.
//!
//! A push (`pushed`) is what GitHub's webhook tells the dashboard, which checks its signature and
//! hands it to the control function. The commit's files are fetched from GitHub as the app, put
//! where a remote build's source goes (remote.rs), and built in the install's builder like any
//! other remote deployment: the production branch as production, any other branch as a preview,
//! as on Vercel. The commit gets a status on GitHub, pending and then how the deployment ended
//! (`report`), and GitHub is told of the deployment itself (its Deployments API), which is what
//! puts "View deployment" on the commit's pull request once it's ready.
//!
//! A project can also be made from a repository (`create`): connected from the start, with its
//! production branch deployed at once.
//!
//! Around a push, as on Vercel:
//! - `git.deploymentEnabled` in the project's `vercel.json` can turn pushes off, for every branch
//!   or for some: such a push makes no deployment at all (`Pushed::Skipped`).
//! - A branch's newest preview answers at `<project>-git-<branch>.<base>` (publish.rs).
//! - Every open pull request of the branch carries one comment per project, kept up to date with
//!   how the branch's newest deployment stands (`comment`); a pull request opened later gets it
//!   then (`pull_request`).
//! - When a branch is deleted its previews go, and its host with them (`branch_deleted`).
//!
//! Everything asked of GitHub here is asked as the app, with its private key, which only the
//! control function's role and the install's administrators can read. Tokens are kept in memory
//! for under an hour and never go to a build.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bytes::Bytes;
use forkrail_common::install::GITHUB_PRIVATE_KEY_PARAM;
use hyper::Method;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::deploy::{self, BRANCH_INFIX, Ctx, branch_host, check_project, new_project, project_record};
use crate::env as env_vars;
use crate::fetch::{Http, Status as HttpStatus};
use crate::github::{self, App};
use crate::package::TempDir;
use crate::remote::{self, Launched, PublishOptions, Start, Started};
use crate::secrets::Secrets;
use crate::state::{self, DeploymentRecord, EnvTarget, GitCommit, GitLink, HookRef, Project, Settings, Status, Target, Via, now_ms, now_rfc3339, now_secs};
use crate::lambda;
use crate::storage;
use crate::vercel;
use forkrail_common::routes::Routes;

/// A repository's files at one commit, zipped by GitHub, at most. A build's source may be 1 GB
/// (source.rs); this is what the control function has room to unpack and zip again.
const MAX_ARCHIVE_BYTES: u64 = 256 << 20;
/// How long a list of repositories is kept before GitHub is asked again.
const REPOS_FOR_MS: u64 = 60_000;
/// Repositories read per installation at most (GitHub gives 100 a page).
const MAX_PAGES: u32 = 20;
/// Repositories listed in one answer.
const LISTED: usize = 30;
/// GitHub's limit on a commit status's description.
const MAX_DESCRIPTION: usize = 140;
/// A pull request's comments are read this many pages deep (100 a page) to find the project's own.
const COMMENT_PAGES: u32 = 10;
/// Deployments looked through, newest first, for a branch's newest.
const BRANCH_LOOKBACK: usize = 40;

/// GitHub refused a request: its status and what it said.
#[derive(Debug)]
pub struct Refused {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GitHub answered {}: {}", self.status, self.message)
    }
}

impl std::error::Error for Refused {}

fn refused(e: &anyhow::Error, status: u16) -> bool {
    e.downcast_ref::<Refused>().is_some_and(|r| r.status == status)
}

/// A repository the app reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub id: u64,
    /// `owner/name`.
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub default_branch: String,
    #[serde(default)]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub html_url: String,
    /// The installation that reaches it (ours to fill in: GitHub doesn't say it here).
    #[serde(default)]
    pub installation_id: u64,
}

/// A GitHub account the app is installed on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    pub installation_id: u64,
    pub login: String,
    pub avatar: Option<String>,
    pub organization: bool,
    /// Every repository of the account, now and later; else the ones chosen on GitHub.
    pub all: bool,
    /// Where on GitHub the account changes which repositories the app reaches.
    pub manage_url: String,
    pub suspended: bool,
}

/// What the dashboard shows of the app when connecting a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInfo {
    pub name: String,
    pub url: String,
    /// Where on GitHub the app is installed on an account, or given more repositories.
    pub install_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub app: AppInfo,
    pub accounts: Vec<Account>,
    /// The repositories matching the query, the most recently pushed first, `LISTED` at most.
    pub repos: Vec<Repo>,
    /// How many match in all.
    pub total: usize,
}

#[derive(Deserialize)]
struct RawInstallation {
    id: u64,
    #[serde(default)]
    account: Option<RawAccount>,
    #[serde(default)]
    repository_selection: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    suspended_at: Option<String>,
}

#[derive(Deserialize)]
struct RawAccount {
    login: String,
    #[serde(default)]
    avatar_url: Option<String>,
    #[serde(default, rename = "type")]
    kind: String,
}

/// A commit, as GitHub's API gives it.
#[derive(Debug, Deserialize)]
pub struct Commit {
    pub sha: String,
    commit: CommitBody,
    /// The GitHub account that wrote it, when GitHub knows one for the commit's email.
    #[serde(default)]
    author: Option<Login>,
    #[serde(default)]
    html_url: String,
}

#[derive(Debug, Deserialize)]
struct CommitBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    author: Option<Person>,
}

#[derive(Debug, Deserialize)]
struct Person {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Login {
    login: String,
}

type Listed = (u64, Vec<Account>, Arc<Vec<Repo>>);

/// GitHub, asked as the install's app.
pub struct Api {
    http: Http,
    /// GitHub's API: `https://api.github.com`.
    base: String,
    pub app: App,
    key: RsaKeyPair,
    /// Installation tokens (they last an hour), with when each stops being used.
    tokens: Mutex<HashMap<u64, (String, u64)>>,
    /// The last list of everything the app reaches, with when it was read.
    listed: Mutex<Option<Listed>>,
}

impl Api {
    /// The install's app and its private key, from SSM. With a stand-in for GitHub
    /// (`FORKRAIL_GITHUB_API_URL`, for tests), `FORKRAIL_GITHUB_APP` and
    /// `FORKRAIL_GITHUB_PRIVATE_KEY_FILE` give a stand-in app instead.
    pub async fn connect(ctx: &Ctx) -> Result<Self> {
        let stand_in = |name: &str| std::env::var("FORKRAIL_GITHUB_API_URL").ok().and_then(|_| std::env::var(name).ok());
        let secrets = Secrets::new(ctx);
        let app: App = match stand_in("FORKRAIL_GITHUB_APP") {
            Some(text) => serde_json::from_str(&text).context("FORKRAIL_GITHUB_APP isn't an app's record")?,
            None => github::read_app(&secrets).await?.context("this install has no GitHub App yet: `forkrail git setup` makes one")?,
        };
        let pem = match stand_in("FORKRAIL_GITHUB_PRIVATE_KEY_FILE") {
            Some(path) => std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?,
            None => secrets
                .get_secret(GITHUB_PRIVATE_KEY_PARAM)
                .await?
                .with_context(|| format!("the GitHub App's private key isn't in SSM ({GITHUB_PRIVATE_KEY_PARAM})"))?,
        };
        Ok(Self { http: Http::new()?, base: github::api(), app, key: key_of(&pem)?, tokens: Mutex::default(), listed: Mutex::default() })
    }

    pub fn info(&self) -> AppInfo {
        AppInfo {
            name: self.app.name.clone(),
            url: self.app.html_url.clone(),
            install_url: format!("{}/apps/{}/installations/new", github::github(), self.app.slug),
        }
    }

    /// A token that says "I am the app", good for ten minutes: for the few calls about the app
    /// itself, and for getting an installation's token.
    fn jwt(&self) -> Result<String> {
        let now = now_secs();
        let part = |value: Value| URL_SAFE_NO_PAD.encode(value.to_string());
        // Issued a minute ago, for a clock that runs ahead of GitHub's.
        let claims = json!({"iat": now - 60, "exp": now + 9 * 60, "iss": self.app.id.to_string()});
        let signed = format!("{}.{}", part(json!({"alg": "RS256", "typ": "JWT"})), part(claims));
        let mut signature = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signed.as_bytes(), &mut signature)
            .map_err(|_| anyhow::anyhow!("signing as the GitHub App failed"))?;
        Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    /// One call to GitHub's API with `auth` as its bearer; a refusal is a [`Refused`].
    async fn call<T: DeserializeOwned>(&self, method: Method, path: &str, auth: &str, body: Option<Value>) -> Result<T> {
        let url = format!("{}{path}", self.base);
        let authorization = format!("Bearer {auth}");
        let mut headers = vec![("accept", "application/vnd.github+json"), ("x-github-api-version", "2022-11-28"), ("authorization", authorization.as_str())];
        if body.is_some() {
            headers.push(("content-type", "application/json"));
        }
        let body = body.map(|b| Bytes::from(b.to_string()));
        let answer = self.http.call(method, &url, &headers, body).await.map_err(explain)?;
        // A delete answers with nothing.
        let answer: &[u8] = if answer.is_empty() { b"null" } else { &answer };
        serde_json::from_slice(answer).with_context(|| format!("GitHub's answer for {path}"))
    }

    /// A token for one installation: what the app may do in that account's repositories.
    async fn token(&self, installation: u64) -> Result<String> {
        if let Some((token, until)) = self.tokens.lock().expect("tokens").get(&installation).cloned()
            && until > now_ms()
        {
            return Ok(token);
        }
        #[derive(Deserialize)]
        struct Granted {
            token: String,
        }
        let path = format!("/app/installations/{installation}/access_tokens");
        let granted: Granted = self.call(Method::POST, &path, &self.jwt()?, Some(json!({}))).await.context("asking GitHub for the installation's token")?;
        self.tokens.lock().expect("tokens").insert(installation, (granted.token.clone(), now_ms() + 50 * 60_000));
        Ok(granted.token)
    }

    /// The accounts the app is installed on.
    async fn accounts(&self) -> Result<Vec<Account>> {
        let raw: Vec<RawInstallation> = self.call(Method::GET, "/app/installations?per_page=100", &self.jwt()?, None).await.context("listing the app's installations")?;
        Ok(raw
            .into_iter()
            .filter_map(|i| {
                let account = i.account?;
                Some(Account {
                    installation_id: i.id,
                    login: account.login,
                    avatar: account.avatar_url,
                    organization: account.kind == "Organization",
                    all: i.repository_selection == "all",
                    manage_url: i.html_url,
                    suspended: i.suspended_at.is_some(),
                })
            })
            .collect())
    }

    /// Every account the app is installed on and every repository it reaches, the most recently
    /// pushed first. Kept a minute, unless `fresh`.
    async fn everything(&self, fresh: bool) -> Result<(Vec<Account>, Arc<Vec<Repo>>)> {
        if !fresh
            && let Some((at, accounts, repos)) = self.listed.lock().expect("listed").clone()
            && at + REPOS_FOR_MS > now_ms()
        {
            return Ok((accounts, repos));
        }
        #[derive(Deserialize)]
        struct Page {
            repositories: Vec<Repo>,
        }
        let accounts = self.accounts().await?;
        let mut repos = Vec::new();
        for account in accounts.iter().filter(|a| !a.suspended) {
            let token = self.token(account.installation_id).await?;
            for page in 1..=MAX_PAGES {
                let path = format!("/installation/repositories?per_page=100&page={page}");
                let got: Page = self.call(Method::GET, &path, &token, None).await.with_context(|| format!("listing {}'s repositories", account.login))?;
                let count = got.repositories.len();
                repos.extend(got.repositories.into_iter().map(|mut r| {
                    r.installation_id = account.installation_id;
                    r
                }));
                if count < 100 {
                    break;
                }
            }
        }
        repos.sort_by(|a, b| b.pushed_at.cmp(&a.pushed_at).then_with(|| a.full_name.cmp(&b.full_name)));
        let repos = Arc::new(repos);
        *self.listed.lock().expect("listed") = Some((now_ms(), accounts.clone(), repos.clone()));
        Ok((accounts, repos))
    }

    /// The repositories the app reaches whose name has every word of `query`.
    pub async fn repos(&self, query: Option<&str>, fresh: bool) -> Result<Listing> {
        let (accounts, repos) = self.everything(fresh).await?;
        let words: Vec<String> = query.unwrap_or_default().split_whitespace().map(str::to_lowercase).collect();
        let matching: Vec<&Repo> = repos
            .iter()
            .filter(|r| {
                let name = r.full_name.to_lowercase();
                words.iter().all(|w| name.contains(w.as_str()))
            })
            .collect();
        Ok(Listing { app: self.info(), accounts, total: matching.len(), repos: matching.into_iter().take(LISTED).cloned().collect() })
    }

    /// The repository `owner/name`, if the app reaches it.
    pub async fn find(&self, full_name: &str) -> Result<Repo> {
        check_repo_name(full_name)?;
        #[derive(Deserialize)]
        struct Installation {
            id: u64,
        }
        let installation: Installation = match self.call(Method::GET, &format!("/repos/{full_name}/installation"), &self.jwt()?, None).await {
            Ok(i) => i,
            Err(e) if refused(&e, 404) => bail!(
                "{} can't reach {full_name}: install it on that account, or give it that repository, at {}",
                self.app.name,
                self.info().install_url
            ),
            Err(e) => return Err(e.context(format!("asking GitHub about {full_name}"))),
        };
        let token = self.token(installation.id).await?;
        let mut repo: Repo = self.call(Method::GET, &format!("/repos/{full_name}"), &token, None).await.with_context(|| format!("reading {full_name}"))?;
        repo.installation_id = installation.id;
        Ok(repo)
    }

    /// The commit `rev` (a hash, or `heads/<branch>`) of a repository; `None` if it has none by that name.
    async fn commit(&self, token: &str, repo_id: u64, rev: &str) -> Result<Option<Commit>> {
        match self.call(Method::GET, &format!("/repositories/{repo_id}/commits/{}", encode_rev(rev)), token, None).await {
            Ok(commit) => Ok(Some(commit)),
            // 422 for a name that is no commit, 409 for a repository with no commits.
            Err(e) if refused(&e, 404) || refused(&e, 422) || refused(&e, 409) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The repository's files at commit `sha`, unpacked under `tmp`: the folder holding them.
    async fn fetch(&self, token: &str, repo_id: u64, sha: &str, tmp: &Path) -> Result<PathBuf> {
        let zip = tmp.join("repo.zip");
        let url = format!("{}/repositories/{repo_id}/zipball/{sha}", self.base);
        let authorization = format!("Bearer {token}");
        let headers = [("accept", "application/vnd.github+json"), ("x-github-api-version", "2022-11-28"), ("authorization", authorization.as_str())];
        self.http
            .download(&url, &headers, &zip, MAX_ARCHIVE_BYTES)
            .await
            .map_err(explain)
            .with_context(|| format!("downloading the commit's files (at most {} MB zipped)", MAX_ARCHIVE_BYTES >> 20))?;
        let out = tmp.join("repo");
        let (from, to) = (zip.clone(), out.clone());
        tokio::task::spawn_blocking(move || remote::unzip(&from, &to)).await.context("unpacking the commit's files")??;
        let _ = std::fs::remove_file(&zip);
        top_folder(&out)
    }

    /// Set the commit's status for a project: what GitHub shows beside the commit and on its pull
    /// request.
    async fn set_status(&self, token: &str, repo_id: u64, sha: &str, project: &str, state: &str, description: &str, url: Option<&str>) -> Result<()> {
        let mut body = json!({"state": state, "context": format!("forkrail/{project}"), "description": cut(description, MAX_DESCRIPTION)});
        if let Some(url) = url {
            body["target_url"] = json!(url);
        }
        let _: Value = self.call(Method::POST, &format!("/repositories/{repo_id}/statuses/{sha}"), token, Some(body)).await.context("setting the commit's status")?;
        Ok(())
    }

    /// Tell GitHub a deployment of the commit has begun: GitHub's id for it. GitHub shows it on
    /// the commit and on its pull request, under an environment named for the project and target,
    /// which GitHub makes the first time it's named.
    async fn create_deployment(&self, token: &str, repo_id: u64, sha: &str, project: &str, target: Target) -> Result<u64> {
        #[derive(Deserialize)]
        struct Made {
            id: u64,
        }
        let production = target == Target::Production;
        let body = json!({
            "ref": sha,
            "task": "deploy",
            // The commit as it is: GitHub merges nothing into it and waits for no other check.
            "auto_merge": false,
            "required_contexts": [],
            "environment": environment(project, target),
            "description": if production { "Production, built by forkrail" } else { "A preview, built by forkrail" },
            "transient_environment": !production,
            "production_environment": production,
        });
        let made: Made = self.call(Method::POST, &format!("/repositories/{repo_id}/deployments"), token, Some(body)).await.context("telling GitHub of the deployment")?;
        Ok(made.id)
    }

    /// Say where one of GitHub's deployments stands.
    async fn deployment_status(&self, token: &str, repo_id: u64, deployment: u64, stand: Stand<'_>) -> Result<()> {
        let mut body = json!({"state": stand.state, "description": cut(stand.description, MAX_DESCRIPTION)});
        if let Some(log) = stand.log {
            body["log_url"] = json!(log);
        }
        if let Some(address) = stand.address {
            body["environment_url"] = json!(address);
        }
        let path = format!("/repositories/{repo_id}/deployments/{deployment}/statuses");
        let _: Value = self.call(Method::POST, &path, token, Some(body)).await.context("telling GitHub how the deployment stands")?;
        Ok(())
    }

    /// The open pull requests whose head is `branch` of this same repository (`owner`'s), by number.
    async fn open_pulls(&self, token: &str, repo_id: u64, owner: &str, branch: &str) -> Result<Vec<u64>> {
        #[derive(Deserialize)]
        struct Pull {
            number: u64,
            head: Head,
        }
        #[derive(Deserialize)]
        struct Head {
            #[serde(rename = "ref")]
            branch: String,
            #[serde(default)]
            repo: Option<HeadRepo>,
        }
        #[derive(Deserialize)]
        struct HeadRepo {
            id: u64,
        }
        let path = format!("/repositories/{repo_id}/pulls?state=open&per_page=30&head={}", encode_query(&format!("{owner}:{branch}")));
        let pulls: Vec<Pull> = self.call(Method::GET, &path, token, None).await.context("listing the branch's pull requests")?;
        // GitHub's filter is trusted for nothing: a pull request from a fork's branch of the same
        // name isn't this branch's.
        Ok(pulls.into_iter().filter(|p| p.head.branch == branch && p.head.repo.as_ref().is_some_and(|r| r.id == repo_id)).map(|p| p.number).collect())
    }

    /// The comments this app wrote on pull request `number` that start with `marker`, oldest first.
    async fn own_comments(&self, token: &str, repo_id: u64, number: u64, marker: &str) -> Result<Vec<u64>> {
        #[derive(Deserialize)]
        struct Comment {
            id: u64,
            #[serde(default)]
            body: Option<String>,
            #[serde(default)]
            user: Option<Login>,
            #[serde(default)]
            performed_via_github_app: Option<Via_>,
        }
        #[derive(Deserialize)]
        struct Via_ {
            id: u64,
        }
        let bot = format!("{}[bot]", self.app.slug);
        let mut own = Vec::new();
        for page in 1..=COMMENT_PAGES {
            let path = format!("/repositories/{repo_id}/issues/{number}/comments?per_page=100&page={page}");
            let comments: Vec<Comment> = self.call(Method::GET, &path, token, None).await.context("reading the pull request's comments")?;
            let count = comments.len();
            own.extend(
                comments
                    .into_iter()
                    // Anyone can type the marker: only a comment the app itself made counts.
                    .filter(|c| c.body.as_deref().is_some_and(|b| b.starts_with(marker)))
                    .filter(|c| c.performed_via_github_app.as_ref().is_some_and(|a| a.id == self.app.id) || c.user.as_ref().is_some_and(|u| u.login == bot))
                    .map(|c| c.id),
            );
            if count < 100 {
                break;
            }
        }
        own.sort_unstable();
        Ok(own)
    }

    /// Make pull request `number` carry exactly one comment starting with `marker`, saying `body`:
    /// the app's own comment is rewritten, written if there's none, and any more of them (two
    /// events that met) are deleted.
    async fn put_comment(&self, token: &str, repo_id: u64, number: u64, marker: &str, body: &str) -> Result<()> {
        let own = self.own_comments(token, repo_id, number, marker).await?;
        let body = json!({"body": body});
        match own.split_first() {
            None => {
                let _: Value = self.call(Method::POST, &format!("/repositories/{repo_id}/issues/{number}/comments"), token, Some(body)).await.context("commenting on the pull request")?;
            }
            Some((first, more)) => {
                let _: Value = self.call(Method::PATCH, &format!("/repositories/{repo_id}/issues/comments/{first}"), token, Some(body)).await.context("updating the pull request's comment")?;
                for extra in more {
                    let _: Option<Value> = self.call(Method::DELETE, &format!("/repositories/{repo_id}/issues/comments/{extra}"), token, None).await.context("deleting a second comment")?;
                }
            }
        }
        Ok(())
    }
}

/// Where one of GitHub's deployments stands, as GitHub is told.
struct Stand<'a> {
    /// `in_progress`, `success`, `failure`, `error` or `inactive`.
    state: &'a str,
    description: &'a str,
    /// Its build log, in the dashboard.
    log: Option<&'a str>,
    /// Where it answers once ready: what "View deployment" opens.
    address: Option<&'a str>,
}

/// The environment GitHub files a project's deployments under: one for its production, one for
/// its previews. Named for the project, since a repository may hold several.
fn environment(project: &str, target: Target) -> String {
    match target {
        Target::Production => format!("Production – {project}"),
        Target::Preview => format!("Preview – {project}"),
    }
}

/// A [`HttpStatus`] from GitHub as a [`Refused`], with GitHub's own message.
fn explain(e: anyhow::Error) -> anyhow::Error {
    let Some(HttpStatus(status, text)) = e.downcast_ref::<HttpStatus>() else { return e };
    // The text is the start of GitHub's JSON, which may be cut short.
    let message = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| text.split_once("\"message\":\"").and_then(|(_, rest)| rest.split('"').next()).map(str::to_owned))
        .unwrap_or_else(|| text.clone());
    Refused { status: *status, message }.into()
}

/// The app's private key from its PEM: PKCS#1, as GitHub hands it out, or PKCS#8.
fn key_of(pem: &str) -> Result<RsaKeyPair> {
    let pkcs8 = pem.contains("BEGIN PRIVATE KEY");
    let body: String = pem.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("-----")).collect();
    let der = STANDARD.decode(body).map_err(|_| anyhow::anyhow!("the GitHub App's private key isn't PEM"))?;
    let key = if pkcs8 { RsaKeyPair::from_pkcs8(&der) } else { RsaKeyPair::from_der(&der) };
    key.map_err(|e| anyhow::anyhow!("the GitHub App's private key can't be read: {e}"))
}

/// A branch or a hash as part of a URL's path. Slashes stay: GitHub reads `feature/logs` whole.
fn encode_rev(rev: &str) -> String {
    let mut out = String::with_capacity(rev.len());
    for byte in rev.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Text as one value of a URL's query.
fn encode_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn cut(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max { line.to_owned() } else { line.chars().take(max - 1).chain(['…']).collect() }
}

fn check_repo_name(full_name: &str) -> Result<()> {
    let valid = |part: &str| !part.is_empty() && part.len() <= 100 && part != "." && part != ".." && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    match full_name.split_once('/') {
        Some((owner, name)) if valid(owner) && valid(name) => Ok(()),
        _ => bail!("{full_name:?}: a repository is OWNER/NAME"),
    }
}

/// A branch name as git allows them, near enough: nothing that could be another part of a URL or
/// a way out of one.
pub(crate) fn check_branch(branch: &str) -> Result<()> {
    let bad = branch.is_empty()
        || branch.len() > 255
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.contains("..")
        || branch.contains("//")
        || branch.bytes().any(|b| b.is_ascii_control() || matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\'));
    if bad {
        bail!("{branch:?} isn't a branch name");
    }
    Ok(())
}

fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A project's folder in its repository, as it's kept: `None` for the repository's top.
fn check_root(root: &str) -> Result<Option<String>> {
    let root = root.trim().trim_start_matches("./").trim_matches('/');
    if root.is_empty() || root == "." {
        return Ok(None);
    }
    let bad = root.len() > 200 || root.contains('\\') || root.split('/').any(|part| part.is_empty() || part == "." || part == ".." || part.bytes().any(|b| b.is_ascii_control()));
    if bad {
        bail!("{root:?}: a root directory is a folder inside the repository, like apps/web");
    }
    Ok(Some(root.to_owned()))
}

/// GitHub's archive of a commit holds one folder, `owner-name-hash`, with the repository in it.
fn top_folder(out: &Path) -> Result<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(out).with_context(|| format!("reading {}", out.display()))?.filter_map(|e| e.ok()).collect();
    match entries.as_slice() {
        [one] if one.file_type().is_ok_and(|t| t.is_dir()) => Ok(one.path()),
        _ => bail!("GitHub's archive of the commit isn't one folder"),
    }
}

/// The project's folder in the unpacked repository.
fn project_dir(top: &Path, root: Option<&str>) -> Result<PathBuf> {
    let Some(root) = root else { return Ok(top.to_owned()) };
    let dir = top.join(root);
    if !dir.is_dir() {
        bail!("the repository has no folder {root} at this commit (the project's root directory)");
    }
    Ok(dir)
}

/// `owner/name` from a commit's page address, `https://github.com/owner/name/commit/<hash>`.
fn repo_of(html_url: &str) -> Option<String> {
    let rest = html_url.split_once("://")?.1.split_once('/')?.1;
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let full = format!("{owner}/{name}");
    check_repo_name(&full).ok().map(|()| full)
}

pub struct Connect {
    pub project: String,
    /// `owner/name`.
    pub repo: String,
    /// The branch whose pushes go to production; the repository's default branch if not given.
    pub branch: Option<String>,
    /// The project's folder in the repository, if not its top.
    pub root: Option<String>,
    /// Who asked: their GitHub username.
    pub actor: Option<String>,
}

/// What connects a project to `repo` (`owner/name`), once GitHub has said the app reaches it and
/// that it has the branch: `branch`, or the repository's default.
async fn link_to(api: &Api, repo: &str, branch: Option<&str>, root: Option<&str>, actor: Option<String>) -> Result<GitLink> {
    let root = match root {
        Some(root) => check_root(root)?,
        None => None,
    };
    let repo = api.find(repo.trim()).await?;
    let branch = match branch.map(str::trim).filter(|b| !b.is_empty()) {
        Some(branch) => {
            check_branch(branch)?;
            let token = api.token(repo.installation_id).await?;
            if api.commit(&token, repo.id, &format!("heads/{branch}")).await?.is_none() {
                bail!("{} has no branch {branch}", repo.full_name);
            }
            branch.to_owned()
        }
        None if repo.default_branch.is_empty() => bail!("{} has no branch yet: push a commit to it first", repo.full_name),
        None => repo.default_branch.clone(),
    };
    Ok(GitLink {
        repo_id: repo.id,
        repo: repo.full_name,
        installation_id: repo.installation_id,
        production_branch: branch,
        root,
        private: repo.private,
        connected_at: now_rfc3339(),
        connected_by: actor,
    })
}

/// Connect a project to a repository the app reaches. Pushes deploy from then on.
pub async fn connect(ctx: &Ctx, api: &Api, c: Connect) -> Result<GitLink> {
    check_project(&c.project)?;
    project_record(ctx, &c.project).await?;
    let link = link_to(api, &c.repo, c.branch.as_deref(), c.root.as_deref(), c.actor).await?;
    ctx.store
        .update::<Project, _>(&state::project_key(&c.project), |current| {
            let mut p = current.with_context(|| format!("no project {}", c.project))?;
            let mut link = link.clone();
            // The same repository with another branch or folder: connected when it was, by who did.
            if let Some(was) = p.git.as_ref().filter(|was| was.repo_id == link.repo_id) {
                link.connected_at = was.connected_at.clone();
                link.connected_by = was.connected_by.clone();
            }
            p.git = Some(link);
            Ok(p)
        })
        .await?;
    Ok(project_record(ctx, &c.project).await?.git.unwrap_or(link))
}

/// Disconnect a project from its repository; what it was connected to. Its deployments stay.
pub async fn disconnect(ctx: &Ctx, project: &str) -> Result<Option<GitLink>> {
    check_project(project)?;
    let mut was = None;
    ctx.store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = current.with_context(|| format!("no project {project}"))?;
            was = p.git.take();
            Ok(p)
        })
        .await?;
    Ok(was)
}

/// A project to make from a repository.
pub struct Create {
    pub project: String,
    /// `owner/name`.
    pub repo: String,
    /// The branch whose pushes go to production; the repository's default branch if not given.
    pub branch: Option<String>,
    /// The project's folder in the repository, if not its top.
    pub root: Option<String>,
    /// Env vars to start with, for production and previews alike.
    pub env: BTreeMap<String, String>,
    /// Who asked: their GitHub username.
    pub actor: Option<String>,
    /// Its framework preset (a Vercel slug), when chosen on import; None finds it from its files.
    pub framework: Option<String>,
}

/// A project made from a repository.
#[derive(Debug, Clone, Serialize)]
pub struct Created {
    pub project: String,
    pub git: GitLink,
    /// Its first deployment, building: the production branch's newest commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<Started>,
    /// Why there's no first deployment, when its build couldn't be started. The project is there
    /// all the same, and its next push deploys.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deploy_error: Option<String>,
    /// Why its env vars weren't (all) saved, when the store failed. The project is there, and
    /// nothing was deployed without them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_error: Option<String>,
}

/// Make a project from a repository the app reaches, as Vercel's import does: the project is
/// connected from the start, gets the env vars given, and its production branch is deployed at
/// once. Refused if a project of that name exists.
///
/// Everything that can be refused is checked before the project is made. Past that point the
/// project stays whatever fails, and the answer says what didn't happen: it is never taken back,
/// which the control function's role couldn't do (it deletes no project file).
pub async fn create(ctx: &Ctx, api: &Api, c: Create) -> Result<Created> {
    check_project(&c.project)?;
    env_vars::check_new(&c.env)?;
    let key = state::project_key(&c.project);
    let taken = || anyhow::anyhow!("a project named {} exists already: pick another name, or connect the repository in that project's settings", c.project);
    if ctx.store.exists(&key).await? {
        return Err(taken());
    }
    // A project renamed to it, or whose domain its address is, answers to the name too.
    crate::project::check_new(ctx, &c.project).await?;
    // A function of that name that isn't this project's (in any region) is refused here, not at
    // the first build's end.
    let function = lambda::function_name(&c.project);
    ctx.lambda.check(&function, &c.project).await?;
    for region in ctx.store.settings().await?.regions.iter().filter(|r| **r != ctx.install.home_region) {
        ctx.lambda_in(region).check(&function, &c.project).await?;
    }
    let link = link_to(api, &c.repo, c.branch.as_deref(), c.root.as_deref(), c.actor.clone()).await?;
    let mut project = new_project(&c.project);
    project.git = Some(link.clone());
    project.framework = c.framework.clone();
    if !ctx.store.create(&key, &project).await? {
        return Err(taken());
    }
    if !c.env.is_empty()
        && let Err(e) = env_vars::set(ctx, &c.project, &c.env, &[EnvTarget::Production, EnvTarget::Preview], None, false).await
    {
        // The store failed (the values themselves were checked): no deployment without them.
        return Ok(Created { project: c.project, git: link, deployment: None, deploy_error: None, env_error: Some(format!("{e:#}")) });
    }
    let push = Push { project: c.project.clone(), repo_id: link.repo_id, branch: link.production_branch.clone(), sha: None, before: None, pusher: c.actor, hook: None, no_cache: false, webhook: false, pusher_role: None };
    let (deployment, deploy_error) = match pushed(ctx, api, push).await {
        Ok(Pushed::Started(started)) => (Some(started), None),
        Ok(Pushed::Skipped(why)) => (None, Some(why)),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    Ok(Created { project: c.project, git: link, deployment, deploy_error, env_error: None })
}

/// A push to a project's connected repository.
#[derive(Debug, Clone)]
pub struct Push {
    pub project: String,
    /// GitHub's id of the repository pushed to: checked against the project's own.
    pub repo_id: u64,
    pub branch: String,
    /// The commit the branch is at after the push, by its whole hash; the branch's newest if not
    /// given.
    pub sha: Option<String>,
    /// The commit the branch was at before the push, by its whole hash, when GitHub said: builds
    /// get it as VERCEL_GIT_PREVIOUS_SHA.
    pub before: Option<String>,
    /// Who pushed: their GitHub username.
    pub pusher: Option<String>,
    /// Not a push: one of the project's deploy hooks was called, for its branch's newest commit.
    pub hook: Option<HookRef>,
    /// Build without the project's build cache (a deploy hook's `?buildCache=false`).
    pub no_cache: bool,
    /// A push GitHub's webhook told of, with the pusher's role in the team (`None`: they aren't
    /// in it, or the dashboard didn't say). Such a push is anyone's with write access to the
    /// repository, so it builds a project whose code acts with more than the shared worker role
    /// only when its pusher may deploy that (`push_refused`). False for the rest (a hook, `forkrail
    /// git deploy`, connecting a repository), whose asker was checked already.
    pub webhook: bool,
    pub pusher_role: Option<String>,
}

impl Push {
    /// How its deployment is recorded as asked for.
    fn via(&self) -> Via {
        if self.hook.is_some() { Via::Hook } else { Via::Git }
    }
}

/// Why a push GitHub told of may not build `project`, if it may not, with `role` the pusher's in
/// the team (`None`: not a member). Write access to the repository is enough for a project whose
/// functions run as the shared worker role, as on Vercel. A project with an AWS role or private
/// egress of its own runs every deployment's code with them, previews too: a member's or an
/// owner's push only, as a deploy from the dashboard or a terminal is. One that runs as one of
/// forkrail's own roles (the dashboard's `forkrail-dashboard` asks forkrail-control for anything,
/// as any role) acts as the install's owners: an owner's push only.
pub fn push_refused(project: &crate::state::Project, role: Option<&str>) -> Option<String> {
    let name = |r: &str| r.rsplit('/').next().unwrap_or(r).to_owned();
    let own = project.role.as_deref().filter(|r| name(r) != "forkrail-worker");
    let owners_only = own.is_some_and(|r| name(r).starts_with("forkrail-"));
    if own.is_none() && project.egress.is_empty() {
        return None;
    }
    let may = match role {
        Some("owner") => true,
        Some("member") => !owners_only,
        _ => false,
    };
    (!may).then(|| {
        let who = if owners_only { "an owner of the team" } else { "a member or owner of the team" };
        let why = if owners_only { "runs as one of forkrail's own roles" } else { "runs with an AWS role or private egress of its own" };
        format!("{} {why}: only a push by {who} deploys it (their GitHub account, as signed in to the dashboard); anyone else's waits for one of them to deploy it", project.name)
    })
}

/// What a push came to.
#[derive(Debug, Clone)]
pub enum Pushed {
    /// A deployment of the commit, building.
    Started(Started),
    /// No deployment: the project's `vercel.json` turns pushes to this branch off. Why, in words.
    Skipped(String),
}

/// Deploy what was pushed: fetch the commit's files and start their build in the builder. The
/// commit's status on GitHub says it's building, or why it couldn't start, and the branch's open
/// pull requests say so in the project's comment. A branch the project's `vercel.json` turns off
/// (`git.deploymentEnabled`) gets no deployment and nothing said on GitHub, as on Vercel.
pub async fn pushed(ctx: &Ctx, api: &Api, push: Push) -> Result<Pushed> {
    check_project(&push.project)?;
    check_branch(&push.branch)?;
    for sha in push.sha.iter().chain(&push.before) {
        if !is_sha(sha) {
            bail!("{sha:?} isn't a commit's hash");
        }
    }
    let project = project_record(ctx, &push.project).await?;
    let refused = push.webhook.then(|| push_refused(&project, push.pusher_role.as_deref())).flatten();
    let Some(link) = project.git else { bail!("{} has no connected repository", push.project) };
    if link.repo_id != push.repo_id {
        bail!("{} is connected to {}, not to the repository pushed to", push.project, link.repo);
    }
    let token = api.token(link.installation_id).await?;
    if let Some(why) = refused {
        // Said on the commit, so whoever pushed sees why nothing deployed.
        if let Some(sha) = &push.sha
            && let Err(e) = api.set_status(&token, link.repo_id, sha, &push.project, "error", &why, None).await
        {
            eprintln!("warning: {e:#}");
        }
        return Ok(Pushed::Skipped(why));
    }
    // A branch by its full name: a bare one could be a tag of the same name, which branch
    // protection doesn't cover, taking the production branch's place.
    let rev = push.sha.clone().unwrap_or_else(|| format!("heads/{}", push.branch));
    let Some(commit) = api.commit(&token, link.repo_id, &rev).await.with_context(|| format!("reading commit {rev}"))? else {
        bail!("{} has no commit {rev}", link.repo);
    };
    let settings = ctx.store.settings().await?;
    let target = if push.branch == link.production_branch { Target::Production } else { Target::Preview };
    let git = GitCommit {
        repo: repo_of(&commit.html_url).unwrap_or_else(|| link.repo.clone()),
        branch: push.branch.clone(),
        sha: commit.sha.chars().take(7).collect(),
        message: commit.commit.message.lines().next().unwrap_or_default().trim().to_owned(),
        full_sha: Some(commit.sha.clone()),
        author: commit.commit.author.as_ref().and_then(|a| a.name.clone()),
    };
    // A hook's deployment is nobody's: the commit's author didn't ask for it.
    let deployer = match push.hook {
        Some(_) => None,
        None => push.pusher.clone().or_else(|| commit.author.as_ref().map(|a| a.login.clone())),
    };
    // Recorded before the commit's files are fetched, so it's in the list (building) a couple of
    // seconds sooner; the build is sent once they're uploaded.
    let reserve = remote::Reserve {
        project: push.project.clone(),
        target,
        via: push.via(),
        deployer: deployer.clone(),
        git: Some(git.clone()),
        redeploy_of: None,
        upload: None,
        github_deployment: None,
        deploy_hook: push.hook.clone(),
    };
    let built = match remote::reserve(ctx, reserve).await {
        Ok(reserved) => build(ctx, api, &token, &link, &push, &commit, &reserved, git, deployer).await,
        Err(e) => Err(e),
    };
    match built {
        Ok(Built::Started(started, github_deployment)) => {
            let what = if started.target == Target::Production { "Building for production" } else { "Building a preview" };
            let page = page_of(&settings, &push.project, &started.id);
            if let Err(e) = api.set_status(&token, link.repo_id, &commit.sha, &push.project, "pending", what, page.as_deref()).await {
                eprintln!("warning: {e:#}");
            }
            if let Some(deployment) = github_deployment
                && let Err(e) = api.deployment_status(&token, link.repo_id, deployment, Stand { state: "in_progress", description: what, log: page.as_deref(), address: None }).await
            {
                eprintln!("warning: {e:#}");
            }
            // After the build is on its way: none of this holds it up.
            let repo = repo_of(&commit.html_url).unwrap_or_else(|| link.repo.clone());
            let name = project.display_name.as_deref().unwrap_or(&push.project);
            let note = Note { project: &push.project, name, status: "⏳ Building".to_owned(), preview: None, page, sha: &commit.sha };
            if let Err(e) = comment(api, &token, link.repo_id, &repo, &push.branch, &note).await {
                eprintln!("warning: {e:#}");
            }
            Ok(Pushed::Started(started))
        }
        Ok(Built::Stopped(started)) => {
            // Its commit says canceled, as one canceled while building would.
            if let Err(e) = report_with(ctx, &started.id, api).await {
                eprintln!("warning: telling GitHub deployment {} was canceled: {e:#}", started.id);
            }
            Ok(Pushed::Started(started))
        }
        Ok(Built::Off) => Ok(Pushed::Skipped(format!("{}'s vercel.json turns deployments off for pushes to {} (git.deploymentEnabled)", push.project, push.branch))),
        Err(e) => {
            let why = format!("Couldn't start: {e:#}");
            if let Err(e) = api.set_status(&token, link.repo_id, &commit.sha, &push.project, "error", &why, None).await {
                eprintln!("warning: {e:#}");
            }
            Err(e)
        }
    }
}

/// What fetching a pushed commit came to.
enum Built {
    /// Its build was sent to the builder: the deployment, and GitHub's deployment for it, if
    /// GitHub took one.
    Started(Started, Option<u64>),
    /// The project's `vercel.json` at this commit turns pushes to the branch off.
    Off,
    /// Canceled (by a newer push) before its build was sent.
    Stopped(Started),
}

/// A deployment's page in the dashboard, where its build log is, if the install has a dashboard.
fn page_of(settings: &Settings, project: &str, id: &str) -> Option<String> {
    settings.dashboard.as_deref().map(|base| format!("{}/projects/{project}/deployments/{id}", base.trim_end_matches('/')))
}

/// Fetch the commit and send its build to the builder, unless the project's own `vercel.json` at
/// that commit says pushes to the branch don't deploy: then the deployment recorded for it goes.
/// If the build can't be sent, the deployment ends as failed, saying why.
#[allow(clippy::too_many_arguments)]
async fn build(ctx: &Ctx, api: &Api, token: &str, link: &GitLink, push: &Push, commit: &Commit, reserved: &remote::Reserved, git: GitCommit, deployer: Option<String>) -> Result<Built> {
    let fetched = async {
        let tmp = TempDir::new("push")?;
        let top = api.fetch(token, link.repo_id, &commit.sha, tmp.path()).await?;
        let dir = project_dir(&top, link.root.as_deref())?;
        if !vercel::push_deploys(&dir, &push.branch) {
            return Ok(None);
        }
        let github = vercel::github_keys(&dir);
        let upload = remote::upload(ctx, &dir, &push.project).await?;
        anyhow::Ok(Some((github, upload)))
    }
    .await;
    let (github, upload) = match fetched {
        Ok(Some(found)) => found,
        Ok(None) => {
            if let Err(e) = remote::forget(ctx, reserved).await {
                eprintln!("warning: forgetting deployment {}: {e:#}", reserved.id);
            }
            return Ok(Built::Off);
        }
        Err(e) => {
            remote::abandon(ctx, reserved, &format!("the commit's files couldn't be fetched: {e:#}")).await;
            return Err(e);
        }
    };
    let target = reserved.started().target;
    // A newer push canceled it while its files were fetched: nothing to tell GitHub but that.
    if remote::was_stopped(ctx, reserved).await.unwrap_or(false) {
        return Ok(Built::Stopped(reserved.started()));
    }
    // GitHub's deployment before the build, so the record holds its id from the start. Without one
    // (an app made before it asked for deployments, GitHub failing) the commit's status still says it all.
    let github_deployment = match api.create_deployment(token, link.repo_id, &commit.sha, &push.project, target).await {
        Ok(id) => Some(id),
        Err(e) => {
            eprintln!("warning: {e:#}");
            None
        }
    };
    let start = Start {
        project: push.project.clone(),
        target,
        upload,
        git: Some(git),
        deployer,
        // `github.autoAlias: false`: the production branch's deployments leave production where it is.
        publish: PublishOptions { skip_domain: target == Target::Production && !github.auto_alias, ..PublishOptions::default() },
        via: push.via(),
        no_cache: push.no_cache,
        github_deployment,
        previous_sha: push.before.clone(),
    };
    match remote::launch(ctx, reserved, start).await {
        Ok(Launched::Stopped(started)) => Ok(Built::Stopped(started)),
        Ok(Launched::Sent(started)) => {
            let canceled = match &push.hook {
                // The same hook called again for the same commit: its earlier build is pointless.
                Some(hook) => cancel_older_of_hook(ctx, &push.project, &hook.id, &commit.sha, &started.id).await,
                // A newer push makes the branch's older builds pointless (`github.autoJobCancelation`).
                None if github.auto_job_cancelation => cancel_older(ctx, &push.project, &push.branch, &started.id).await,
                None => Ok(()),
            };
            if let Err(e) = canceled {
                eprintln!("warning: canceling {}'s older builds: {e:#}", push.branch);
            }
            Ok(Built::Started(started, github_deployment))
        }
        Err(e) => {
            let why = format!("Couldn't start: {e:#}");
            if let Some(deployment) = github_deployment
                && let Err(e) = api.deployment_status(token, link.repo_id, deployment, Stand { state: "error", description: &why, log: None, address: None }).await
            {
                eprintln!("warning: {e:#}");
            }
            Err(e)
        }
    }
}

/// Cancel the project's builds of `branch` still running in the builder, but `newest`.
async fn cancel_older(ctx: &Ctx, project: &str, branch: &str, newest: &str) -> Result<()> {
    let record = project_record(ctx, project).await?;
    // Only those made before it (the list is newest first): a later push recorded while this one's
    // files were fetched is newer, and stays.
    for summary in record.deployments.iter().skip_while(|d| d.id != newest).skip(1).filter(|d| d.status.in_progress()) {
        let rec = crate::deploy::record(ctx, &summary.id).await?;
        if rec.builder.is_some() && rec.git.as_ref().is_some_and(|g| g.branch == branch) && rec.status_now().in_progress() {
            match remote::cancel(ctx, &rec.id).await {
                Ok(_) => eprintln!("{project}: canceled {}, an older build of {branch}", rec.id),
                Err(e) => eprintln!("warning: canceling {}: {e:#}", rec.id),
            }
        }
    }
    Ok(())
}

/// Cancel the project's builds still running that deploy hook `hook` started for the same commit,
/// but `newest`: calling a hook again for one commit builds it once, as on Vercel.
async fn cancel_older_of_hook(ctx: &Ctx, project: &str, hook: &str, sha: &str, newest: &str) -> Result<()> {
    let record = project_record(ctx, project).await?;
    for summary in record.deployments.iter().skip_while(|d| d.id != newest).skip(1).filter(|d| d.status.in_progress()) {
        let rec = crate::deploy::record(ctx, &summary.id).await?;
        let same_commit = rec.git.as_ref().and_then(|g| g.full_sha.as_deref()) == Some(sha);
        if rec.deploy_hook.as_ref().is_some_and(|h| h.id == hook) && same_commit && rec.status_now().in_progress() {
            match remote::cancel(ctx, &rec.id).await {
                Ok(_) => eprintln!("{project}: canceled {}, an earlier build of the same commit by the same deploy hook", rec.id),
                Err(e) => eprintln!("warning: canceling {}: {e:#}", rec.id),
            }
        }
    }
    Ok(())
}

/// How a deployment that has ended stands, in the words each place on GitHub takes.
struct Standing {
    /// The commit status's state, and the state of GitHub's own deployment.
    commit: &'static str,
    deployment: &'static str,
    description: String,
    /// Where it answers, once ready: what "View deployment" opens.
    address: Option<String>,
    /// For the pull request's comment: the state with its mark, and where to look at it.
    status: String,
    preview: Option<String>,
}

/// `None` while the deployment is still being made.
fn standing(rec: &DeploymentRecord, project: &Project, settings: &Settings) -> Option<Standing> {
    Some(match rec.status_now() {
        Status::Ready => {
            let live = project.production.as_deref() == Some(rec.id.as_str());
            let description = match (rec.target, live) {
                (Target::Production, true) => "Production is at this commit",
                (Target::Production, false) => "Ready; production stayed on a newer deployment",
                (Target::Preview, _) => "Preview ready",
            };
            // Production's own name when it's live there, else the deployment's own. The pull
            // request's comment links the branch's host, which stays as the branch moves on.
            let own = if live { rec.hosts.iter().rfind(|h| !h.contains(BRANCH_INFIX)) } else { rec.hosts.first() };
            let branch = rec.hosts.iter().find(|h| h.contains(BRANCH_INFIX));
            Standing {
                commit: "success",
                deployment: "success",
                description: description.to_owned(),
                address: own.map(|h| settings.url(h)),
                status: "✅ Ready".to_owned(),
                preview: branch.or(own).map(|h| settings.url(h)),
            }
        }
        Status::Error => {
            let error = rec.error.as_ref();
            let step = error.and_then(|e| e.step.as_deref()).map(|s| format!("{s}: ")).unwrap_or_default();
            let description = format!("{step}{}", error.map_or("the build failed", |e| e.message.as_str()));
            Standing { commit: "failure", deployment: "failure", status: format!("❌ Failed: {}", cut(&description, MAX_DESCRIPTION)), description, address: None, preview: None }
        }
        // The project's own ignoreCommand called it off: nothing is wrong with the commit, and a
        // check that must pass before merging isn't held up by it.
        Status::Canceled if rec.error.is_some() => Standing {
            commit: "success",
            deployment: "inactive",
            description: "Skipped by the project's ignoreCommand".to_owned(),
            address: None,
            status: "⏭ Skipped by the project's ignoreCommand".to_owned(),
            preview: None,
        },
        // Someone stopped it: nothing failed, and nothing is there to open.
        Status::Canceled => Standing { commit: "error", deployment: "inactive", description: "Canceled".to_owned(), address: None, status: "⏹ Canceled".to_owned(), preview: None },
        Status::Queued | Status::Building => return None,
    })
}

/// What a project's comment on a pull request says of the branch's newest deployment.
#[derive(Clone)]
struct Note<'a> {
    /// The project's id: its comment's marker.
    project: &'a str,
    /// What it's called now, shown in the comment.
    name: &'a str,
    /// `Building`, `Ready` and so on, with its mark.
    status: String,
    /// Where it answers, once it does.
    preview: Option<String>,
    /// Its page in the dashboard, with its build log.
    page: Option<String>,
    /// The commit, by its hash.
    sha: &'a str,
}

/// What every comment of a project starts with, unseen on the page: how its own comment is found
/// again. One per project, since a repository may hold several.
fn marker(project: &str) -> String {
    format!("<!-- forkrail:{project} -->")
}

/// The comment itself: one table row, as Vercel's. `now` is an RFC 3339 time.
fn comment_body(note: &Note<'_>, now: &str) -> String {
    let cell = |text: &str| text.replace('|', "\\|").replace(['\n', '\r'], " ");
    let status = match &note.page {
        Some(page) => format!("{} ([build log]({page}))", cell(&note.status)),
        None => cell(&note.status),
    };
    let preview = match &note.preview {
        Some(url) => format!("[{}]({url})", url.split_once("://").map_or(url.as_str(), |(_, host)| host)),
        None => "–".to_owned(),
    };
    let sha: String = note.sha.chars().take(7).collect();
    let updated = now.get(..16).unwrap_or(now).replace('T', " ");
    format!(
        "{}\n**{}** on forkrail\n\n| Status | Preview | Commit | Updated (UTC) |\n| :-- | :-- | :-- | :-- |\n| {status} | {preview} | `{sha}` | {updated} |\n",
        marker(note.project),
        note.name
    )
}

/// Put the project's comment on every open pull request of `branch` in the repository
/// (`owner/name`): how many carry it now.
async fn comment(api: &Api, token: &str, repo_id: u64, repo: &str, branch: &str, note: &Note<'_>) -> Result<usize> {
    let owner = repo.split('/').next().unwrap_or_default();
    let pulls = api.open_pulls(token, repo_id, owner, branch).await?;
    let (marker, body) = (marker(note.project), comment_body(note, &now_rfc3339()));
    for number in &pulls {
        api.put_comment(token, repo_id, *number, &marker, &body).await?;
    }
    Ok(pulls.len())
}

/// The note for a deployment as it stands now.
fn note_of<'a>(rec: &'a DeploymentRecord, project: &'a Project, settings: &Settings) -> Note<'a> {
    let sha = rec.git.as_ref().map_or("", |g| g.full_sha.as_deref().unwrap_or(&g.sha));
    let page = page_of(settings, &rec.project, &rec.id);
    match standing(rec, project, settings) {
        Some(s) => Note { project: &rec.project, name: project.shown_name(), status: s.status, preview: s.preview, page, sha },
        None => Note { project: &rec.project, name: project.shown_name(), status: "⏳ Building".to_owned(), preview: None, page, sha },
    }
}

/// Tell GitHub how a deployment that a push made ended: a status on its commit, linking to the
/// deployment once it's ready, else to its build log in the dashboard; the same of GitHub's own
/// deployment for it, whose address is what "View deployment" opens; and the project's comment on
/// the branch's open pull requests. What was set, or `None` for a deployment made any other way
/// (GitHub is asked nothing then, and `api` isn't called).
pub async fn report<F, Fut>(ctx: &Ctx, id: &str, api: F) -> Result<Option<String>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Arc<Api>>>,
{
    let Some(said) = to_say(ctx, id).await? else { return Ok(None) };
    let api = api().await?;
    say(ctx, said, &api).await.map(Some)
}

/// `report`, with GitHub's client in hand.
async fn report_with(ctx: &Ctx, id: &str, api: &Api) -> Result<Option<String>> {
    let Some(said) = to_say(ctx, id).await? else { return Ok(None) };
    say(ctx, said, api).await.map(Some)
}

/// What GitHub is to be told of an ended deployment of a push.
struct ToSay {
    rec: DeploymentRecord,
    git: GitCommit,
    sha: String,
    project: Project,
    link: GitLink,
    page: Option<String>,
    stands: Standing,
    settings: Settings,
}

async fn to_say(ctx: &Ctx, id: &str) -> Result<Option<ToSay>> {
    let rec = deploy::record(ctx, id).await?;
    if !rec.via.is_some_and(Via::from_repository) {
        return Ok(None);
    }
    let Some(git) = rec.git.clone() else { return Ok(None) };
    let Some(sha) = git.full_sha.clone() else { return Ok(None) };
    let project = project_record(ctx, &rec.project).await?;
    // Disconnected since: the commit keeps the status it had.
    let Some(link) = project.git.clone() else { return Ok(None) };
    let settings = ctx.store.settings().await?;
    let page = page_of(&settings, &rec.project, id);
    let Some(stands) = standing(&rec, &project, &settings) else { return Ok(None) };
    Ok(Some(ToSay { rec, git, sha, project, link, page, stands, settings }))
}

async fn say(ctx: &Ctx, s: ToSay, api: &Api) -> Result<String> {
    let ToSay { rec, git, sha, project, link, page, stands, settings } = s;
    let token = api.token(link.installation_id).await?;
    let mut set = format!("{}: {}", stands.commit, stands.description);
    // GitHub's deployment first: a failure there is said, and the commit's status is set all the same.
    if let Some(deployment) = rec.github_deployment {
        let stand = Stand { state: stands.deployment, description: &stands.description, log: page.as_deref(), address: stands.address.as_deref() };
        match api.deployment_status(&token, link.repo_id, deployment, stand).await {
            Ok(()) => set.push_str(&format!(" (GitHub deployment {deployment}: {})", stands.deployment)),
            Err(e) => set.push_str(&format!(" (GitHub deployment {deployment} not told: {e:#})")),
        }
    }
    api.set_status(&token, link.repo_id, &sha, &rec.project, stands.commit, &stands.description, stands.address.clone().or(page).as_deref()).await?;
    // The comment says the branch's newest deployment: one that ended after a newer was started
    // leaves it to that one.
    if newest_of_branch(ctx, &project, &git.branch).await?.is_none_or(|newest| newest.id == rec.id) {
        match comment(api, &token, link.repo_id, &git.repo, &git.branch, &note_of(&rec, &project, &settings)).await {
            Ok(0) => {}
            Ok(pulls) => set.push_str(&format!(" (comment on {pulls} pull request{})", if pulls == 1 { "" } else { "s" })),
            Err(e) => set.push_str(&format!(" (pull requests not told: {e:#})")),
        }
    }
    Ok(set)
}

/// The newest deployment a push to `branch` made, among the project's most recent.
async fn newest_of_branch(ctx: &Ctx, project: &Project, branch: &str) -> Result<Option<DeploymentRecord>> {
    let ids: Vec<String> = project.deployments.iter().take(BRANCH_LOOKBACK).map(|d| d.id.clone()).collect();
    let mut found: HashMap<String, DeploymentRecord> = storage::records(ctx, &ids).await?.into_iter().map(|r| (r.id.clone(), r)).collect();
    Ok(ids.iter().filter_map(|id| found.remove(id)).find(|r| r.via.is_some_and(Via::from_repository) && r.git.as_ref().is_some_and(|g| g.branch == branch)))
}

/// A pull request that was opened, or opened again.
#[derive(Debug, Clone)]
pub struct PullRequest {
    pub project: String,
    /// GitHub's id of the repository it's in: checked against the project's own.
    pub repo_id: u64,
    pub number: u64,
    /// Its head: the branch it asks to merge.
    pub branch: String,
}

/// Put the project's comment on a pull request that was just opened: how the newest deployment of
/// its branch stands (later pushes keep it up to date). The deployment it says, or `None` when
/// the branch has none yet, or the pull request isn't an open one of that branch.
pub async fn pull_request(ctx: &Ctx, api: &Api, pr: PullRequest) -> Result<Option<String>> {
    check_project(&pr.project)?;
    check_branch(&pr.branch)?;
    let project = project_record(ctx, &pr.project).await?;
    let Some(link) = project.git.clone() else { bail!("{} has no connected repository", pr.project) };
    if link.repo_id != pr.repo_id {
        bail!("{} is connected to {}, not to the pull request's repository", pr.project, link.repo);
    }
    let Some(rec) = newest_of_branch(ctx, &project, &pr.branch).await? else { return Ok(None) };
    let token = api.token(link.installation_id).await?;
    // The repository's name as GitHub last gave it, should it have been renamed since connecting.
    let repo = rec.git.as_ref().map_or(link.repo.as_str(), |g| g.repo.as_str());
    let owner = repo.split('/').next().unwrap_or_default();
    if !api.open_pulls(&token, link.repo_id, owner, &pr.branch).await?.contains(&pr.number) {
        return Ok(None);
    }
    let settings = ctx.store.settings().await?;
    let note = note_of(&rec, &project, &settings);
    api.put_comment(&token, link.repo_id, pr.number, &marker(&pr.project), &comment_body(&note, &now_rfc3339())).await?;
    Ok(Some(rec.id.clone()))
}

/// What deleting a branch removed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Cleaned {
    /// Deployments removed, with their Lambda versions and the files only they used.
    pub removed: Vec<String>,
    /// Builds that were still running, stopped.
    pub canceled: Vec<String>,
    /// The branch's host, which no longer answers, when anything of the branch's was here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// A branch of a project's connected repository was deleted: its previews go, and its host with
/// them. Kept: anything that is or has been production (a preview that was promoted is a rollback
/// target), and deployments made by hand from a checkout of the branch, which nobody pushed.
pub async fn branch_deleted(ctx: &Ctx, project_name: &str, repo_id: u64, branch: &str) -> Result<Cleaned> {
    check_project(project_name)?;
    check_branch(branch)?;
    let project = project_record(ctx, project_name).await?;
    let Some(link) = project.git.clone() else { bail!("{project_name} has no connected repository") };
    if link.repo_id != repo_id {
        bail!("{project_name} is connected to {}, not to the repository the branch was in", link.repo);
    }
    // The production branch has no previews and no host of its own; its deployments are production's.
    if branch == link.production_branch {
        return Ok(Cleaned::default());
    }
    let settings = ctx.store.settings().await?;
    let host = branch_host(project_name, branch, &settings.base_domain);
    let previews: Vec<String> = project.deployments.iter().filter(|d| d.target == Target::Preview).map(|d| d.id.clone()).collect();
    let of_the_branch = |r: &DeploymentRecord| {
        r.git.as_ref().is_some_and(|g| g.branch == branch) && (r.via.is_some_and(Via::from_repository) || r.redeploy_of.is_some() || r.hosts.contains(&host))
    };
    let kept_as_production = |id: &str| project.production.as_deref() == Some(id) || project.production_history.iter().any(|d| d == id);
    let mut cleaned = Cleaned::default();
    for rec in storage::records(ctx, &previews).await?.into_iter().filter(of_the_branch).filter(|r| !kept_as_production(&r.id)) {
        if !rec.status_now().in_progress() {
            cleaned.removed.push(rec.id);
        } else if rec.builder.is_some() && remote::cancel(ctx, &rec.id).await.is_ok() {
            cleaned.canceled.push(rec.id);
        }
    }
    cleaned.removed.sort();
    if !cleaned.removed.is_empty() {
        deploy::remove_deployments(ctx, project_name, &cleaned.removed).await?;
    }
    // The host goes even where what it pointed at stays (a preview that was promoted).
    let mut had_host = false;
    ctx.store
        .update::<Routes, _>(state::ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            had_host = routes.lookup(&host).is_some_and(|(_, d)| d.project == project_name);
            if had_host {
                routes.hosts.remove(&host);
            }
            // The branch's domains stay, serving nothing until a branch of that name is pushed again.
            let domains: Vec<String> =
                routes.domains.iter().filter(|(_, d)| d.project == project_name && d.branch.as_deref() == Some(branch)).map(|(n, _)| n.clone()).collect();
            for domain in domains {
                routes.hosts.remove(&domain);
            }
            Ok(routes)
        })
        .await?;
    cleaned.host = (had_host || !cleaned.removed.is_empty()).then_some(host);
    Ok(cleaned)
}

/// `forkrail git repos [QUERY]`.
pub async fn print_repos(ctx: &Ctx, query: Option<&str>) -> Result<()> {
    let api = Api::connect(ctx).await?;
    let listing = api.repos(query, true).await?;
    if listing.accounts.is_empty() {
        println!("{} isn't installed on any GitHub account yet. Install it at:\n  {}", listing.app.name, listing.app.install_url);
        return Ok(());
    }
    for account in &listing.accounts {
        let reach = if account.suspended { "suspended" } else if account.all { "all repositories" } else { "selected repositories" };
        println!("{}  ({reach}; change at {})", account.login, account.manage_url);
    }
    for repo in &listing.repos {
        println!("  {}{}  {}", repo.full_name, if repo.private { " (private)" } else { "" }, repo.default_branch);
    }
    if listing.total > listing.repos.len() {
        println!("  … and {} more: narrow them with `forkrail git repos QUERY`", listing.total - listing.repos.len());
    }
    Ok(())
}

/// The accounts the app is installed on, for `forkrail git status`.
pub async fn print_accounts(ctx: &Ctx) -> Result<()> {
    let api = Api::connect(ctx).await?;
    let accounts = api.accounts().await?;
    if accounts.is_empty() {
        println!("  installed on: no account yet; to connect repositories, install it at {}", api.info().install_url);
    }
    for account in accounts {
        let reach = if account.suspended { "suspended" } else if account.all { "all repositories" } else { "selected repositories" };
        println!("  installed on: {} ({reach})", account.login);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    use super::*;

    #[test]
    fn a_push_builds_code_that_acts_with_more_only_for_who_may_deploy_it() {
        let plain = crate::state::Project { name: "shop".into(), ..Default::default() };
        let worker = crate::state::Project { name: "shop".into(), role: Some("arn:aws:iam::1:role/forkrail-worker".into()), ..Default::default() };
        let own_role = crate::state::Project { name: "api".into(), role: Some("arn:aws:iam::1:role/orders-app".into()), ..Default::default() };
        let egress = crate::state::Project { name: "db".into(), egress: vec!["postgres".into()], ..Default::default() };
        let dashboard = crate::state::Project { name: "forkrail-web".into(), role: Some("arn:aws:iam::1:role/forkrail-dashboard".into()), ..Default::default() };
        // Write access to the repository is enough where the code acts with nothing.
        for p in [&plain, &worker] {
            for role in [None, Some("viewer"), Some("developer"), Some("member"), Some("owner")] {
                assert_eq!(push_refused(p, role), None, "{role:?}");
            }
        }
        // A role or egress of its own: a member's or an owner's push.
        for p in [&own_role, &egress] {
            for role in [None, Some("viewer"), Some("developer"), Some("someone")] {
                assert!(push_refused(p, role).is_some(), "{role:?}");
            }
            assert_eq!(push_refused(p, Some("member")), None);
            assert_eq!(push_refused(p, Some("owner")), None);
        }
        // forkrail's own role acts as the owners: an owner's push only.
        for role in [None, Some("viewer"), Some("developer"), Some("member")] {
            let why = push_refused(&dashboard, role).expect("refused");
            assert!(why.contains("an owner of the team"), "{why}");
        }
        assert_eq!(push_refused(&dashboard, Some("owner")), None);
    }

    /// A key made for this test alone, with openssl; `None` on a machine without it.
    fn test_key() -> Option<String> {
        let out = std::process::Command::new("openssl").args(["genrsa", "-traditional", "2048"]).stderr(std::process::Stdio::null()).output().ok()?;
        let pem = String::from_utf8(out.stdout).ok()?;
        (out.status.success() && pem.contains("BEGIN RSA PRIVATE KEY")).then_some(pem)
    }

    fn api(pem: &str) -> Api {
        let app: App = serde_json::from_value(json!({
            "id": 4242, "slug": "forkrail-test", "name": "forkrail (test)", "client_id": "Iv1.test", "html_url": "https://github.com/apps/forkrail-test",
            "owner": {"id": 1, "login": "ada"}, "callback": "https://dash.example.com/auth/github/callback", "callbacks": [], "webhook": "",
        }))
        .unwrap();
        Api { http: Http::new().unwrap(), base: "https://api.github.example".into(), app, key: key_of(pem).unwrap(), tokens: Mutex::default(), listed: Mutex::default() }
    }

    /// A stand-in for GitHub's archive of a commit: the API's address wants the token and
    /// redirects to another host's, which must get none. Returns its port.
    async fn serve_archive(zip: Vec<u8>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let zip = zip.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf).await {
                            Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
                            _ => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&head).to_lowercase();
                    let path = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
                    let token = head.contains("authorization: bearer tok");
                    let answer = match (path.as_str(), token) {
                        ("/repositories/7/zipball/0123456789abcdef0123456789abcdef01234567", true) => {
                            format!("HTTP/1.1 302 Found\r\nlocation: http://localhost:{port}/codeload\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()
                        }
                        ("/codeload", false) => {
                            let mut answer = format!("HTTP/1.1 200 OK\r\ncontent-type: application/zip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", zip.len()).into_bytes();
                            answer.extend_from_slice(&zip);
                            answer
                        }
                        _ => {
                            let body = r#"{"message":"Bad token"}"#;
                            format!("HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
                        }
                    };
                    let _ = stream.write_all(&answer).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn a_commits_files_come_from_github_and_the_token_stays_with_its_api() {
        let Some(pem) = test_key() else {
            eprintln!("skipped: no openssl here");
            return;
        };
        // GitHub's archive: one folder named for the commit, holding the repository.
        let tmp = TempDir::new("fetch-test").unwrap();
        let zip_path = tmp.path().join("made.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        for (name, text) in [
            ("ada-engine-0123456/api/main.py", "app = None\n"),
            ("ada-engine-0123456/vercel.json", "{}"),
            ("ada-engine-0123456/apps/web/server.mjs", "export default 1\n"),
            ("ada-engine-0123456/apps/web/.env.local", "SECRET=1\n"),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut zip, text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        let port = serve_archive(std::fs::read(&zip_path).unwrap()).await;
        let mut api = api(&pem);
        api.base = format!("http://127.0.0.1:{port}");
        let sha = "0123456789abcdef0123456789abcdef01234567";

        let top = api.fetch("tok", 7, sha, tmp.path()).await.unwrap();
        assert!(top.ends_with("repo/ada-engine-0123456"), "{}", top.display());
        assert!(top.join("api/main.py").is_file() && top.join("apps/web/server.mjs").is_file());
        // What a build of the project in apps/web gets: its own files, as a remote build's source.
        let web = project_dir(&top, Some("apps/web")).unwrap();
        let archive = crate::source::upload_archive(&web, &tmp.path().join("source.zip")).unwrap();
        assert_eq!(archive.files, 1, "server.mjs; never .env.local");
        assert_eq!(crate::source::upload_archive(&top, &tmp.path().join("whole.zip")).unwrap().files, 3);

        // A wrong token is GitHub's refusal, in GitHub's words.
        let other = TempDir::new("fetch-test").unwrap();
        let e = api.fetch("wrong", 7, sha, other.path()).await.unwrap_err();
        assert!(format!("{e:#}").contains("GitHub answered 401: Bad token"), "{e:#}");
    }

    #[test]
    fn the_apps_token_is_a_jwt_its_public_key_verifies() {
        assert!(key_of("not a key").is_err());
        let Some(pem) = test_key() else {
            eprintln!("skipped: no openssl here");
            return;
        };
        let api = api(&pem);
        let jwt = api.jwt().unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(header, json!({"alg": "RS256", "typ": "JWT"}));
        assert_eq!(claims["iss"], "4242");
        let (iat, exp) = (claims["iat"].as_u64().unwrap(), claims["exp"].as_u64().unwrap());
        assert!(iat < now_secs() && exp > now_secs() && exp - iat <= 600, "GitHub takes at most ten minutes");
        let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, api.key.public().as_ref());
        let signed = format!("{}.{}", parts[0], parts[1]);
        public.verify(signed.as_bytes(), &URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).expect("the signature verifies");
    }

    #[test]
    fn names_are_checked_before_they_reach_a_url() {
        for name in ["ada/engine", "vercel/next.js", "a-b/c_d.e"] {
            assert!(check_repo_name(name).is_ok(), "{name}");
        }
        for name in ["engine", "ada/", "/engine", "ada/engine/extra", "ada/..", "ada/en gine", "ada/engine?x=1", ""] {
            assert!(check_repo_name(name).is_err(), "{name}");
        }
        for branch in ["main", "feature/logs", "release-1.2", "fix_#12"] {
            assert!(check_branch(branch).is_ok(), "{branch}");
        }
        for branch in ["", "/main", "main/", "a..b", "a b", "a?b", "a\nb", "a//b", "a:b"] {
            assert!(check_branch(branch).is_err(), "{branch:?}");
        }
        assert_eq!(encode_rev("feature/logs"), "feature/logs");
        assert_eq!(encode_rev("fix_#12%"), "fix_%2312%25");
        assert!(is_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_sha("0123456") && !is_sha("z123456789abcdef0123456789abcdef01234567"));
    }

    #[test]
    fn a_root_directory_stays_inside_the_repository() {
        assert_eq!(check_root("").unwrap(), None);
        assert_eq!(check_root("./").unwrap(), None);
        assert_eq!(check_root("/apps/web/").unwrap().as_deref(), Some("apps/web"));
        assert_eq!(check_root("./web").unwrap().as_deref(), Some("web"));
        for root in ["..", "apps/../..", "apps//web", "apps\\web", "a/./b"] {
            assert!(check_root(root).is_err(), "{root}");
        }
    }

    #[test]
    fn githubs_archive_is_one_folder_and_the_root_is_inside_it() {
        let tmp = TempDir::new("connect-test").unwrap();
        let out = tmp.path().join("repo");
        std::fs::create_dir_all(out.join("ada-engine-0123abc/apps/web")).unwrap();
        let top = top_folder(&out).unwrap();
        assert!(top.ends_with("ada-engine-0123abc"));
        assert_eq!(project_dir(&top, None).unwrap(), top);
        assert_eq!(project_dir(&top, Some("apps/web")).unwrap(), top.join("apps/web"));
        assert!(project_dir(&top, Some("apps/api")).unwrap_err().to_string().contains("no folder apps/api"));
        std::fs::write(out.join("stray.txt"), "").unwrap();
        assert!(top_folder(&out).is_err());
    }

    #[test]
    fn what_github_says_is_what_the_error_says() {
        let e = explain(HttpStatus(404, r#"{"message":"Not Found","documentation_url":"https://docs.github.com/rest","status":"404"}"#.into()).into());
        assert_eq!(e.to_string(), "GitHub answered 404: Not Found");
        assert!(refused(&e, 404) && !refused(&e, 403));
        // Cut short, it's still read.
        let e = explain(HttpStatus(403, r#"{"message":"Resource not accessible by integration","documentation_url":"https://docs.git"#.into()).into());
        assert_eq!(e.to_string(), "GitHub answered 403: Resource not accessible by integration");
        assert_eq!(repo_of("https://github.com/ada/engine/commit/0123abc").as_deref(), Some("ada/engine"));
        assert_eq!(repo_of("https://github.com/ada"), None);
        assert_eq!(cut("first line\nsecond", 140), "first line");
        assert_eq!(cut(&"x".repeat(200), 140).chars().count(), 140);
    }

    fn record(value: Value) -> DeploymentRecord {
        let mut base = json!({"id": "a1b2c3d4", "project": "pyapp", "target": "preview", "created": now_rfc3339(), "via": "git",
            "git": {"repo": "ada/pyapp", "branch": "feature/logs", "sha": "0123456", "message": "m", "full_sha": "0123456789abcdef0123456789abcdef01234567"}});
        base.as_object_mut().unwrap().extend(value.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn how_a_deployment_stands_on_github() {
        let settings = Settings { base_domain: "forkrail.dev".into(), dashboard: Some("https://dash.forkrail.dev/".into()), ..Default::default() };
        let mut project = Project { name: "pyapp".into(), ..Default::default() };

        // A preview that holds its branch's host: "View deployment" opens the deployment itself,
        // and the pull request's comment links the host that follows the branch.
        let ready = record(json!({"status": "ready", "hosts": ["pyapp-a1b2c3d4.forkrail.dev", "pyapp-git-feature-logs.forkrail.dev"]}));
        let s = standing(&ready, &project, &settings).unwrap();
        assert_eq!((s.commit, s.deployment, s.description.as_str()), ("success", "success", "Preview ready"));
        assert_eq!(s.address.as_deref(), Some("https://pyapp-a1b2c3d4.forkrail.dev"));
        assert_eq!(s.preview.as_deref(), Some("https://pyapp-git-feature-logs.forkrail.dev"));
        // One that left the host to a newer deployment links its own.
        let older = record(json!({"status": "ready", "hosts": ["pyapp-a1b2c3d4.forkrail.dev"]}));
        assert_eq!(standing(&older, &project, &settings).unwrap().preview.as_deref(), Some("https://pyapp-a1b2c3d4.forkrail.dev"));

        // Production, live: its own name.
        let live = record(json!({"status": "ready", "target": "production", "hosts": ["pyapp-a1b2c3d4.forkrail.dev", "pyapp.forkrail.dev"]}));
        project.production = Some("a1b2c3d4".into());
        let s = standing(&live, &project, &settings).unwrap();
        assert_eq!((s.description.as_str(), s.address.as_deref()), ("Production is at this commit", Some("https://pyapp.forkrail.dev")));

        let failed = record(json!({"status": "error", "error": {"step": "Building", "message": "`npm run build` failed (exit status: 1)"}}));
        let s = standing(&failed, &project, &settings).unwrap();
        assert_eq!((s.commit, s.deployment), ("failure", "failure"));
        assert_eq!(s.status, "❌ Failed: Building: `npm run build` failed (exit status: 1)");

        // Stopped by someone is an error on the commit; called off by the project's own
        // ignoreCommand is a success there, so a required check doesn't hold a merge up.
        let stopped = record(json!({"status": "canceled"}));
        assert_eq!(standing(&stopped, &project, &settings).map(|s| (s.commit, s.deployment)), Some(("error", "inactive")));
        let skipped = record(json!({"status": "canceled", "error": {"message": crate::build::IGNORED}}));
        let s = standing(&skipped, &project, &settings).unwrap();
        assert_eq!((s.commit, s.deployment, s.status.as_str()), ("success", "inactive", "⏭ Skipped by the project's ignoreCommand"));

        let building = record(json!({"status": "building", "building_at": state::now_rfc3339_ms()}));
        assert!(standing(&building, &project, &settings).is_none());
        assert_eq!(note_of(&building, &project, &settings).status, "⏳ Building");
        assert_eq!(page_of(&settings, "pyapp", "a1b2c3d4").as_deref(), Some("https://dash.forkrail.dev/projects/pyapp/deployments/a1b2c3d4"));
    }

    #[test]
    fn a_pull_requests_comment_is_one_row_found_again_by_its_marker() {
        let note = Note {
            project: "pyapp",
            name: "pyapp",
            status: "✅ Ready".into(),
            preview: Some("https://pyapp-git-feature-logs.forkrail.dev".into()),
            page: Some("https://dash.forkrail.dev/projects/pyapp/deployments/a1b2c3d4".into()),
            sha: "0123456789abcdef0123456789abcdef01234567",
        };
        let body = comment_body(&note, "2026-11-12T15:04:05Z");
        assert!(body.starts_with("<!-- forkrail:pyapp -->\n**pyapp** on forkrail\n"), "{body}");
        // Renamed, the comment shows the new name and is still found by the first one.
        let renamed = comment_body(&Note { name: "py-api", ..note.clone() }, "2026-11-12T15:04:05Z");
        assert!(renamed.starts_with("<!-- forkrail:pyapp -->\n**py-api** on forkrail\n"), "{renamed}");
        let row = body.lines().last().unwrap();
        assert_eq!(
            row,
            "| ✅ Ready ([build log](https://dash.forkrail.dev/projects/pyapp/deployments/a1b2c3d4)) | [pyapp-git-feature-logs.forkrail.dev](https://pyapp-git-feature-logs.forkrail.dev) | `0123456` | 2026-11-12 15:04 |"
        );
        // A failure's words can't break the table, and with no dashboard there's no log to link.
        let failed = Note { project: "pyapp", name: "pyapp", status: "❌ Failed: a | b\nc".into(), preview: None, page: None, sha: "0123456" };
        assert_eq!(comment_body(&failed, "2026-11-12T15:04:05Z").lines().last().unwrap(), "| ❌ Failed: a \\| b c | – | `0123456` | 2026-11-12 15:04 |");
        // Each project of a repository has its own comment.
        assert_ne!(marker("web"), marker("api"));
        assert!(!marker("web-api").starts_with(&marker("web")));
        assert_eq!(encode_query("ada:feature/a b&c#1"), "ada%3Afeature%2Fa%20b%26c%231");
    }

    #[test]
    fn a_projects_deployments_have_their_own_environments_on_github() {
        assert_eq!(environment("atlas", Target::Production), "Production – atlas");
        assert_eq!(environment("atlas", Target::Preview), "Preview – atlas");
        assert_ne!(environment("web", Target::Preview), environment("api", Target::Preview));
    }
}
