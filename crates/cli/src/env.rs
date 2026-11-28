//! A project's env vars: their names in its project file, their values in SSM (secrets.rs). A
//! change takes effect on the project's next deploy or redeploy, as on Vercel. Production's and
//! previews' values go in and are never read back out, except by publishing; Development's, which
//! no deployment uses, come back out through `pull` (`forkrail env pull`), for a local `.env.local`.
//!
//! A preview value can also be for one Git branch (`--git-branch`), as on Vercel: it wins over
//! preview's value of the same name for that branch's previews, which still get every other
//! preview value. Asking for a branch's values names no target: a branch's are always preview's,
//! and a forkrail from before branches, which needs a target, refuses such a request rather than
//! change preview's own.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use forkrail_common::install::{MAX_BRANCH_DIR, env_branch_dir};
use serde::{Deserialize, Serialize};

use crate::connect::check_branch;
use crate::deploy::{Ctx, check_env_key, check_project, check_user_env_key, function_env, new_project};
use crate::secrets::{self, Scope, Secrets, overlay};
use crate::state::{self, EnvTarget, Project, Target};
use crate::vercel::Language;

/// Where a change goes: each of `targets`, or with a Git branch, that branch's own preview values
/// (`targets` then names preview, or nothing).
pub fn scopes<'a>(targets: &[EnvTarget], branch: Option<&'a str>) -> Result<Vec<Scope<'a>>> {
    match branch {
        None if targets.is_empty() => bail!("no environment to set them for"),
        None => Ok(targets.iter().map(|&t| Scope::Target(t)).collect()),
        Some(branch) => {
            if targets.iter().any(|&t| t != EnvTarget::Preview) {
                bail!("only preview variables can be for a Git branch");
            }
            check_env_branch(branch)?;
            Ok(vec![Scope::Branch(branch)])
        }
    }
}

/// A branch's name, as git allows them, short enough for SSM's names.
pub fn check_env_branch(branch: &str) -> Result<()> {
    check_branch(branch)?;
    if env_branch_dir(branch).len() > MAX_BRANCH_DIR {
        bail!("{branch:?} is too long a branch name for its own env vars");
    }
    Ok(())
}

/// Set `values` (name → value) for each of `targets`, or for Git branch `branch`'s previews. With
/// `create`, a project that doesn't exist yet is created, so it can be set up before its first
/// deploy.
pub async fn set(ctx: &Ctx, project: &str, values: &BTreeMap<String, String>, targets: &[EnvTarget], branch: Option<&str>, create: bool) -> Result<()> {
    for key in values.keys() {
        check_user_env_key(key)?;
    }
    put(ctx, project, values, &scopes(targets, branch)?, create).await
}

/// Set env vars forkrail itself manages (`deploy::SYSTEM_ENV`), which `set` refuses.
pub async fn set_system(ctx: &Ctx, project: &str, values: &BTreeMap<String, String>, targets: &[EnvTarget]) -> Result<()> {
    if let Some(key) = values.keys().find(|k| !crate::deploy::SYSTEM_ENV.contains(&k.as_str())) {
        bail!("{key} isn't one forkrail sets");
    }
    put(ctx, project, values, &scopes(targets, None)?, false).await
}

async fn put(ctx: &Ctx, project: &str, values: &BTreeMap<String, String>, scopes: &[Scope<'_>], create: bool) -> Result<()> {
    check_project(project)?;
    if values.is_empty() {
        bail!("no env vars to set");
    }
    for (key, value) in values {
        check_env_key(key)?;
        if value.is_empty() {
            bail!("empty value for {key}");
        }
    }
    let current = ctx.store.get_value::<Project>(&state::project_key(project)).await?;
    match &current {
        Some(p) => secrets::check_moved(p)?,
        None if create => crate::project::check_new(ctx, project).await?,
        None => bail!("no project {project}"),
    }
    // Pushes to the production branch make production deployments, which never get a branch's.
    let production_branch = current.as_ref().and_then(|p| p.git.as_ref()).map(|g| g.production_branch.as_str());
    if let Some(branch) = scopes.iter().find_map(|s| match s {
        Scope::Branch(b) => Some(*b).filter(|b| Some(*b) == production_branch),
        Scope::Target(_) => None,
    }) {
        bail!("{branch} is {project}'s production branch: its pushes are production deployments, which get production's values");
    }
    let store = Secrets::new(ctx);
    // A deployment's env vars may take 64 KB in all (past Lambda's 4 KB it carries them in a sealed
    // file): refuse here rather than at the next deploy, before any value is written. A branch's
    // previews get preview's values with the branch's over them, so a change to either is checked
    // for each branch with values of its own.
    let branches: Vec<&str> = current.as_ref().map(|p| p.env_names.branches.keys().map(String::as_str).collect()).unwrap_or_default();
    let fitting = |scope: Scope<'_>, all: &BTreeMap<String, String>| fits(all).map_err(|e| anyhow::anyhow!("{project} ({}): {e}", scope.describe()));
    for &scope in scopes {
        match scope {
            Scope::Branch(_) => {
                let (preview, own) = tokio::try_join!(store.values(project, Target::Preview), store.values(project, scope))?;
                fitting(scope, &overlay(preview, overlay(own, values.clone())))?;
            }
            Scope::Target(EnvTarget::Preview) => {
                let preview = overlay(store.values(project, scope).await?, values.clone());
                fitting(scope, &preview)?;
                for &branch in &branches {
                    let own = store.values(project, Scope::Branch(branch)).await?;
                    fitting(Scope::Branch(branch), &overlay(preview.clone(), own))?;
                }
            }
            Scope::Target(_) => fitting(scope, &overlay(store.values(project, scope).await?, values.clone()))?,
        }
    }
    // The values first: a name is listed only once its value is there.
    for &scope in scopes {
        store.put_all(project, scope, values).await?;
    }
    ctx.store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = match current {
                Some(p) => p,
                None if create => new_project(project),
                None => bail!("no project {project}"),
            };
            for &scope in scopes {
                match scope {
                    Scope::Target(target) => p.env_names.get_mut(target).extend(values.keys().cloned()),
                    Scope::Branch(branch) => p.env_names.add_to_branch(branch, values.keys().cloned()),
                }
            }
            Ok(p)
        })
        .await?;
    Ok(())
}

/// Remove `key` from each of `targets`, or from Git branch `branch`'s own preview values; false if
/// it wasn't set for any of them. Without a branch, branches' own values stay.
pub async fn remove(ctx: &Ctx, project: &str, key: &str, targets: &[EnvTarget], branch: Option<&str>) -> Result<bool> {
    check_project(project)?;
    let scopes = scopes(targets, branch)?;
    let mut removed = false;
    ctx.store
        .update::<Project, _>(&state::project_key(project), |current| {
            let Some(mut p) = current else { bail!("no project {project}") };
            secrets::check_moved(&p)?;
            removed = false;
            for &scope in &scopes {
                removed |= match scope {
                    Scope::Target(target) => p.env_names.get_mut(target).remove(key),
                    Scope::Branch(branch) => p.env_names.remove_from_branch(branch, key),
                };
            }
            Ok(p)
        })
        .await?;
    let store = Secrets::new(ctx);
    for &scope in &scopes {
        removed |= store.delete(project, scope, key).await?;
    }
    Ok(removed)
}

/// What `forkrail env pull` gets for one target: a readable target's values (Development's), and
/// only the names of the rest, whose values never come back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pulled {
    pub project: String,
    pub target: EnvTarget,
    /// For preview: the Git branch whose previews' variables these are, its own among them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Name → value, for the names the project lists that have one.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    /// Names set for the target whose values can't be read back.
    #[serde(default)]
    pub secret: Vec<String>,
}

impl Pulled {
    /// Whose variables these are, in words: `development`, `preview (feature/logs)`.
    pub fn label(&self) -> String {
        match &self.branch {
            Some(branch) => Scope::Branch(branch).describe(),
            None => self.target.as_str().to_owned(),
        }
    }
}

/// Which variables a pull asks for: `target`'s, or with a Git branch, that branch's previews'
/// (`target` then names preview, or nothing).
pub fn pull_target(target: Option<EnvTarget>, branch: Option<&str>) -> Result<EnvTarget> {
    match (target, branch) {
        (Some(target), None) => Ok(target),
        (None, None) => bail!("no environment to pull"),
        (Some(EnvTarget::Preview) | None, Some(branch)) => check_env_branch(branch).map(|_| EnvTarget::Preview),
        (Some(_), Some(_)) => bail!("only preview variables can be for a Git branch"),
    }
}

/// A project's variables for `target`, as `Pulled` says; for preview with a branch, what that
/// branch's previews get: preview's names and the branch's own.
pub async fn pull(ctx: &Ctx, project: &str, target: EnvTarget, branch: Option<&str>) -> Result<Pulled> {
    check_project(project)?;
    let target = pull_target(Some(target), branch)?;
    let p = ctx.store.get_value::<Project>(&state::project_key(project)).await?.with_context(|| format!("no project {project}"))?;
    let names = match target.deployed() {
        Some(deployed) => p.env_names.deployed(deployed, branch),
        None => p.env_names.get(target).clone(),
    };
    let branch = branch.map(str::to_owned);
    if !target.readable() {
        return Ok(Pulled { project: project.to_owned(), target, branch, values: BTreeMap::new(), secret: names.into_iter().collect() });
    }
    let mut values = Secrets::new(ctx).values(project, target).await?;
    // Only what the project lists: a value is written before its name, and its name removed first.
    values.retain(|k, _| names.contains(k));
    Ok(Pulled { project: project.to_owned(), target, branch, values, secret: Vec::new() })
}

/// Whether `values` could be a new project's env vars: what `set` would refuse of them, known
/// before anything is made.
pub fn check_new(values: &BTreeMap<String, String>) -> Result<()> {
    for (key, value) in values {
        check_user_env_key(key)?;
        if value.is_empty() {
            bail!("empty value for {key}");
        }
    }
    fits(values)
}

/// Whether these env vars fit in a deployment: 64 KB in all, names and values together, as on
/// Vercel (deploy.rs, `function_env`).
fn fits(values: &BTreeMap<String, String>) -> Result<()> {
    function_env("api/main.py", Language::Python, values, &BTreeMap::new(), &[]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_is_previews_alone() {
        use EnvTarget::*;
        assert_eq!(scopes(&[Production, Preview], None).unwrap(), [Scope::Target(Production), Scope::Target(Preview)]);
        assert!(scopes(&[], None).is_err());
        // With a branch: preview's, named or not, and nothing else.
        assert_eq!(scopes(&[], Some("feature/logs")).unwrap(), [Scope::Branch("feature/logs")]);
        assert_eq!(scopes(&[Preview], Some("feature/logs")).unwrap(), [Scope::Branch("feature/logs")]);
        for targets in [&[Production][..], &[Preview, Production], &[Development]] {
            assert!(scopes(targets, Some("feature/logs")).unwrap_err().to_string().contains("only preview"));
        }
        for bad in ["", "a b", "a..b", "/a", "a:b", &"x".repeat(256)] {
            assert!(scopes(&[], Some(bad)).is_err(), "{bad:?}");
        }
        // A name git allows that's too long once written for SSM.
        assert!(check_env_branch(&"é".repeat(70)).unwrap_err().to_string().contains("too long"));
        assert!(check_env_branch(&"x".repeat(255)).is_ok());
        assert_eq!(pull_target(Some(Development), None).unwrap(), Development);
        assert_eq!((pull_target(None, Some("b")).unwrap(), pull_target(Some(Preview), Some("b")).unwrap()), (Preview, Preview));
        assert!(pull_target(Some(Production), Some("b")).is_err() && pull_target(None, None).is_err());
        let pulled = Pulled { project: "p".into(), target: Preview, branch: Some("b".into()), values: BTreeMap::new(), secret: vec![] };
        assert_eq!(pulled.label(), "preview (b)");
        // Read by an older CLI, it's preview's; one without a branch writes none.
        assert_eq!(serde_json::to_value(&pulled).unwrap()["branch"], "b");
        assert!(serde_json::to_value(Pulled { branch: None, ..pulled }).unwrap().get("branch").is_none());
    }

    #[test]
    fn values_must_fit_beside_the_runtimes_own() {
        let small: BTreeMap<String, String> = [("KEY".to_owned(), "x".repeat(100))].into();
        assert!(fits(&small).is_ok());
        assert!(check_new(&small).is_ok() && check_new(&BTreeMap::new()).is_ok());
        assert!(check_new(&[("KEY".to_owned(), String::new())].into()).is_err());
        assert!(check_new(&[("AWS_LAMBDA_EXEC_WRAPPER".to_owned(), "x".to_owned())].into()).is_err());
        assert!(check_new(&[("AWS_ACCESS_KEY_ID".to_owned(), "x".to_owned())].into()).is_ok());
        // Past Lambda's 4 KB is fine now (sealed at publish); past 64 KB isn't.
        let big: BTreeMap<String, String> = [("A".to_owned(), "x".repeat(2000)), ("B".to_owned(), "y".repeat(2000))].into();
        assert!(check_new(&big).is_ok());
        let over: BTreeMap<String, String> = [("A".to_owned(), "x".repeat(40_000)), ("B".to_owned(), "y".repeat(30_000))].into();
        assert!(fits(&over).unwrap_err().to_string().contains("65536"));
    }
}
