//! System variables: what a deployment's build and its app are told about it, as Vercel tells
//! them (`VERCEL_ENV`, `VERCEL_URL`, `VERCEL_GIT_*` …, its docs' list read 2026-11-15), so code
//! moved from Vercel reads what it expects, with forkrail's values; and forkrail's own names for the
//! main ones (`FORKRAIL_ENV`, `FORKRAIL_URL` …). A project's own variables of the same name win.
//!
//! Builds get `build` (build.rs); the app gets `runtime` (publish.rs), and the runtime adds
//! `VERCEL_REGION` and `FORKRAIL_REGION` itself, since a deployment runs in more than one region.

use std::collections::BTreeMap;

use crate::deploy::{BYPASS_SECRET_ENV, branch_host, hosts_for};
use crate::state::{GitCommit, Target};

/// Vercel cuts a commit's message here.
const MAX_MESSAGE_BYTES: usize = 2048;

/// The deployment the variables describe.
pub struct Deployment<'a> {
    /// The project's id: its addresses are made of it.
    pub project: &'a str,
    /// What it's called now (its id unless it was renamed): the name the app is told.
    pub name: &'a str,
    pub id: &'a str,
    pub target: Target,
    pub base_domain: &'a str,
    pub git: Option<&'a GitCommit>,
    /// Its branch's host, `<project>-git-<branch>.<base>`, when it takes one: a preview of a
    /// pushed branch other than the production branch.
    pub branch_url: Option<&'a str>,
    /// The project's env vars, for the automation bypass secret, which Vercel's name carries too.
    pub user: &'a BTreeMap<String, String>,
    /// Skew protection was on as it was built (skew.rs): frameworks then put its id on their
    /// requests, and the app is told so too, as Vercel tells it.
    pub skew: bool,
}

/// What the app sees.
pub fn runtime(d: &Deployment) -> BTreeMap<String, String> {
    let target = d.target.as_str();
    let own_host = hosts_for(d.project, d.id, d.base_domain, false).remove(0);
    let production = format!("{}.{}", d.project, d.base_domain);
    let mut env = BTreeMap::new();
    let mut both = |vercel: &str, forkrail: Option<&str>, value: String| {
        if let Some(name) = forkrail {
            env.insert(name.to_owned(), value.clone());
        }
        env.insert(vercel.to_owned(), value);
    };
    both("VERCEL", None, "1".to_owned());
    both("VERCEL_ENV", Some("FORKRAIL_ENV"), target.to_owned());
    both("VERCEL_TARGET_ENV", None, target.to_owned());
    both("VERCEL_URL", Some("FORKRAIL_URL"), own_host);
    both("VERCEL_PROJECT_PRODUCTION_URL", Some("FORKRAIL_PROJECT_PRODUCTION_URL"), production);
    both("VERCEL_DEPLOYMENT_ID", Some("FORKRAIL_DEPLOYMENT_ID"), d.id.to_owned());
    both("VERCEL_PROJECT_ID", None, d.project.to_owned());
    both("VERCEL_PROJECT_NAME", Some("FORKRAIL_PROJECT_NAME"), d.name.to_owned());
    if let Some(branch_url) = d.branch_url {
        both("VERCEL_BRANCH_URL", Some("FORKRAIL_BRANCH_URL"), branch_url.to_owned());
    }
    if d.skew {
        both(crate::skew::ENV, Some(crate::skew::FORKRAIL_ENV), "1".to_owned());
    }
    if let Some(secret) = d.user.get(BYPASS_SECRET_ENV) {
        both("VERCEL_AUTOMATION_BYPASS_SECRET", None, secret.clone());
    }
    if let Some(git) = d.git {
        let (owner, slug) = git.repo.split_once('/').unwrap_or(("", &git.repo));
        both("VERCEL_GIT_PROVIDER", None, "github".to_owned());
        both("VERCEL_GIT_REPO_OWNER", None, owner.to_owned());
        both("VERCEL_GIT_REPO_SLUG", None, slug.to_owned());
        both("VERCEL_GIT_COMMIT_REF", Some("FORKRAIL_GIT_COMMIT_REF"), git.branch.clone());
        both("VERCEL_GIT_COMMIT_SHA", Some("FORKRAIL_GIT_COMMIT_SHA"), git.full_sha.clone().unwrap_or_else(|| git.sha.clone()));
        both("VERCEL_GIT_COMMIT_MESSAGE", None, cut(&git.message, MAX_MESSAGE_BYTES));
        both("VERCEL_GIT_COMMIT_AUTHOR_NAME", None, git.author.clone().unwrap_or_default());
        both("VERCEL_GIT_PULL_REQUEST_ID", None, String::new());
    }
    env
}

/// What the build sees: the app's, and `CI` and the commit before (`VERCEL_GIT_PREVIOUS_SHA`),
/// which Vercel gives builds only.
pub fn build(d: &Deployment, previous_sha: Option<&str>) -> BTreeMap<String, String> {
    let mut env = runtime(d);
    env.insert("CI".to_owned(), "1".to_owned());
    // Vercel's build sets it beside VERCEL (its CLI's `build` command), and tools detect Vercel by
    // either (std-env, which Nitro and Nuxt ask).
    env.insert("NOW_BUILDER".to_owned(), "1".to_owned());
    if d.git.is_some() {
        env.insert("VERCEL_GIT_PREVIOUS_SHA".to_owned(), previous_sha.unwrap_or_default().to_owned());
    }
    env
}

/// A branch's host for `d`'s build, before publishing knows whether it takes it: a pushed preview
/// of a branch other than `HEAD`.
pub fn build_branch_url(project: &str, base: &str, target: Target, pushed: bool, git: Option<&GitCommit>) -> Option<String> {
    let git = git.filter(|g| pushed && target == Target::Preview && !g.branch.is_empty() && g.branch != "HEAD")?;
    Some(branch_host(project, &git.branch, base))
}

fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_vercel_names_them_with_forkrail_values() {
        let git = GitCommit {
            repo: "ada/shop".into(),
            branch: "feature/cart".into(),
            sha: "abc1234".into(),
            message: "é".repeat(1500),
            full_sha: Some("a".repeat(40)),
            author: Some("Ada Lovelace".into()),
        };
        let user: BTreeMap<String, String> = [(BYPASS_SECRET_ENV.to_owned(), "s3cret".to_owned())].into();
        let d = Deployment { project: "shop", name: "shop", id: "a1b2c3d4", target: Target::Preview, base_domain: "example.dev", git: Some(&git), branch_url: Some("shop-git-feature-cart.example.dev"), user: &user, skew: true };
        let env = runtime(&d);
        assert_eq!((env["VERCEL"].as_str(), env["VERCEL_ENV"].as_str(), env["FORKRAIL_ENV"].as_str()), ("1", "preview", "preview"));
        assert_eq!(env["VERCEL_URL"], "shop-a1b2c3d4.example.dev");
        assert_eq!(env["FORKRAIL_URL"], env["VERCEL_URL"]);
        assert_eq!(env["VERCEL_PROJECT_PRODUCTION_URL"], "shop.example.dev");
        assert_eq!(env["VERCEL_BRANCH_URL"], "shop-git-feature-cart.example.dev");
        assert_eq!((env["VERCEL_DEPLOYMENT_ID"].as_str(), env["VERCEL_PROJECT_NAME"].as_str()), ("a1b2c3d4", "shop"));
        assert_eq!((env["VERCEL_GIT_REPO_OWNER"].as_str(), env["VERCEL_GIT_REPO_SLUG"].as_str(), env["VERCEL_GIT_COMMIT_REF"].as_str()), ("ada", "shop", "feature/cart"));
        assert_eq!(env["VERCEL_GIT_COMMIT_SHA"].len(), 40);
        assert_eq!(env["VERCEL_GIT_COMMIT_MESSAGE"].len(), 2048, "cut between characters at 2048 bytes");
        assert_eq!(env["VERCEL_AUTOMATION_BYPASS_SECRET"], "s3cret");
        assert_eq!((env["VERCEL_SKEW_PROTECTION_ENABLED"].as_str(), env["FORKRAIL_SKEW_PROTECTION_ENABLED"].as_str()), ("1", "1"));
        // Build-only ones stay out of the app's.
        assert!(!env.contains_key("CI") && !env.contains_key("VERCEL_GIT_PREVIOUS_SHA"));
        let built = build(&d, Some("prev"));
        assert_eq!((built["CI"].as_str(), built["VERCEL_GIT_PREVIOUS_SHA"].as_str(), built["NOW_BUILDER"].as_str()), ("1", "prev", "1"));
        assert!(!env.contains_key("NOW_BUILDER"), "only builds have it");
        // No commit, no branch: only what there is.
        let plain = runtime(&Deployment { git: None, branch_url: None, target: Target::Production, user: &BTreeMap::new(), skew: false, ..d });
        assert!(plain.keys().all(|k| !k.starts_with("VERCEL_GIT_")) && !plain.contains_key("VERCEL_BRANCH_URL") && !plain.contains_key("VERCEL_AUTOMATION_BYPASS_SECRET"));
        assert!(!plain.contains_key("VERCEL_SKEW_PROTECTION_ENABLED"), "only when it's on, as on Vercel");
        assert_eq!(plain["VERCEL_ENV"], "production");
        // Renamed: the app is told its new name; its id and addresses stay the first name's.
        let renamed = runtime(&Deployment { name: "store", ..d });
        assert_eq!((renamed["VERCEL_PROJECT_NAME"].as_str(), renamed["FORKRAIL_PROJECT_NAME"].as_str(), renamed["VERCEL_PROJECT_ID"].as_str()), ("store", "store", "shop"));
        assert_eq!((renamed["VERCEL_URL"].as_str(), renamed["VERCEL_PROJECT_PRODUCTION_URL"].as_str()), ("shop-a1b2c3d4.example.dev", "shop.example.dev"));
        assert_eq!(build_branch_url("shop", "example.dev", Target::Preview, true, Some(&git)).as_deref(), Some("shop-git-feature-cart.example.dev"));
        assert_eq!(build_branch_url("shop", "example.dev", Target::Production, true, Some(&git)), None);
        assert_eq!(build_branch_url("shop", "example.dev", Target::Preview, false, Some(&git)), None);
    }
}
