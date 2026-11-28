//! New projects from the dashboard: an empty one, to be deployed from a terminal, or one started
//! from a template, whose files (kept in this binary, from `templates/` in the repository) are
//! uploaded and built in the builder as production at once, like any remote deploy.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::deploy::{Ctx, check_project, new_project};
use crate::package::TempDir;
use crate::remote::{self, PublishOptions, Start, Started};
use crate::state::{self, Target, Via};
use crate::lambda;

/// A template: what the New project page offers.
pub struct Template {
    pub id: &'static str,
    pub title: &'static str,
    files: &'static [(&'static str, &'static str)],
}

macro_rules! file {
    ($dir:literal, $path:literal) => {
        ($path, include_str!(concat!("../../../templates/", $dir, "/", $path)))
    };
}

pub const TEMPLATES: &[Template] = &[
    Template {
        id: "fastapi",
        title: "FastAPI",
        files: &[file!("fastapi", "api/main.py"), file!("fastapi", "requirements.txt"), file!("fastapi", ".gitignore")],
    },
    Template {
        id: "node",
        title: "Node.js HTTP server",
        files: &[file!("node", "server.mjs"), file!("node", "package.json"), file!("node", ".gitignore")],
    },
];

pub fn template(id: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.id == id)
}

impl Template {
    /// Its files, written into `dir`.
    pub fn write(&self, dir: &Path) -> Result<()> {
        for (path, text) in self.files {
            let to = dir.join(path);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(to, text)?;
        }
        Ok(())
    }
}

/// A project to make.
#[derive(Debug, Clone, Deserialize)]
pub struct New {
    pub project: String,
    /// A template's id, to deploy at once; none makes an empty project.
    pub template: Option<String>,
    pub actor: Option<String>,
}

/// A project made.
#[derive(Debug, Clone, Serialize)]
pub struct Made {
    pub project: String,
    /// Its first deployment, building, when it was made from a template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<Started>,
    /// Why there's no first deployment from the template. The project is there all the same.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deploy_error: Option<String>,
}

/// Make a project: empty (its first `forkrail deploy --project NAME` fills it), or from a template,
/// whose production deployment starts building at once. Refused if the name is taken, by a project
/// or by another's function in any region. Like a project made from a repository, it stays made
/// whatever fails after.
pub async fn create(ctx: &Ctx, n: New) -> Result<Made> {
    check_project(&n.project)?;
    let chosen = match n.template.as_deref() {
        Some(id) => Some(template(id).ok_or_else(|| anyhow::anyhow!("no template {id:?}"))?),
        None => None,
    };
    let key = state::project_key(&n.project);
    let taken = || anyhow::anyhow!("a project named {} exists already: pick another name", n.project);
    if ctx.store.exists(&key).await? {
        return Err(taken());
    }
    // A project renamed to it, or whose domain its address is, answers to the name too.
    crate::project::check_new(ctx, &n.project).await?;
    let function = lambda::function_name(&n.project);
    ctx.lambda.check(&function, &n.project).await?;
    for region in ctx.store.settings().await?.regions.iter().filter(|r| **r != ctx.install.home_region) {
        ctx.lambda_in(region).check(&function, &n.project).await?;
    }
    if !ctx.store.create(&key, &new_project(&n.project)).await? {
        return Err(taken());
    }
    let Some(t) = chosen else {
        return Ok(Made { project: n.project, deployment: None, deploy_error: None });
    };
    let (deployment, deploy_error) = match deploy(ctx, &n.project, t, n.actor).await {
        Ok(started) => (Some(started), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    Ok(Made { project: n.project, deployment, deploy_error })
}

/// The template's files, uploaded and built as the project's production deployment.
async fn deploy(ctx: &Ctx, project: &str, t: &Template, actor: Option<String>) -> Result<Started> {
    let tmp = TempDir::new("template")?;
    t.write(tmp.path())?;
    let upload = remote::upload(ctx, tmp.path(), project).await?;
    remote::start(
        ctx,
        Start {
            project: project.to_owned(),
            target: Target::Production,
            upload,
            git: None,
            deployer: actor,
            publish: PublishOptions::default(),
            via: Via::Dashboard,
            no_cache: false,
            github_deployment: None,
            previous_sha: None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_write_files_a_build_recognises() {
        for t in TEMPLATES {
            let dir = TempDir::new("template-test").unwrap();
            t.write(dir.path()).unwrap();
            let entry = if t.id == "fastapi" { "api/main.py" } else { "server.mjs" };
            assert!(dir.path().join(entry).is_file(), "{} has {entry}", t.id);
            assert!(!t.files.iter().any(|(p, _)| p.contains("..") || p.starts_with('/')));
        }
        assert!(template("fastapi").is_some() && template("rails").is_none());
    }
}
