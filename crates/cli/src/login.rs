//! Signing this machine in to an install: `forkrail login`, for people who have no AWS credentials
//! and need none. The install's dashboard is also its API (api.rs); a login is a token for it, kept
//! in `~/.config/forkrail/auth.json`, readable by its owner only.
//!
//! The login itself goes through the browser without anything secret passing through it. This
//! machine asks the dashboard to begin and gets a secret it keeps and a short code it shows; it
//! opens the dashboard at that code, where whoever is signed in (with GitHub, as a member of the
//! install's team) says yes; meanwhile this machine asks until the answer is in, and gets its
//! token. It works over SSH too: open the address on any machine and check the code.
//!
//! `FORKRAIL_TOKEN` (with `FORKRAIL_API`, or the address a login saved) is used instead of the
//! file, for a machine nobody sits at.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use hyper::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config;
use crate::deploy::log;
use crate::fetch::Http;

pub const TOKEN_ENV: &str = "FORKRAIL_TOKEN";
pub const API_ENV: &str = "FORKRAIL_API";

/// A login, as it's kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// The dashboard's address, `https://dashboard.example.com`.
    pub api: String,
    pub token: String,
    /// Who it is, as the dashboard said at login: for messages only.
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub team: String,
}

pub fn path() -> Option<PathBuf> {
    Some(config::path()?.with_file_name("auth.json"))
}

/// The saved login, if there is one.
pub fn load() -> Result<Option<Saved>> {
    let Some(path) = path() else { return Ok(None) };
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Keep a login, in a file only its owner can read.
pub fn save(saved: &Saved) -> Result<PathBuf> {
    let path = path().context(config::NOWHERE)?;
    let dir = path.parent().context("the login's file has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(".auth.json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        // An older file's mode is kept by `open`: set it whatever it was.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all((serde_json::to_string_pretty(saved)? + "\n").as_bytes()).with_context(|| format!("writing {}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Remove the saved login; whether there was one.
pub fn forget() -> Result<bool> {
    let Some(path) = path() else { return Ok(false) };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// The login this run uses: the environment's token (at the environment's address, else the saved
/// one), else the saved login.
pub fn resolve(env: impl Fn(&str) -> Option<String>, file: Option<Saved>) -> Result<Option<Saved>> {
    let env = |name: &str| env(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let Some(token) = env(TOKEN_ENV) else { return Ok(file) };
    let api = match env(API_ENV).or_else(|| file.as_ref().map(|f| f.api.clone())) {
        Some(api) => clean_api(&api)?,
        None => bail!("{TOKEN_ENV} is set, but not where to use it: set {API_ENV} to the dashboard's address"),
    };
    Ok(Some(Saved { api, token, login: String::new(), team: String::new() }))
}

/// A dashboard's address as it's kept: `https://host`, without a path. Plain http only for this
/// machine itself (a dashboard in development): a token must not cross a network unencrypted.
pub fn clean_api(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let url = if url.contains("://") { url.to_owned() } else { format!("https://{url}") };
    let uri: hyper::Uri = url.parse().with_context(|| format!("{url} isn't an address"))?;
    let host = uri.host().with_context(|| format!("{url} has no host"))?;
    let local = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    match uri.scheme_str() {
        Some("https") => {}
        Some("http") if local => {}
        _ => bail!("{url}: the dashboard's address starts with https://"),
    }
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        bail!("{url}: the dashboard's address alone, without a path");
    }
    Ok(url)
}

#[derive(Deserialize)]
struct Begun {
    device_code: String,
    user_code: String,
    verify_url: String,
    #[serde(default)]
    interval: u64,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Deserialize)]
struct Granted {
    token: String,
    user: Who,
    #[serde(default)]
    team: String,
}

#[derive(Deserialize)]
struct Who {
    login: String,
    #[serde(default)]
    role: String,
}

/// What an API answered when it refused: its own words.
fn refusal(status: u16, body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| format!("it answered HTTP {status}"))
}

/// This machine's name, for the dashboard to show beside the login.
fn machine() -> String {
    let named = std::process::Command::new("hostname").output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    named.filter(|n| !n.is_empty()).or_else(|| std::env::var("HOSTNAME").ok()).unwrap_or_else(|| "a terminal".to_owned())
}

/// `forkrail login [URL]`.
pub async fn login(url: Option<String>, no_browser: bool) -> Result<()> {
    let saved = load()?;
    let api = match url.or_else(|| std::env::var(API_ENV).ok().filter(|v| !v.is_empty())).or_else(|| saved.as_ref().map(|s| s.api.clone())) {
        Some(url) => clean_api(&url)?,
        None => bail!("which install? Name its dashboard: forkrail login https://dashboard.example.com"),
    };
    let http = Http::new()?;
    let post = |path: &'static str, body: Value| {
        let (http, url) = (&http, format!("{api}/api/v1/{path}"));
        async move { http.exchange(Method::POST, &url, &[("content-type", "application/json")], Some(Bytes::from(body.to_string()))).await.with_context(|| format!("asking {url}")) }
    };
    let (status, body) = post("login", json!({ "name": machine() })).await?;
    if status != 200 {
        bail!("{api} didn't begin a login: {}", refusal(status, &body));
    }
    let begun: Begun = serde_json::from_slice(&body).with_context(|| format!("{api} isn't a forkrail dashboard, or is an older one"))?;
    eprintln!();
    eprintln!("  To sign this terminal in, open:");
    eprintln!();
    eprintln!("      {}", begun.verify_url);
    eprintln!();
    eprintln!("  and check that the page shows this code:  {}", begun.user_code);
    eprintln!();
    if !no_browser && crate::github::open_browser(&begun.verify_url) {
        eprintln!("  (opened in your browser)");
        eprintln!();
    }
    let every = Duration::from_secs(begun.interval.clamp(1, 30));
    let until = Instant::now() + Duration::from_secs(if begun.expires_in == 0 { 600 } else { begun.expires_in });
    let granted: Granted = loop {
        tokio::time::sleep(every).await;
        if Instant::now() > until {
            bail!("nobody answered in time: run `forkrail login` again");
        }
        // A lost answer is asked for again: the dashboard gives the same token until it's used.
        let Ok((status, body)) = post("login/token", json!({ "device_code": begun.device_code })).await else { continue };
        match status {
            200 => break serde_json::from_slice(&body).context("reading the dashboard's answer")?,
            202 | 500..=599 | 429 => continue,
            _ => bail!("{}", refusal(status, &body)),
        }
    };
    let saved = Saved { api: api.clone(), token: granted.token, login: granted.user.login, team: granted.team };
    let path = save(&saved)?;
    let role = if granted.user.role.is_empty() { String::new() } else { format!(" ({})", granted.user.role) };
    log(&format!("signed in to {api} as {}{role}, in {}", saved.login, if saved.team.is_empty() { "its team" } else { saved.team.as_str() }));
    log(&format!("kept in {}; `forkrail logout` signs this terminal out", path.display()));
    Ok(())
}

/// `forkrail logout`: end the token at the dashboard, then forget it here.
pub async fn logout() -> Result<()> {
    let Some(saved) = load()? else {
        log("this terminal isn't signed in");
        return Ok(());
    };
    let url = format!("{}/api/v1/logout", saved.api);
    let authorization = format!("Bearer {}", saved.token);
    let ended = Http::new()?.exchange(Method::POST, &url, &[("authorization", authorization.as_str())], None).await;
    forget()?;
    match ended {
        // 401: it had ended there already.
        Ok((200 | 401, _)) => log(&format!("signed out of {}", saved.api)),
        Ok((status, body)) => log(&format!("forgotten here; {} didn't end the login ({}): sign it out under Account in the dashboard", saved.api, refusal(status, &body))),
        Err(e) => log(&format!("forgotten here; {} couldn't be reached ({e:#}): sign it out under Account in the dashboard", saved.api)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(clean_api("https://app.example.com/").unwrap(), "https://app.example.com");
        assert_eq!(clean_api("app.example.com").unwrap(), "https://app.example.com");
        assert_eq!(clean_api("http://localhost:3043").unwrap(), "http://localhost:3043");
        assert_eq!(clean_api("http://127.0.0.1:3043/").unwrap(), "http://127.0.0.1:3043");
        for bad in ["http://app.example.com", "https://app.example.com/api", "ftp://x", "https://app.example.com?x=1", ""] {
            assert!(clean_api(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_environments_token_wins() {
        let file = Saved { api: "https://app.example.com".into(), token: "frk_file".into(), login: "ada".into(), team: "t".into() };
        let none = |_: &str| None;
        assert_eq!(resolve(none, Some(file.clone())).unwrap(), Some(file.clone()));
        assert_eq!(resolve(none, None).unwrap(), None);
        let token = |name: &str| (name == TOKEN_ENV).then(|| "frk_env".to_owned());
        let used = resolve(token, Some(file.clone())).unwrap().unwrap();
        assert_eq!((used.api.as_str(), used.token.as_str()), ("https://app.example.com", "frk_env"));
        assert!(resolve(token, None).is_err(), "a token with nowhere to use it");
        let both = |name: &str| match name {
            TOKEN_ENV => Some("frk_env".to_owned()),
            API_ENV => Some("other.example.com".to_owned()),
            _ => None,
        };
        assert_eq!(resolve(both, Some(file)).unwrap().unwrap().api, "https://other.example.com");
    }

    #[test]
    fn refusals_are_read() {
        assert_eq!(refusal(403, br#"{"error":"the login was refused in the dashboard","code":"denied"}"#), "the login was refused in the dashboard");
        assert_eq!(refusal(502, b"<html>bad gateway</html>"), "it answered HTTP 502");
    }
}
