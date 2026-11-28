//! `forkrail cache …`: purging a project's response cache by tag, as Vercel's `vercel cache` does
//! (vercel.com/docs/cli/cache and /docs/caching/cdn-cache/purge, read 2026-11-16; the rules are in
//! `forkrail_common::purge`).
//!
//! - `invalidate --tag a,b` (or `--srcimg`): what the routers kept with those tags is served once
//!   more (`STALE`) while it's made again.
//! - `dangerously-delete --tag a,b [--revalidation-deadline-seconds N]`: it's made again while the
//!   visitor waits (`REVALIDATED`); with a deadline, served stale until then.
//! - `purge`: everything the project's deployments have kept (the `*` tag), invalidated: each
//!   entry is served once more while it's made again, so a purge never stampedes the functions
//!   (the author, 2026-11-16; `dangerously-delete --tag '*'` deletes).
//!
//! Each applies to the project's production and preview environments unless `--target` names one
//! (Vercel's API takes `target` the same way). A purge goes in the routing table, in the project's
//! entry (`ProjectRouting::purges`, the latest 50), which every router reads within a second and
//! applies itself; purges from functions take the faster path over the routers' own link. As on
//! Vercel, it's an owner's or a member's to do; anyone sees the latest.

use anyhow::{Result, bail};
use clap::Subcommand;
use forkrail_common::purge::{self, ALL, MAX_TABLE_PURGES, MAX_TAGS_PER_CALL, Purge, PurgeOp};
use forkrail_common::routes::Routes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api;
use crate::deploy::{Ctx, check_project, project_record};
use crate::state::ROUTES_KEY;

/// What the dashboard, the CLI and the API ask of a project's cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum CacheOp {
    /// The project's latest purges.
    List { project: String },
    /// Served stale once while made again.
    Invalidate {
        project: String,
        tags: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        src_images: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    /// Made again while the visitor waits, after the deadline if one is given.
    Delete {
        project: String,
        tags: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        src_images: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline_s: Option<u64>,
    },
    /// Everything kept, invalidated (`vercel cache purge`).
    Purge {
        project: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
}

impl CacheOp {
    pub fn name(&self) -> &'static str {
        match self {
            CacheOp::List { .. } => "list",
            CacheOp::Invalidate { .. } => "invalidate",
            CacheOp::Delete { .. } => "delete",
            CacheOp::Purge { .. } => "purge",
        }
    }

    pub fn changes(&self) -> bool {
        !matches!(self, CacheOp::List { .. })
    }

    pub fn project(&self) -> &str {
        match self {
            CacheOp::List { project } | CacheOp::Invalidate { project, .. } | CacheOp::Delete { project, .. } | CacheOp::Purge { project, .. } => project,
        }
    }

    /// The purge it asks for (none for a look), checked, made at `at_ms` by `by`.
    pub fn purge(&self, at_ms: u64, by: Option<&str>) -> Result<Option<Purge>> {
        let mut purge = match self.clone() {
            CacheOp::List { .. } => return Ok(None),
            CacheOp::Invalidate { tags, src_images, target, .. } => Purge { op: PurgeOp::Invalidate, tags, src_images, target, deadline_s: None, at_ms, by: None },
            CacheOp::Delete { tags, src_images, target, deadline_s, .. } => Purge { op: PurgeOp::Delete, tags, src_images, target, deadline_s, at_ms, by: None },
            CacheOp::Purge { target, .. } => Purge { op: PurgeOp::Invalidate, tags: vec![ALL.to_owned()], src_images: false, target, deadline_s: None, at_ms, by: None },
        };
        purge.by = by.map(str::to_owned);
        purge.check(MAX_TAGS_PER_CALL).map_err(anyhow::Error::msg)?;
        Ok(Some(purge))
    }
}

/// A project's latest purges, as the CLI and the dashboard show them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub project: String,
    /// Newest last.
    pub purges: Vec<Purge>,
}

/// Add a purge to a project's entry in the table, keeping the latest `MAX_TABLE_PURGES`.
pub fn add(routes: &mut Routes, project: &str, purge: Purge) {
    let entry = routes.projects.entry(project.to_owned()).or_default();
    entry.purges.push(purge);
    let over = entry.purges.len().saturating_sub(MAX_TABLE_PURGES);
    entry.purges.drain(..over);
}

pub async fn run(ctx: &Ctx, op: CacheOp, actor: Option<&str>) -> Result<Value> {
    check_project(op.project())?;
    let project = op.project().to_owned();
    // A project that isn't there has nothing kept, and its name nothing to do in the table.
    project_record(ctx, &project).await?;
    let Some(purge) = op.purge(crate::state::now_ms(), actor)? else {
        let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
        let purges = routes.projects.get(&project).map(|p| p.purges.clone()).unwrap_or_default();
        return Ok(json!(View { project, purges }));
    };
    let kept = purge.clone();
    let routes = ctx
        .store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            add(&mut routes, &project, kept.clone());
            routes.validate().map_err(anyhow::Error::msg)?;
            Ok(routes)
        })
        .await?;
    let purges = routes.projects.get(&project).map(|p| p.purges.clone()).unwrap_or_default();
    Ok(json!({"project": project, "purge": purge, "purges": purges}))
}

/// `forkrail cache …`, as Vercel's `vercel cache`.
#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Mark what PROJECT's deployments kept with these tags (or source images) stale: served
    /// once more while it's made again in the background (`STALE`).
    Invalidate {
        /// The project (Vercel's `--project`).
        #[arg(long, short = 'p')]
        project: String,
        /// Tags, comma-separated (`*` is everything).
        #[arg(long, conflicts_with = "srcimg", required_unless_present = "srcimg")]
        tag: Option<String>,
        /// A source image, exactly as the app names it: its optimized copies.
        #[arg(long)]
        srcimg: Option<String>,
        /// production or preview; both when not given.
        #[arg(long)]
        target: Option<String>,
        /// Don't ask first.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Delete what PROJECT's deployments kept with these tags (or source images): made again
    /// while the visitor waits (`REVALIDATED`). Many paths at once can stampede the functions:
    /// prefer `invalidate`.
    #[command(name = "dangerously-delete")]
    DangerouslyDelete {
        /// The project (Vercel's `--project`).
        #[arg(long, short = 'p')]
        project: String,
        /// Tags, comma-separated (`*` is everything).
        #[arg(long, conflicts_with = "srcimg", required_unless_present = "srcimg")]
        tag: Option<String>,
        /// A source image, exactly as the app names it: its optimized copies.
        #[arg(long)]
        srcimg: Option<String>,
        /// production or preview; both when not given.
        #[arg(long)]
        target: Option<String>,
        /// Served stale (and made again) for this many seconds first; deleted after, if nothing
        /// asked for it meanwhile (up to a year).
        #[arg(long = "revalidation-deadline-seconds")]
        deadline: Option<u64>,
        /// Don't ask first.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Invalidate everything PROJECT's deployments kept (its pages and CDN answers): each is
    /// served once more while it's made again. `dangerously-delete --tag '*'` deletes.
    Purge {
        /// The project (Vercel's `--project`).
        #[arg(long, short = 'p')]
        project: String,
        /// production or preview; both when not given.
        #[arg(long)]
        target: Option<String>,
        /// Vercel's `--type`: `cdn` or `all` (forkrail has no separate data cache: both are this).
        #[arg(long = "type", default_value = "all")]
        kind: String,
        /// Don't ask first.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// PROJECT's latest purges.
    Ls { project: String },
}

/// The op a command asks for, and whether it was confirmed beforehand.
pub fn op_of(command: CacheCommand) -> Result<(CacheOp, bool)> {
    let tags = |tag: Option<String>, srcimg: Option<String>| -> (Vec<String>, bool) {
        match (tag, srcimg) {
            (Some(tag), _) => (purge::split_tags(&tag).map(str::to_owned).collect(), false),
            (None, Some(src)) => (vec![src], true),
            (None, None) => (Vec::new(), false),
        }
    };
    Ok(match command {
        CacheCommand::Invalidate { project, tag, srcimg, target, yes } => {
            let (tags, src_images) = tags(tag, srcimg);
            (CacheOp::Invalidate { project, tags, src_images, target }, yes)
        }
        CacheCommand::DangerouslyDelete { project, tag, srcimg, target, deadline, yes } => {
            let (tags, src_images) = tags(tag, srcimg);
            (CacheOp::Delete { project, tags, src_images, target, deadline_s: deadline }, yes)
        }
        CacheCommand::Purge { project, target, kind, yes } => {
            if !matches!(kind.as_str(), "all" | "cdn" | "data") {
                bail!("Invalid cache type {kind:?}. Valid types are: cdn, data, all");
            }
            if kind == "data" {
                bail!("forkrail has no data cache apart from the CDN's: `forkrail cache purge` (or --type cdn) purges what there is");
            }
            (CacheOp::Purge { project, target }, yes)
        }
        CacheCommand::Ls { project } => (CacheOp::List { project }, true),
    })
}

/// What the op is about to do, as Vercel's CLI says it.
fn describe(op: &CacheOp) -> String {
    let scope = |target: &Option<String>| target.as_deref().map_or(String::new(), |t| format!(" ({t})"));
    let items = |tags: &[String], src: bool| {
        if src {
            format!("source image {}", tags.join(", "))
        } else {
            format!("{} {}", if tags.len() == 1 { "tag" } else { "tags" }, tags.join(","))
        }
    };
    match op {
        CacheOp::Invalidate { project, tags, src_images, target } => {
            format!("You are about to invalidate all cached content associated with {} for project {project}{}", items(tags, *src_images), scope(target))
        }
        CacheOp::Delete { project, tags, src_images, target, .. } => {
            format!("You are about to dangerously delete all cached content associated with {} for project {project}{}", items(tags, *src_images), scope(target))
        }
        CacheOp::Purge { project, target } => format!("You are about to purge the CDN cache for project {project}{}", scope(target)),
        CacheOp::List { project } => format!("{project}'s purges"),
    }
}

/// Run `forkrail cache …` through the install (AWS credentials) or its API (a login).
pub async fn run_cli(via: crate::routing_cli::Via<'_>, command: CacheCommand) -> Result<()> {
    let (op, yes) = op_of(command)?;
    // Checked before asking, so a mistake isn't confirmed first.
    let checked = op.purge(0, None)?;
    if op.changes() && !yes {
        let message = describe(&op);
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            bail!("{message}. To continue, run it again with --yes.");
        }
        eprint!("{message}. Continue? [Y/n] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim().eq_ignore_ascii_case("n") || answer.trim().eq_ignore_ascii_case("no") {
            eprintln!("Canceled.");
            return Ok(());
        }
    }
    let answer = match via {
        crate::routing_cli::Via::Aws(ctx) => run(ctx, op.clone(), None).await?,
        crate::routing_cli::Via::Api(client) => send(client, &op).await?,
    };
    match &op {
        CacheOp::List { project } => {
            let view: View = serde_json::from_value(answer)?;
            if view.purges.is_empty() {
                eprintln!("==> {project}: no purges in the table");
            }
            for p in &view.purges {
                println!("{}", line(p));
            }
        }
        _ => {
            let purge = checked.expect("a change has a purge");
            let what = match (op.name(), purge.tags.first().map(String::as_str)) {
                ("purge", _) | (_, Some(ALL)) => "the CDN cache".to_owned(),
                _ if purge.src_images => format!("source image {}", purge.tags.join(", ")),
                _ => format!("{} {}", if purge.tags.len() == 1 { "tag" } else { "tags" }, purge.tags.join(",")),
            };
            let done = match op.name() {
                "invalidate" => "Successfully invalidated",
                "delete" => "Successfully deleted",
                _ => "Successfully purged",
            };
            eprintln!("{done} {what} for project {}: every router has it within a few seconds", op.project());
        }
    }
    Ok(())
}

async fn send(client: &api::Client, op: &CacheOp) -> Result<Value> {
    let body = serde_json::to_value(op)?;
    if op.changes() { client.post("cache", body).await } else { client.ask("cache", body).await }
}

/// One purge as `forkrail cache ls` prints it.
fn line(p: &Purge) -> String {
    let when = crate::state::rfc3339(p.at_ms / 1000);
    let op = match (p.op, p.deadline_s) {
        (PurgeOp::Invalidate, _) => "invalidate".to_owned(),
        (PurgeOp::Delete, Some(d)) => format!("delete (deadline {d}s)"),
        (PurgeOp::Delete, None) => "delete".to_owned(),
    };
    let what = if p.src_images { format!("srcimg {}", p.tags.join(", ")) } else { p.tags.join(",") };
    let target = p.target.as_deref().unwrap_or("all");
    let by = p.by.as_deref().map_or(String::new(), |b| format!(" by {b}"));
    format!("{when}  {op:<22} {target:<10} {what}{by}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_become_checked_purges() {
        let (op, yes) = op_of(CacheCommand::Invalidate { project: "shop".into(), tag: Some("a, b".into()), srcimg: None, target: None, yes: true }).unwrap();
        assert!(yes);
        let p = op.purge(5, Some("me")).unwrap().unwrap();
        assert_eq!((p.op, p.tags.clone(), p.at_ms, p.by.as_deref(), p.target), (PurgeOp::Invalidate, vec!["a".to_owned(), "b".to_owned()], 5, Some("me"), None));
        let (op, _) = op_of(CacheCommand::DangerouslyDelete { project: "shop".into(), tag: None, srcimg: Some("/a.png?x=1,2".into()), target: Some("preview".into()), deadline: Some(60), yes: false }).unwrap();
        let p = op.purge(5, None).unwrap().unwrap();
        assert_eq!((p.tags, p.src_images, p.deadline_s), (vec!["/a.png?x=1,2".to_owned()], true, Some(60)));
        let (op, _) = op_of(CacheCommand::Purge { project: "shop".into(), target: None, kind: "cdn".into(), yes: false }).unwrap();
        let p = op.purge(5, None).unwrap().unwrap();
        assert_eq!((p.op, p.tags), (PurgeOp::Invalidate, vec![ALL.to_owned()]));
        assert!(op_of(CacheCommand::Purge { project: "shop".into(), target: None, kind: "edge".into(), yes: false }).is_err());
        // Vercel's limits: 16 tags a call, 256 bytes a tag, a target that's one of two.
        let many = (0..17).map(|i| format!("t{i}")).collect::<Vec<_>>().join(",");
        let (op, _) = op_of(CacheCommand::Invalidate { project: "shop".into(), tag: Some(many), srcimg: None, target: None, yes: true }).unwrap();
        assert!(op.purge(5, None).is_err());
        let (op, _) = op_of(CacheCommand::Invalidate { project: "shop".into(), tag: Some("a".into()), srcimg: None, target: Some("dev".into()), yes: true }).unwrap();
        assert!(op.purge(5, None).is_err());
        // As the dashboard's API sends them.
        let op: CacheOp = serde_json::from_value(json!({"op": "delete", "project": "shop", "tags": ["a"], "deadline_s": 30})).unwrap();
        assert_eq!(op.name(), "delete");
        assert!(op.changes() && !CacheOp::List { project: "shop".into() }.changes());
    }

    #[test]
    fn the_table_keeps_the_latest_purges() {
        let mut routes = Routes::default();
        for i in 0..(MAX_TABLE_PURGES as u64 + 5) {
            let p = Purge { op: PurgeOp::Invalidate, tags: vec!["a".into()], src_images: false, target: None, deadline_s: None, at_ms: i, by: None };
            add(&mut routes, "shop", p);
        }
        let kept = &routes.projects["shop"].purges;
        assert_eq!((kept.len(), kept[0].at_ms, kept.last().unwrap().at_ms), (MAX_TABLE_PURGES, 5, MAX_TABLE_PURGES as u64 + 4));
        routes.validate().unwrap();
        // Old routers read the table as before: an entry with purges is a plain project entry.
        let text = serde_json::to_string(&routes).unwrap();
        assert!(text.contains("\"purges\""));
    }
}
