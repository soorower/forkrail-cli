//! Deploy hooks, as Vercel's: a secret URL per project and branch that builds the branch's newest
//! commit of the project's connected repository when it's called, with GET or POST and nothing
//! else (a CMS that publishes, a cron service, a script). `?buildCache=false` builds without the
//! project's build cache.
//!
//! The URL is `<dashboard>/api/deploy-hooks/<project>/<secret>`: the dashboard answers it, finds
//! the hook by the secret's SHA-256, counts it against the project's 60 an hour (across its hooks)
//! and asks the control function to build (`trigger`), which checks the secret again. Only the
//! SHA-256 is kept, in the project file: the secret is shown once, when the hook is made, as a
//! bypass secret's is.
//!
//! A hook's deployment is recorded `via: hook` with the hook's id and name, built as a push to its
//! branch would be (production for the production branch, else a preview with the branch's host),
//! and GitHub hears how it ends as it would for a push. Calling a hook again for the same commit
//! cancels the build it started before.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::connect::{self, Api, Push, Pushed};
use crate::deploy::{Ctx, check_project, project_record};
use crate::state::{self, DeployHook, HookRef, Project, now_rfc3339};

/// Hooks a project may have (Vercel's limit for Hobby and Pro).
pub const MAX_HOOKS: usize = 5;
/// Calls a project's hooks may make in an hour, all together: the dashboard counts them.
pub const PER_HOUR: u32 = 60;
const MAX_NAME: usize = 100;
/// Where the dashboard answers hooks, under its address.
pub const PATH: &str = "/api/deploy-hooks";

/// What's asked of a project's deploy hooks: the CLI's `forkrail deploy-hooks`, the control
/// function's `deploy_hooks` action and the dashboard's Git settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DeployHooksOp {
    List { project: String },
    /// A new hook for `branch` (the production branch if not given).
    Create {
        project: String,
        name: String,
        #[serde(default)]
        branch: Option<String>,
    },
    Remove { project: String, id: String },
}

impl DeployHooksOp {
    pub fn changes(&self) -> bool {
        !matches!(self, DeployHooksOp::List { .. })
    }

    pub fn name(&self) -> &'static str {
        match self {
            DeployHooksOp::List { .. } => "list",
            DeployHooksOp::Create { .. } => "create",
            DeployHooksOp::Remove { .. } => "remove",
        }
    }

    pub fn project(&self) -> &str {
        match self {
            DeployHooksOp::List { project } | DeployHooksOp::Create { project, .. } | DeployHooksOp::Remove { project, .. } => project,
        }
    }
}

/// A hook's URL, from the dashboard's address.
pub fn url(dashboard: &str, project: &str, secret: &str) -> String {
    format!("{}{PATH}/{project}/{secret}", dashboard.trim().trim_end_matches('/'))
}

/// SHA-256 of a secret, in hex: what's kept.
pub fn secret_sha256(secret: &str) -> String {
    forkrail_common::hex(&Sha256::digest(secret.as_bytes()))
}

/// The project's hook whose secret this is.
pub fn find<'a>(project: &'a Project, secret: &str) -> Option<&'a DeployHook> {
    let hash = secret_sha256(secret);
    project.deploy_hooks.iter().find(|h| same(h.secret_sha256.as_bytes(), hash.as_bytes()))
}

/// Equal, in a time that doesn't depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A hook as it's shown: never its secret's hash.
fn view(h: &DeployHook) -> Value {
    json!({"id": h.id, "name": h.name, "branch": h.branch, "created": h.created, "created_by": h.created_by})
}

/// A name as typed, checked: one line, 1 to 100 characters.
fn check_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a deploy hook needs a name");
    }
    if name.chars().count() > MAX_NAME {
        bail!("a deploy hook's name is {MAX_NAME} characters at most");
    }
    if name.chars().any(char::is_control) {
        bail!("a deploy hook's name is one line of text");
    }
    Ok(name.to_owned())
}

/// Do `op`. `actor` is recorded as the creator of a new hook.
pub async fn run(ctx: &Ctx, op: DeployHooksOp, actor: Option<&str>) -> Result<Value> {
    check_project(op.project())?;
    match op {
        DeployHooksOp::List { project } => {
            let p = project_record(ctx, &project).await?;
            Ok(json!({
                "project": project,
                "connected": p.git.is_some(),
                "production_branch": p.git.as_ref().map(|g| g.production_branch.clone()),
                "hooks": p.deploy_hooks.iter().map(view).collect::<Vec<_>>(),
            }))
        }
        DeployHooksOp::Create { project, name, branch } => {
            let name = check_name(&name)?;
            if let Some(b) = &branch {
                connect::check_branch(b)?;
            }
            let secret = forkrail_common::random_hex(20);
            let hook_id = forkrail_common::random_hex(6);
            let hash = secret_sha256(&secret);
            let mut made = None;
            ctx.store
                .update::<Project, _>(&state::project_key(&project), |current| {
                    let mut p = current.with_context(|| format!("no project {project}"))?;
                    let Some(link) = &p.git else { bail!("{project} has no connected repository: a deploy hook builds a branch of it, so connect one first") };
                    if p.deploy_hooks.len() >= MAX_HOOKS {
                        bail!("{project} has {MAX_HOOKS} deploy hooks, the most a project may have: remove one first");
                    }
                    let hook = DeployHook {
                        id: hook_id.clone(),
                        name: name.clone(),
                        branch: branch.clone().unwrap_or_else(|| link.production_branch.clone()),
                        secret_sha256: hash.clone(),
                        created: now_rfc3339(),
                        created_by: actor.map(str::to_owned),
                    };
                    made = Some(hook.clone());
                    p.deploy_hooks.push(hook);
                    Ok(p)
                })
                .await?;
            let hook = made.context("the hook wasn't made")?;
            let settings = ctx.store.settings().await?;
            let url = settings.dashboard.as_deref().map(|d| url(d, &project, &secret));
            Ok(json!({"project": project, "hook": view(&hook), "secret": secret, "url": url}))
        }
        DeployHooksOp::Remove { project, id } => {
            ctx.store
                .update::<Project, _>(&state::project_key(&project), |current| {
                    let mut p = current.with_context(|| format!("no project {project}"))?;
                    let before = p.deploy_hooks.len();
                    p.deploy_hooks.retain(|h| h.id != id);
                    if p.deploy_hooks.len() == before {
                        bail!("{project} has no deploy hook {id}");
                    }
                    Ok(p)
                })
                .await?;
            Ok(json!({"project": project, "id": id, "removed": true}))
        }
    }
}

/// A hook was called: build its branch's newest commit, if `secret` is its secret.
pub async fn trigger(ctx: &Ctx, api: &Api, project: &str, id: &str, secret: &str, no_cache: bool) -> Result<Pushed> {
    check_project(project)?;
    let p = project_record(ctx, project).await?;
    let Some(hook) = find(&p, secret).filter(|h| h.id == id) else { bail!("{project} has no such deploy hook") };
    let Some(link) = &p.git else { bail!("{project} has no connected repository") };
    let push = Push {
        project: project.to_owned(),
        repo_id: link.repo_id,
        branch: hook.branch.clone(),
        sha: None,
        before: None,
        pusher: None,
        hook: Some(HookRef { id: hook.id.clone(), name: hook.name.clone() }),
        no_cache,
        webhook: false,
        pusher_role: None,
    };
    connect::pushed(ctx, api, push).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(hooks: &[(&str, &str)]) -> Project {
        Project {
            name: "shop".into(),
            deploy_hooks: hooks
                .iter()
                .map(|(id, secret)| DeployHook { id: (*id).into(), name: format!("hook {id}"), branch: "main".into(), secret_sha256: secret_sha256(secret), created: String::new(), created_by: None })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_hook_is_found_by_its_secret_only() {
        let p = project(&[("aaaaaaaaaaaa", "s3cr3t"), ("bbbbbbbbbbbb", "other")]);
        assert_eq!(find(&p, "s3cr3t").map(|h| h.id.as_str()), Some("aaaaaaaaaaaa"));
        assert_eq!(find(&p, "other").map(|h| h.id.as_str()), Some("bbbbbbbbbbbb"));
        assert!(find(&p, "s3cr3").is_none() && find(&p, "").is_none());
        assert!(find(&p, &secret_sha256("s3cr3t")).is_none(), "the hash isn't the secret");
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"abc", b"ab"));
    }

    #[test]
    fn names_urls_and_ops() {
        assert_eq!(check_name("  Sanity publish ").unwrap(), "Sanity publish");
        assert!(check_name(" ").is_err() && check_name("a\nb").is_err() && check_name(&"x".repeat(101)).is_err());
        assert!(check_name(&"é".repeat(100)).is_ok());
        assert_eq!(url("https://app.example.com/", "shop", "abc"), "https://app.example.com/api/deploy-hooks/shop/abc");
        let op: DeployHooksOp = serde_json::from_value(json!({"op": "create", "project": "shop", "name": "CMS"})).unwrap();
        assert!(matches!(&op, DeployHooksOp::Create { branch: None, .. }) && op.changes() && op.project() == "shop");
        let op: DeployHooksOp = serde_json::from_value(json!({"op": "list", "project": "shop"})).unwrap();
        assert!(!op.changes() && op.name() == "list");
        assert!(serde_json::from_value::<DeployHooksOp>(json!({"op": "trigger", "project": "shop"})).is_err());
        // The view never carries the hash.
        let p = project(&[("aaaaaaaaaaaa", "s3cr3t")]);
        let shown = view(&p.deploy_hooks[0]).to_string();
        assert!(!shown.contains(&secret_sha256("s3cr3t")) && shown.contains("hook aaaaaaaaaaaa"), "{shown}");
    }
}
