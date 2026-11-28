//! Env var values: one SSM SecureString parameter per variable,
//! `/forkrail/env/<project>/<target>/<KEY>`, encrypted with the install's own KMS key
//! (`alias/forkrail-env`, from deploy/install/secrets.yaml). Project files keep only the names.
//! A value longer than a parameter holds (4 KB) is kept in pieces: `<KEY>`, then `<KEY>.1`,
//! `<KEY>.2` … (no env var's name has a dot), joined again when read.
//!
//! Previews' values for one Git branch, which win over preview's own for that branch's previews,
//! are kept the same way under `/forkrail/env/<project>/branches/<branch>/` (`Scope::Branch`).
//!
//! The key, not SSM, is what keeps values private: AWS's managed SSM policy (the routers' role has
//! it) can read any parameter, but only a principal IAM gives `kms:Decrypt` on this key gets a
//! value out, and the key refuses the routers' role outright. The functions' environments are
//! encrypted with the same key in each region (`Ctx::env_key`), so reading a function's
//! configuration shows no values without it either.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use aws_sdk_ssm::error::DisplayErrorContext;
use aws_sdk_ssm::types::{ParameterTier, ParameterType, Tag};
use forkrail_common::install::{ENV_KEY_ALIAS, ENV_PARAM_PREFIX, PROJECT_TAG, TAG_KEY, TAG_VALUE, env_branch_dir, env_param};

use crate::deploy::Ctx;
use crate::state::{EnvTarget, Project, Target};

/// Which of a project's values: a target's, or one Git branch's previews' (`forkrail env add
/// --git-branch`), which win over preview's own of the same name for that branch's previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    Target(EnvTarget),
    Branch(&'a str),
}

impl Scope<'_> {
    /// Its folder under the project's: `production`, `branches/feature_2Flogs`.
    fn dir(self) -> String {
        match self {
            Scope::Target(target) => target.as_str().to_owned(),
            Scope::Branch(branch) => env_branch_dir(branch),
        }
    }

    /// In words: `production`, `preview (feature/logs)`.
    pub fn describe(self) -> String {
        match self {
            Scope::Target(target) => target.as_str().to_owned(),
            Scope::Branch(branch) => format!("preview ({branch})"),
        }
    }
}

impl From<EnvTarget> for Scope<'_> {
    fn from(target: EnvTarget) -> Self {
        Scope::Target(target)
    }
}

impl From<Target> for Scope<'_> {
    fn from(target: Target) -> Self {
        Scope::Target(target.into())
    }
}

/// A project whose values are still in its file, from before they moved, can't be deployed or
/// promoted until `forkrail env migrate` moves them: SSM wouldn't have them.
pub fn check_moved(project: &Project) -> Result<()> {
    if project.legacy_env.has_values() {
        bail!("{}'s env var values are still in its project file: run `forkrail env migrate` first", project.name);
    }
    Ok(())
}

/// SSM's limit on a standard parameter's value: a longer one is kept in pieces of this size.
const PIECE_BYTES: usize = 4096;

/// `value` in pieces of at most `PIECE_BYTES`, cut between characters.
fn pieces(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = value;
    while rest.len() > PIECE_BYTES {
        let mut cut = PIECE_BYTES;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out.push(rest);
    out
}

/// The name of `key`'s piece `index` under its target: the key itself for the first.
fn piece_name(key: &str, index: usize) -> String {
    if index == 0 { key.to_owned() } else { format!("{key}.{index}") }
}

/// Which variable's piece a parameter's last name part is, and which piece: `("KEY", 0)` for
/// `KEY`, `("KEY", 2)` for `KEY.2`.
fn piece_of(name: &str) -> Option<(&str, usize)> {
    match name.rsplit_once('.') {
        None => Some((name, 0)),
        Some((key, index)) => index.parse::<usize>().ok().filter(|&i| i > 0).map(|i| (key, i)),
    }
}

/// Values from their parameters (last name part → value), pieces joined. Pieces left without
/// their first (a removal cut short) are ignored; a value missing a piece is an error.
fn join_pieces(params: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut by_key: BTreeMap<String, BTreeMap<usize, String>> = BTreeMap::new();
    for (name, value) in params {
        if let Some((key, index)) = piece_of(&name) {
            by_key.entry(key.to_owned()).or_default().insert(index, value);
        }
    }
    let mut values = BTreeMap::new();
    for (key, parts) in by_key {
        if !parts.contains_key(&0) {
            continue;
        }
        if let Some((i, _)) = parts.keys().enumerate().find(|(i, index)| i != *index) {
            bail!("{key}'s value is missing piece {i} in SSM: set it again");
        }
        values.insert(key, parts.into_values().collect());
    }
    Ok(values)
}

/// Preview's values with a branch's own over them: a branch needs only the values it changes.
pub fn overlay(mut general: BTreeMap<String, String>, own: BTreeMap<String, String>) -> BTreeMap<String, String> {
    general.extend(own);
    general
}

pub struct Secrets {
    ssm: aws_sdk_ssm::Client,
}

impl Secrets {
    /// In the home region, where the install's state is.
    pub fn new(ctx: &Ctx) -> Self {
        Self { ssm: aws_sdk_ssm::Client::new(&ctx.config) }
    }

    /// Set `values` (name → value) for `target`, each in as many pieces as it needs; pieces a
    /// longer value had before are removed.
    pub async fn put_all<'a>(&self, project: &str, scope: impl Into<Scope<'a>>, values: &BTreeMap<String, String>) -> Result<()> {
        let dir = scope.into().dir();
        let before = self.names(project, &dir).await?;
        for (key, value) in values {
            let parts = pieces(value);
            for (index, part) in parts.iter().enumerate() {
                self.put_one(project, &env_param(project, &dir, &piece_name(key, index)), part).await?;
            }
            let stale = before.iter().filter_map(|n| piece_of(n)).filter(|(k, i)| k == key && *i >= parts.len());
            for (_, index) in stale.collect::<Vec<_>>() {
                self.delete_param(&env_param(project, &dir, &piece_name(key, index))).await?;
            }
        }
        Ok(())
    }

    /// The last name parts of a scope's parameters (its `dir`), without their values.
    async fn names(&self, project: &str, dir: &str) -> Result<Vec<String>> {
        let path = format!("{ENV_PARAM_PREFIX}{project}/{dir}");
        let mut names = Vec::new();
        let mut pages = self.ssm.get_parameters_by_path().path(&path).with_decryption(false).into_paginator().send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("listing {path}/*: {}", DisplayErrorContext(&e)))?;
            names.extend(page.parameters().iter().filter_map(|p| p.name()).filter_map(|n| n.rsplit('/').next()).map(str::to_owned));
        }
        Ok(names)
    }

    async fn delete_param(&self, name: &str) -> Result<bool> {
        match self.ssm.delete_parameter().name(name).send().await {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_parameter_not_found()) => Ok(false),
            Err(e) => bail!("removing {name}: {}", DisplayErrorContext(&e)),
        }
    }

    /// Set one parameter, creating it (tagged) or replacing it.
    async fn put_one(&self, project: &str, name: &str, value: &str) -> Result<()> {
        let name = name.to_owned();
        let put = || {
            self.ssm
                .put_parameter()
                .name(&name)
                .value(value)
                .r#type(ParameterType::SecureString)
                .key_id(ENV_KEY_ALIAS)
                .tier(ParameterTier::Standard)
        };
        let tags = [(TAG_KEY, TAG_VALUE), (PROJECT_TAG, project)].map(|(k, v)| Tag::builder().key(k).value(v).build());
        let tags = tags.into_iter().collect::<Result<Vec<_>, _>>()?;
        // SSM takes tags only when creating, and overwrites only when told to.
        match put().set_tags(Some(tags)).send().await {
            Ok(_) => Ok(()),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_parameter_already_exists()) => {
                put().overwrite(true).send().await.map_err(|e| anyhow::anyhow!("setting {name}: {}", DisplayErrorContext(&e)))?;
                Ok(())
            }
            Err(e) => bail!("setting {name}: {}", DisplayErrorContext(&e)),
        }
    }

    /// Set one of the install's own parameters (not a project's env var): a SecureString under
    /// the install's key when `secret`, else plain text.
    pub async fn put_install(&self, name: &str, value: &str, secret: bool, description: &str) -> Result<()> {
        if value.len() > PIECE_BYTES {
            bail!("{name}'s value is {} bytes; a parameter holds at most {PIECE_BYTES}", value.len());
        }
        let put = || {
            let put = self.ssm.put_parameter().name(name).value(value).description(description).tier(ParameterTier::Standard);
            if secret { put.r#type(ParameterType::SecureString).key_id(ENV_KEY_ALIAS) } else { put.r#type(ParameterType::String) }
        };
        let tag = Tag::builder().key(TAG_KEY).value(TAG_VALUE).build()?;
        match put().tags(tag).send().await {
            Ok(_) => Ok(()),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_parameter_already_exists()) => {
                put().overwrite(true).send().await.map_err(|e| anyhow::anyhow!("setting {name}: {}", DisplayErrorContext(&e)))?;
                Ok(())
            }
            Err(e) => bail!("setting {name}: {}", DisplayErrorContext(&e)),
        }
    }

    /// One of the install's plain parameters, or `None` if it isn't there. Never decrypts.
    pub async fn get_install(&self, name: &str) -> Result<Option<String>> {
        match self.ssm.get_parameter().name(name).send().await {
            Ok(out) => Ok(out.parameter().and_then(|p| p.value()).map(str::to_owned)),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_parameter_not_found()) => Ok(None),
            Err(e) => bail!("reading {name}: {}", DisplayErrorContext(&e)),
        }
    }

    /// One of the install's secrets, decrypted, or `None` if it isn't there. Only a role the
    /// install's key lets decrypt gets one.
    pub async fn get_secret(&self, name: &str) -> Result<Option<String>> {
        match self.ssm.get_parameter().name(name).with_decryption(true).send().await {
            Ok(out) => Ok(out.parameter().and_then(|p| p.value()).map(str::to_owned)),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_parameter_not_found()) => Ok(None),
            Err(e) => bail!("reading {name}: {}", DisplayErrorContext(&e)),
        }
    }

    /// Whether a parameter exists, without reading its value.
    pub async fn exists(&self, name: &str) -> Result<bool> {
        Ok(self.get_install(name).await?.is_some())
    }

    /// Remove one value, every piece of it; true if it was there.
    pub async fn delete<'a>(&self, project: &str, scope: impl Into<Scope<'a>>, key: &str) -> Result<bool> {
        let dir = scope.into().dir();
        // The first piece first: what's left of a removal cut short is never read.
        let removed = self.delete_param(&env_param(project, &dir, key)).await?;
        for name in self.names(project, &dir).await? {
            if piece_of(&name).is_some_and(|(k, i)| k == key && i > 0) {
                self.delete_param(&env_param(project, &dir, &name)).await?;
            }
        }
        Ok(removed)
    }

    /// Remove every value the project has, for every target and branch; returns how many went.
    /// Names are listed without their values.
    pub async fn delete_project(&self, project: &str) -> Result<usize> {
        let path = format!("{ENV_PARAM_PREFIX}{project}/");
        let mut names = Vec::new();
        let mut pages = self.ssm.get_parameters_by_path().path(&path).recursive(true).with_decryption(false).into_paginator().send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("listing {path}*: {}", DisplayErrorContext(&e)))?;
            names.extend(page.parameters().iter().filter_map(|p| p.name().map(str::to_owned)));
        }
        for name in names.iter().filter(|n| n.starts_with(&path)) {
            match self.ssm.delete_parameter().name(name).send().await {
                Ok(_) => {}
                Err(e) if e.as_service_error().is_some_and(|e| e.is_parameter_not_found()) => {}
                Err(e) => bail!("removing {name}: {}", DisplayErrorContext(&e)),
            }
        }
        Ok(names.len())
    }

    /// What a deployment of `target` gets: the target's values, and for a preview of Git branch
    /// `branch`, that branch's own over them, as on Vercel (both read at once).
    pub async fn deployed(&self, project: &str, target: Target, branch: Option<&str>) -> Result<BTreeMap<String, String>> {
        match branch.filter(|_| target == Target::Preview) {
            None => self.values(project, target).await,
            Some(branch) => {
                let (general, own) = tokio::try_join!(self.values(project, target), self.values(project, Scope::Branch(branch)))?;
                Ok(overlay(general, own))
            }
        }
    }

    /// Every value the project has for `scope`, decrypted, pieces joined.
    pub async fn values<'a>(&self, project: &str, scope: impl Into<Scope<'a>>) -> Result<BTreeMap<String, String>> {
        let path = format!("{ENV_PARAM_PREFIX}{project}/{}", scope.into().dir());
        let mut values = BTreeMap::new();
        let mut pages = self.ssm.get_parameters_by_path().path(&path).with_decryption(true).into_paginator().send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("reading {path}/*: {}", DisplayErrorContext(&e)))?;
            for parameter in page.parameters() {
                let name = parameter.name().context("a parameter without a name")?;
                let key = name.rsplit('/').next().unwrap_or_default().to_owned();
                values.insert(key, parameter.value().unwrap_or_default().to_owned());
            }
        }
        join_pieces(values)
    }
}

#[cfg(test)]
mod tests {
    use forkrail_common::install::env_param;

    #[test]
    fn long_values_are_kept_in_pieces() {
        use super::*;
        assert_eq!(pieces("short"), ["short"]);
        let long = format!("{}é{}", "a".repeat(4095), "b".repeat(9000));
        let parts = pieces(&long);
        assert_eq!(parts.len(), 4);
        assert!(parts.iter().all(|p| p.len() <= PIECE_BYTES) && parts.concat() == long);
        assert_eq!(parts[0].len(), 4095, "cut before the two-byte character");
        let names: Vec<String> = (0..parts.len()).map(|i| piece_name("PEM", i)).collect();
        assert_eq!(names, ["PEM", "PEM.1", "PEM.2", "PEM.3"]);
        assert_eq!((piece_of("PEM.2"), piece_of("PEM"), piece_of("PEM.0"), piece_of("PEM.x")), (Some(("PEM", 2)), Some(("PEM", 0)), None, None));
        // Read back in order, whatever order SSM lists them in; other variables untouched.
        let mut params: BTreeMap<String, String> = names.iter().cloned().zip(parts.iter().map(|p| p.to_string())).collect();
        params.insert("API_KEY".into(), "k".into());
        let joined = join_pieces(params.clone()).unwrap();
        assert_eq!((joined["PEM"].as_str(), joined["API_KEY"].as_str()), (long.as_str(), "k"));
        // A missing middle piece is an error; pieces left without their first are ignored.
        let mut gap = params.clone();
        gap.remove("PEM.2");
        assert!(join_pieces(gap).unwrap_err().to_string().contains("missing piece 2"));
        let mut orphans = params;
        orphans.remove("PEM");
        assert_eq!(join_pieces(orphans).unwrap().keys().collect::<Vec<_>>(), ["API_KEY"]);
    }

    #[test]
    fn a_branch_overrides_only_what_it_sets() {
        use super::*;
        let map = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        let preview = map(&[("API_URL", "https://staging"), ("KEY", "k")]);
        let merged = overlay(preview.clone(), map(&[("API_URL", "https://feature"), ("ONLY_HERE", "1")]));
        assert_eq!(merged, map(&[("API_URL", "https://feature"), ("KEY", "k"), ("ONLY_HERE", "1")]));
        assert_eq!(overlay(preview.clone(), BTreeMap::new()), preview);
        assert_eq!(Scope::Branch("feature/logs").dir(), "branches/feature_2Flogs");
        assert_eq!((Scope::from(Target::Preview).dir(), Scope::Branch("x").describe()), ("preview".to_owned(), "preview (x)".to_owned()));
    }

    #[test]
    fn a_value_is_the_last_part_of_its_name() {
        let name = env_param("shop", "production", "API_KEY");
        assert_eq!(name.rsplit('/').next(), Some("API_KEY"));
        assert!(name.starts_with(super::ENV_PARAM_PREFIX));
    }
}
