//! The CLI's own settings: the AWS profile and home region that reach the install, so they needn't
//! be named on every command. Kept in `~/.config/forkrail/config.json` (or `$XDG_CONFIG_HOME`; on
//! Windows `%APPDATA%\forkrail\config.json`), and written by `forkrail link`.
//!
//! Each setting comes from its flag, then the environment, then this file. Without a profile from
//! any of them the AWS SDK's own chain is used (credentials in the environment, the default profile,
//! a container or instance role), which is what a build machine or a Lambda function has.

use std::path::PathBuf;

use anyhow::{Context, Result};
use forkrail_common::install::DEFAULT_HOME_REGION;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The state bucket, when it isn't `forkrail-artifacts-<account>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
}

/// How this run reaches the install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub profile: Option<String>,
    pub region: String,
    pub bucket: Option<String>,
}

pub fn path() -> Option<PathBuf> {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    // Windows keeps a program's settings in %APPDATA%, and has no HOME unless a shell like Git Bash
    // sets one: before HOME, so every terminal finds the same file.
    let base = var("XDG_CONFIG_HOME").or_else(|| if cfg!(windows) { var("APPDATA") } else { None }).or_else(|| var("HOME").map(|home| home.join(".config")))?;
    Some(base.join("forkrail").join("config.json"))
}

/// Why there's no settings file: none of the variables [`path`] reads is set.
pub const NOWHERE: &str = if cfg!(windows) { "neither APPDATA nor HOME is set, so there's nowhere to keep forkrail's settings" } else { "neither HOME nor XDG_CONFIG_HOME is set, so there's nowhere to keep forkrail's settings" };

/// Set by `forkrail install` for the commands it runs itself: they reach the install it was given,
/// never the one `forkrail link` saved (which may be another account's).
pub const IGNORE_SAVED_ENV: &str = "FORKRAIL_IGNORE_SAVED";

/// The saved settings, or none if nothing was saved.
pub fn load() -> Result<Config> {
    if std::env::var_os(IGNORE_SAVED_ENV).is_some_and(|v| !v.is_empty()) {
        return Ok(Config::default());
    }
    let Some(path) = path() else { return Ok(Config::default()) };
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save(config: &Config) -> Result<PathBuf> {
    let path = path().context(NOWHERE)?;
    let dir = path.parent().context("the settings file has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(".config.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(config)? + "\n").with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Each setting from its flag, then the environment, then the saved file.
pub fn resolve(profile: Option<String>, region: Option<String>, env: impl Fn(&str) -> Option<String>, file: &Config) -> Resolved {
    let env = |name: &str| env(name).filter(|v| !v.is_empty());
    Resolved {
        profile: profile.or_else(|| env("AWS_PROFILE")).or_else(|| file.profile.clone()),
        region: region
            .or_else(|| env("FORKRAIL_HOME_REGION"))
            .or_else(|| file.region.clone())
            .unwrap_or_else(|| DEFAULT_HOME_REGION.to_owned()),
        bucket: env("FORKRAIL_BUCKET").or_else(|| file.bucket.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_then_environment_then_file() {
        let file = Config { profile: Some("saved".into()), region: Some("eu-central-1".into()), bucket: Some("mine".into()) };
        let none = |_: &str| None;
        let env = |name: &str| match name {
            "AWS_PROFILE" => Some("from-env".to_owned()),
            "FORKRAIL_HOME_REGION" => Some("us-west-2".to_owned()),
            "FORKRAIL_BUCKET" => Some("env-bucket".to_owned()),
            _ => None,
        };

        let flags = resolve(Some("flag".into()), Some("us-west-1".into()), env, &file);
        assert_eq!((flags.profile.as_deref(), flags.region.as_str()), (Some("flag"), "us-west-1"));
        assert_eq!(flags.bucket.as_deref(), Some("env-bucket"));

        let from_env = resolve(None, None, env, &file);
        assert_eq!((from_env.profile.as_deref(), from_env.region.as_str()), (Some("from-env"), "us-west-2"));

        let saved = resolve(None, None, none, &file);
        assert_eq!(saved, Resolved { profile: Some("saved".into()), region: "eu-central-1".into(), bucket: Some("mine".into()) });

        // Nothing anywhere: the SDK's default credentials, in us-east-1, with the default bucket.
        let bare = resolve(None, None, none, &Config::default());
        assert_eq!(bare, Resolved { profile: None, region: "us-east-1".into(), bucket: None });

        // An empty variable counts as unset.
        let empty = resolve(None, None, |_| Some(String::new()), &file);
        assert_eq!(empty.profile.as_deref(), Some("saved"));
    }

    #[test]
    fn saved_file_round_trips() {
        let config = Config { profile: Some("forkrail".into()), region: Some("us-east-1".into()), bucket: None };
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(json, r#"{"profile":"forkrail","region":"us-east-1"}"#);
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
        assert_eq!(serde_json::from_str::<Config>("{}").unwrap(), Config::default());
    }
}
