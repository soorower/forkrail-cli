//! How long a project's deployments are kept: Vercel's Deployment Retention (its docs
//! `/docs/deployment-retention`, read 2026-11-15).
//!
//! A period per state, canceled, errored, preview and production (Vercel's Pro default: 30 days
//! each), set for the whole install (`state/retention.json`) and over it per project (`Project`'s
//! `retention`), field by field. A deployment older than its state's period is deleted, unless
//! something keeps it whatever its age (`Exception`):
//! - Vercel's: among the 10 newest deployments, the 20 newest ready production deployments, the 20
//!   newest ready previews; production now (a production domain points at it); the latest ready
//!   deployment of a branch with a domain; a preview with a custom domain; the latest preview of a
//!   branch still in use (Vercel: with an open pull request; here: its branch's URL still points at
//!   it, which holds until the branch is deleted, a little more than Vercel keeps).
//! - forkrail's own, so a project keeps everything the fixed counts before this kept (`retained` in
//!   deploy.rs; tested): the last 5 deployments that served production (rollback targets: a preview
//!   promoted here keeps its target), every deployment still building, and the 10 newest that
//!   failed or were canceled (so their logs can be read). A deployment restored from the deleted
//!   ones counts its age from when it was restored.
//!
//! Deleting is in two steps, as on Vercel. A deployment deleted (by the policy, by hand, or with its
//! branch) leaves the routing table and the project's list for its `deleted` list, and nothing of
//! it is touched: its Lambda versions in every region, its record, package, source, static files
//! and build log stay, so a restore puts it back exactly as it was, at the same address. It stays
//! restorable for 30 days, less when it's younger: by the policy, the policy's period (never more
//! than 30 days); by hand, its age (at least a day). Only after that does `purge` delete it for
//! good: a deleted Lambda version can't come back.
//!
//! The policy runs after every publish (`deploy::prune`) and once a day for every project, from
//! EventBridge Scheduler through the control function (deploy/install/retention.yaml). `plan` is
//! the dry run: what a policy, the one in force or one proposed, would delete now, and why the rest
//! stays.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use forkrail_common::routes::{Deployment, Routes, valid_id};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

use crate::deploy::{self, BRANCH_INFIX, Ctx, check_project, log, project_record};
use crate::lambda;
use crate::state::{self, DeploymentRecord, Project, ROUTES_KEY, Status, Summary, Target, now_rfc3339, parse_rfc3339};
use crate::storage;

const DAY_S: u64 = 86_400;

/// Now, Unix seconds. A debug build may be told another time in FORKRAIL_RETENTION_NOW (RFC 3339),
/// to try on a throwaway project what the policy does weeks from now; a release build (control,
/// the release CLI) never reads it.
pub fn now() -> u64 {
    #[cfg(debug_assertions)]
    if let Some(at) = std::env::var("FORKRAIL_RETENTION_NOW").ok().and_then(|t| parse_rfc3339(&t)) {
        return at;
    }
    state::now_secs()
}
/// Vercel's default period for every state (Hobby and Pro).
pub const DEFAULT_DAYS: u32 = 30;
/// The longest a period may be set to, ten years; or `unlimited`.
pub const MAX_DAYS: u32 = 3_650;
/// The longest a deleted deployment stays restorable.
pub const RECOVERY_DAYS: u64 = 30;
/// Vercel's exceptions on Pro and Enterprise.
pub const KEEP_NEWEST: usize = 10;
pub const KEEP_READY_PRODUCTION: usize = 20;
pub const KEEP_READY_PREVIEWS: usize = 20;
/// forkrail's own exceptions: the counts kept before retention policies.
pub const KEEP_ROLLBACK_TARGETS: usize = deploy::KEEP_PRODUCTION;
pub const KEEP_FAILED: usize = 10;
/// The install's default policy.
pub const RETENTION_KEY: &str = "state/retention.json";
/// Who deletes by the policy, in a deleted deployment's `by`.
pub const BY_POLICY: &str = "retention";

/// How long deployments of one state are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Days(u32),
    /// Never deleted for its age; the exceptions don't matter.
    Unlimited,
}

impl Period {
    pub const DEFAULT: Period = Period::Days(DEFAULT_DAYS);

    /// `30d`, `2w`, `6m` (30-day months), `1y` (365 days), a plain number of days, or `unlimited`.
    pub fn parse(text: &str) -> Result<Period> {
        let t = text.trim().to_ascii_lowercase();
        if t == "unlimited" || t == "never" {
            return Ok(Period::Unlimited);
        }
        let (digits, unit) = match t.find(|c: char| !c.is_ascii_digit()) {
            Some(at) => t.split_at(at),
            None => (t.as_str(), "d"),
        };
        let n: u32 = digits.parse().with_context(|| format!("{text:?}: a period is like 30d, 2w, 6m, 1y or unlimited"))?;
        let days = match unit {
            "d" | "day" | "days" => Some(n),
            "w" | "week" | "weeks" => n.checked_mul(7),
            "m" | "month" | "months" => n.checked_mul(30),
            "y" | "year" | "years" => n.checked_mul(365),
            _ => bail!("{text:?}: a period is like 30d, 2w, 6m, 1y or unlimited"),
        };
        let period = Period::Days(days.unwrap_or(u32::MAX));
        period.check()?;
        Ok(period)
    }

    pub fn check(self) -> Result<()> {
        match self {
            Period::Days(d) if !(1..=MAX_DAYS).contains(&d) => bail!("{d} days: a period is 1 day to {MAX_DAYS} days, or unlimited"),
            _ => Ok(()),
        }
    }

    pub fn seconds(self) -> Option<u64> {
        match self {
            Period::Days(d) => Some(u64::from(d) * DAY_S),
            Period::Unlimited => None,
        }
    }

    /// "30 days", "1 year", "unlimited".
    pub fn words(self) -> String {
        match self {
            Period::Unlimited => "unlimited".to_owned(),
            Period::Days(1) => "1 day".to_owned(),
            Period::Days(d) if d % 365 == 0 => plural(u64::from(d / 365), "year"),
            Period::Days(d) if d % 30 == 0 && d >= 60 => plural(u64::from(d / 30), "month"),
            Period::Days(d) if d % 7 == 0 && d < 30 => plural(u64::from(d / 7), "week"),
            Period::Days(d) => plural(u64::from(d), "day"),
        }
    }
}

fn plural(n: u64, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
}

/// A number of days, or `"unlimited"`, as Vercel's API gives days.
impl Serialize for Period {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Period::Days(d) => s.serialize_u32(*d),
            Period::Unlimited => s.serialize_str("unlimited"),
        }
    }
}

impl<'de> Deserialize<'de> for Period {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Days(u64),
            Text(String),
        }
        let period = match Raw::deserialize(d)? {
            Raw::Days(n) => Period::Days(u32::try_from(n).map_err(serde::de::Error::custom)?),
            Raw::Text(t) => Period::parse(&t).map_err(serde::de::Error::custom)?,
        };
        period.check().map_err(serde::de::Error::custom)?;
        Ok(period)
    }
}

/// Periods by state, each unset where it follows what's under it (a project's, the install's
/// default; then Vercel's 30 days).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canceled: Option<Period>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errored: Option<Period>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Period>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub production: Option<Period>,
}

impl Policy {
    pub fn is_empty(&self) -> bool {
        *self == Policy::default()
    }

    /// This policy's periods, and `under`'s where this one sets none.
    pub fn over(&self, under: &Policy) -> Policy {
        Policy {
            canceled: self.canceled.or(under.canceled),
            errored: self.errored.or(under.errored),
            preview: self.preview.or(under.preview),
            production: self.production.or(under.production),
        }
    }

    pub fn periods(&self) -> Periods {
        Periods {
            canceled: self.canceled.unwrap_or(Period::DEFAULT),
            errored: self.errored.unwrap_or(Period::DEFAULT),
            preview: self.preview.unwrap_or(Period::DEFAULT),
            production: self.production.unwrap_or(Period::DEFAULT),
        }
    }

    pub fn check(&self) -> Result<()> {
        for p in [self.canceled, self.errored, self.preview, self.production].into_iter().flatten() {
            p.check()?;
        }
        Ok(())
    }

    /// `change` applied: a field given replaces, `Some(None)` goes back to what's under it.
    pub fn changed(&self, change: &PolicyChange) -> Policy {
        let pick = |now: Option<Period>, to: Option<Option<Period>>| to.unwrap_or(now);
        Policy {
            canceled: pick(self.canceled, change.canceled),
            errored: pick(self.errored, change.errored),
            preview: pick(self.preview, change.preview),
            production: pick(self.production, change.production),
        }
    }
}

/// The periods in force for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Periods {
    pub canceled: Period,
    pub errored: Period,
    pub preview: Period,
    pub production: Period,
}

impl Periods {
    pub fn of(&self, class: Class) -> Option<Period> {
        match class {
            Class::Canceled => Some(self.canceled),
            Class::Errored => Some(self.errored),
            Class::Preview => Some(self.preview),
            Class::Production => Some(self.production),
            Class::Building => None,
        }
    }
}

/// A change to a policy: absent leaves a period as it is, null goes back to what's under it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyChange {
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub canceled: Option<Option<Period>>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub errored: Option<Option<Period>>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub preview: Option<Option<Period>>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub production: Option<Option<Period>>,
}

impl PolicyChange {
    pub fn is_empty(&self) -> bool {
        *self == PolicyChange::default()
    }
}

/// A field that's there, even as null: `Some(None)`. Absent is `None`, by `default`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// The install's default policy, `state/retention.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstallPolicy {
    #[serde(default)]
    pub default: Policy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<String>,
}

/// A deleted deployment, restorable until `until`. Everything of it is still stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trashed {
    pub id: String,
    pub target: Target,
    pub created: String,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub version: String,
    pub deleted_at: String,
    /// Restorable until then; deleted for good after (`purge`).
    pub until: String,
    /// Who deleted it: a person, `retention` for the policy, or nobody known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Why, in words.
    pub reason: String,
    /// Its entry in the routing table, put back by a restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<Deployment>,
    /// The hosts that pointed at it (its own, a branch's), given back by a restore where nothing
    /// else has taken them since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
}

impl Trashed {
    fn until_secs(&self) -> u64 {
        parse_rfc3339(&self.until).unwrap_or(0)
    }
}

/// What a deployment counts as, for its period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Production,
    Preview,
    Errored,
    Canceled,
    Building,
}

/// What keeps a deployment whatever its age.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exception {
    /// Production now: `<project>.<base>` or a production domain points at it.
    Production,
    /// Still being built.
    Building,
    /// Among the 10 newest deployments.
    Newest,
    /// Among the 20 newest ready production deployments.
    NewestProduction,
    /// Among the 20 newest ready previews.
    NewestPreview,
    /// One of the last 5 that served production: rollback targets.
    RollbackTarget,
    /// A domain that follows a branch points at it: that branch's latest ready deployment.
    BranchDomain,
    /// A preview a custom domain points at.
    CustomDomain,
    /// Its branch's URL points at it: the latest preview of a branch that still exists.
    ActiveBranch,
    /// Among the 10 newest that failed or were canceled.
    NewestFailed,
}

impl Exception {
    pub fn words(self) -> &'static str {
        match self {
            Exception::Production => "production now",
            Exception::Building => "still building",
            Exception::Newest => "among the 10 newest",
            Exception::NewestProduction => "among the 20 newest ready production deployments",
            Exception::NewestPreview => "among the 20 newest ready previews",
            Exception::RollbackTarget => "one of the last 5 production deployments (a rollback target)",
            Exception::BranchDomain => "a branch's domain points at it",
            Exception::CustomDomain => "a preview with a custom domain",
            Exception::ActiveBranch => "the latest preview of a branch that still exists",
            Exception::NewestFailed => "among the 10 newest that failed or were canceled",
        }
    }
}

/// What the policy does with one deployment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub target: Target,
    pub class: Class,
    pub status: Status,
    pub created: String,
    /// When its period ends (RFC 3339); none for an unlimited period or one still building.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Past its period.
    pub expired: bool,
    /// What keeps it whatever its age, every one that applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept_by: Vec<Exception>,
    /// Deleted by the next sweep: past its period, and nothing keeps it.
    pub delete: bool,
}

/// What `plan` reads besides the project.
pub struct Facts<'a> {
    pub routes: &'a Routes,
    pub base_domain: &'a str,
}

/// When a deployment was made, Unix seconds; one without a date counts as the oldest.
fn created_secs(created: &str) -> u64 {
    parse_rfc3339(created).unwrap_or(0)
}

fn class_of(status: Status, target: Target) -> Class {
    match status {
        Status::Queued | Status::Building => Class::Building,
        Status::Error => Class::Errored,
        Status::Canceled => Class::Canceled,
        Status::Ready => match target {
            Target::Production => Class::Production,
            Target::Preview => Class::Preview,
        },
    }
}

/// What the policy with `periods` does with each of `p`'s deployments at `now` (Unix seconds),
/// newest first. Pure: the dry run and the sweep both use it.
pub fn plan(p: &Project, facts: &Facts, periods: &Periods, now: u64) -> Vec<Decision> {
    let mut list: Vec<&Summary> = p.deployments.iter().collect();
    // Newest first; a stable sort keeps the list's order between equal dates.
    list.sort_by_key(|d| std::cmp::Reverse(created_secs(&d.created)));
    let status = |d: &Summary| d.status.at(&d.created, now);

    let mut keep: HashMap<String, BTreeSet<Exception>> = HashMap::new();
    let add = |id: &str, why: Exception, keep: &mut HashMap<String, BTreeSet<Exception>>| {
        keep.entry(id.to_owned()).or_default().insert(why);
    };
    for d in list.iter().take(KEEP_NEWEST) {
        add(&d.id, Exception::Newest, &mut keep);
    }
    let ready = |d: &&&Summary| status(d) == Status::Ready;
    for d in list.iter().filter(ready).filter(|d| d.target == Target::Production).take(KEEP_READY_PRODUCTION) {
        add(&d.id, Exception::NewestProduction, &mut keep);
    }
    for d in list.iter().filter(ready).filter(|d| d.target == Target::Preview).take(KEEP_READY_PREVIEWS) {
        add(&d.id, Exception::NewestPreview, &mut keep);
    }
    for d in list.iter().filter(|d| matches!(status(d), Status::Error | Status::Canceled)).take(KEEP_FAILED) {
        add(&d.id, Exception::NewestFailed, &mut keep);
    }
    for d in list.iter().filter(|d| status(d).in_progress()) {
        add(&d.id, Exception::Building, &mut keep);
    }
    for id in p.production_history.iter().take(KEEP_ROLLBACK_TARGETS) {
        add(id, Exception::RollbackTarget, &mut keep);
    }
    if let Some(id) = &p.production {
        add(id, Exception::Production, &mut keep);
    }
    // What the routing table points at it, besides its own address.
    let production_host = format!("{}.{}", p.name, facts.base_domain);
    let preview_ids: HashSet<&str> = p.deployments.iter().filter(|d| d.target == Target::Preview).map(|d| d.id.as_str()).collect();
    for (host, id) in &facts.routes.hosts {
        if !p.deployments.iter().any(|d| d.id == *id) {
            continue;
        }
        if *host == production_host {
            add(id, Exception::Production, &mut keep);
        }
        if let Some(domain) = facts.routes.domains.get(host).filter(|d| d.project == p.name) {
            if domain.production() {
                add(id, Exception::Production, &mut keep);
            }
            if domain.branch.is_some() {
                add(id, Exception::BranchDomain, &mut keep);
            }
            if preview_ids.contains(id.as_str()) && p.production.as_deref() != Some(id) {
                add(id, Exception::CustomDomain, &mut keep);
            }
        }
        // As the fixed counts did: any address with the branch infix (`<project>-git-<branch>`).
        if host.contains(BRANCH_INFIX) {
            add(id, Exception::ActiveBranch, &mut keep);
        }
    }
    list.iter()
        .map(|d| {
            let status = status(d);
            let class = class_of(status, d.target);
            // A restored deployment's age counts from its restore.
            let start = created_secs(&d.created).max(p.restored.get(&d.id).and_then(|at| parse_rfc3339(at)).unwrap_or(0));
            let ends = periods.of(class).and_then(Period::seconds).map(|s| start.saturating_add(s));
            let expired = ends.is_some_and(|e| e <= now);
            let kept_by: Vec<Exception> = keep.get(&d.id).map(|s| s.iter().copied().collect()).unwrap_or_default();
            Decision {
                id: d.id.clone(),
                target: d.target,
                class,
                status,
                created: d.created.clone(),
                expires: ends.map(rfc3339),
                expired,
                delete: expired && kept_by.is_empty() && class != Class::Building,
                kept_by,
            }
        })
        .collect()
}

fn rfc3339(secs: u64) -> String {
    chrono::DateTime::from_timestamp(i64::try_from(secs).unwrap_or(i64::MAX), 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// How long a deployment deleted at `age_s` seconds old stays restorable: 30 days once it's 30
/// days old; younger, the policy's period if the policy deleted it (`by_policy`), else its age, at
/// least a day (Vercel's rules).
pub fn recovery_s(age_s: u64, by_policy: Option<Period>) -> u64 {
    let max = RECOVERY_DAYS * DAY_S;
    if age_s >= max {
        return max;
    }
    match by_policy.and_then(Period::seconds) {
        Some(period) => period.clamp(DAY_S, max),
        None => age_s.clamp(DAY_S, max),
    }
}

/// The install's default policy; Vercel's 30 days everywhere if none was set.
pub async fn install_policy(ctx: &Ctx) -> Result<InstallPolicy> {
    Ok(ctx.store.get_value::<InstallPolicy>(RETENTION_KEY).await?.unwrap_or_default())
}

/// What a policy decides for a project now, with what it reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectView {
    pub project: String,
    /// The project's own periods, the install's default, and what's in force.
    pub own: Policy,
    pub install: Policy,
    pub periods: Periods,
    /// Every kept deployment, newest first.
    pub deployments: Vec<Decision>,
    /// Deleted deployments, restorable until their time; newest deletion first.
    pub deleted: Vec<Trashed>,
    /// Whether this is a policy proposed and not saved (a dry run).
    #[serde(default)]
    pub proposed: bool,
}

impl ProjectView {
    pub fn doomed(&self) -> Vec<&Decision> {
        self.deployments.iter().filter(|d| d.delete).collect()
    }
}

/// What the policy in force (or `proposed`, over the project's own) does with `project` now.
pub async fn view(ctx: &Ctx, project: &str, proposed: Option<&PolicyChange>) -> Result<ProjectView> {
    let p = project_record(ctx, project).await?;
    let install = install_policy(ctx).await?.default;
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let settings = ctx.store.settings().await?;
    Ok(view_of(&p, &install, &routes, &settings.base_domain, proposed, now()))
}

fn view_of(p: &Project, install: &Policy, routes: &Routes, base_domain: &str, proposed: Option<&PolicyChange>, now: u64) -> ProjectView {
    let own = match proposed {
        Some(change) => p.retention.changed(change),
        None => p.retention.clone(),
    };
    let periods = own.over(install).periods();
    let deployments = plan(p, &Facts { routes, base_domain }, &periods, now);
    let mut deleted = p.deleted.clone();
    deleted.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    ProjectView { project: p.name.clone(), own, install: install.clone(), periods, deployments, deleted, proposed: proposed.is_some() }
}

/// How a deployment came to be deleted.
#[derive(Debug, Clone)]
pub struct Removal {
    /// Who: a person, `retention`, or nobody known.
    pub by: Option<String>,
    pub reason: String,
    /// Deleted by the policy, with the period it passed: that bounds how long it stays restorable.
    pub period: Option<Period>,
}

impl Removal {
    pub fn by_hand(by: Option<&str>) -> Self {
        Removal { by: by.map(str::to_owned), reason: match by { Some(who) => format!("deleted by {who}"), None => "deleted".to_owned() }, period: None }
    }
}

/// Delete deployments `ids` of `project`, restorably: they stop being routed and move to the
/// project's deleted list, with everything of them kept until their time is up. Production is
/// refused. Returns the ids deleted (ones already gone are skipped).
pub async fn trash(ctx: &Ctx, project: &str, ids: &[String], removal: &Removal) -> Result<Vec<String>> {
    trash_with(ctx, project, ids, |_| Some(removal.clone())).await
}

/// `trash`, with each one's removal from `why` (none: it stays, as things are now).
async fn trash_with<F>(ctx: &Ctx, project: &str, ids: &[String], why: F) -> Result<Vec<String>>
where
    F: Fn(&Project) -> Option<Removal>,
{
    let wanted: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let now = now();
    let mut moved: Vec<String> = Vec::new();
    ctx.store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = current.context("no such project")?;
            moved.clear();
            if p.production.as_deref().is_some_and(|d| wanted.contains(d)) {
                bail!("refusing to delete the production deployment");
            }
            let Some(removal) = why(&p) else { return Ok(p) };
            let mut kept = Vec::with_capacity(p.deployments.len());
            for d in std::mem::take(&mut p.deployments) {
                if !wanted.contains(d.id.as_str()) {
                    kept.push(d);
                    continue;
                }
                let age = now.saturating_sub(created_secs(&d.created));
                let until = now + recovery_s(age, removal.period);
                let hosts = routes.hosts.iter().filter(|(_, at)| **at == d.id).map(|(h, _)| h.clone()).collect();
                moved.push(d.id.clone());
                p.deleted.retain(|t| t.id != d.id);
                p.deleted.push(Trashed {
                    route: routes.deployments.get(&d.id).cloned(),
                    hosts,
                    id: d.id,
                    target: d.target,
                    created: d.created,
                    status: d.status,
                    version: d.version,
                    deleted_at: rfc3339(now),
                    until: rfc3339(until),
                    by: removal.by.clone(),
                    reason: removal.reason.clone(),
                });
            }
            p.deployments = kept;
            p.production_history.retain(|d| !wanted.contains(d.as_str()));
            for id in &moved {
                p.restored.remove(id);
            }
            Ok(p)
        })
        .await?;
    if moved.is_empty() {
        return Ok(moved);
    }
    let gone: BTreeSet<&str> = moved.iter().map(String::as_str).collect();
    ctx.store
        .update::<Routes, _>(ROUTES_KEY, |routes| {
            let mut routes = routes.unwrap_or_default();
            routes.hosts.retain(|_, d| !gone.contains(d.as_str()));
            routes.deployments.retain(|d, _| !gone.contains(d.as_str()));
            Ok(routes)
        })
        .await?;
    Ok(moved)
}

/// What a sweep of one project did (or, dry, would do).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Swept {
    pub project: String,
    /// Deleted by the policy now, restorable for a while.
    pub deleted: Vec<String>,
    /// Deleted ones whose time was up, gone for good.
    pub purged: Vec<String>,
    #[serde(default)]
    pub dry_run: bool,
    /// Why the project was left alone, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Run the policy on `project`: delete (restorably) what it deletes, and purge deleted deployments
/// whose time is up. `dry_run` only says what it would do.
pub async fn sweep(ctx: &Ctx, project: &str, dry_run: bool) -> Result<Swept> {
    check_project(project)?;
    let install = install_policy(ctx).await?.default;
    let settings = ctx.store.settings().await?;
    sweep_with(ctx, project, &install, &settings.base_domain, dry_run).await
}

async fn sweep_with(ctx: &Ctx, project: &str, install: &Policy, base_domain: &str, dry_run: bool) -> Result<Swept> {
    let p = project_record(ctx, project).await?;
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let now = now();
    let periods = p.retention.over(install).periods();
    let first = plan(&p, &Facts { routes: &routes, base_domain }, &periods, now);
    let doomed: Vec<String> = first.iter().filter(|d| d.delete).map(|d| d.id.clone()).collect();
    let due: Vec<String> = p.deleted.iter().filter(|t| t.until_secs() <= now).map(|t| t.id.clone()).collect();
    let mut out = Swept { project: project.to_owned(), dry_run, ..Default::default() };
    if dry_run {
        out.deleted = doomed;
        out.purged = due;
        return Ok(out);
    }
    if !doomed.is_empty() {
        // Decided again on the project as it is when it's written, so nothing that became
        // production, or newer, meanwhile goes.
        let ids = doomed.clone();
        out.deleted = trash_with(ctx, project, &ids, |fresh| {
            let still: Vec<Decision> = plan(fresh, &Facts { routes: &routes, base_domain }, &fresh.retention.over(install).periods(), now);
            let again: BTreeSet<&str> = still.iter().filter(|d| d.delete).map(|d| d.id.as_str()).collect();
            // All or none: the ids asked for must all still go, else this write is skipped.
            ids.iter().all(|id| again.contains(id.as_str())).then(|| Removal {
                by: Some(BY_POLICY.to_owned()),
                reason: format!("older than the policy keeps ({})", reasons(fresh, &ids, &periods, now)),
                period: shortest(&periods, fresh, &ids, now),
            })
        })
        .await?;
    }
    if !due.is_empty() {
        out.purged = purge(ctx, project, &due).await?;
    }
    Ok(out)
}

/// Which periods the deleted passed, in words: "preview 30 days".
fn reasons(p: &Project, ids: &[String], periods: &Periods, now: u64) -> String {
    let mut seen = BTreeSet::new();
    for d in p.deployments.iter().filter(|d| ids.contains(&d.id)) {
        let class = class_of(d.status.at(&d.created, now), d.target);
        if let Some(period) = periods.of(class) {
            seen.insert(format!("{} {}", class_name(class), period.words()));
        }
    }
    seen.into_iter().collect::<Vec<_>>().join(", ")
}

fn class_name(class: Class) -> &'static str {
    match class {
        Class::Production => "production",
        Class::Preview => "preview",
        Class::Errored => "errored",
        Class::Canceled => "canceled",
        Class::Building => "building",
    }
}

/// The shortest period the deleted passed: how long they stay restorable when younger than 30 days.
fn shortest(periods: &Periods, p: &Project, ids: &[String], now: u64) -> Option<Period> {
    p.deployments
        .iter()
        .filter(|d| ids.contains(&d.id))
        .filter_map(|d| periods.of(class_of(d.status.at(&d.created, now), d.target)))
        .min_by_key(|period| period.seconds().unwrap_or(u64::MAX))
}

/// Run the policy on every project, one after another. A project that fails is reported in its
/// line and the others go on.
pub async fn sweep_all(ctx: &Ctx, dry_run: bool) -> Result<Vec<Swept>> {
    let install = install_policy(ctx).await?.default;
    let settings = ctx.store.settings().await?;
    let mut out = Vec::new();
    for key in ctx.store.list(forkrail_common::install::PROJECTS_PREFIX).await? {
        let Some(name) = key.strip_prefix(forkrail_common::install::PROJECTS_PREFIX).and_then(|k| k.strip_suffix(".json")) else { continue };
        match sweep_with(ctx, name, &install, &settings.base_domain, dry_run).await {
            Ok(swept) => out.push(swept),
            Err(e) => out.push(Swept { project: name.to_owned(), dry_run, error: Some(format!("{e:#}")), ..Default::default() }),
        }
    }
    Ok(out)
}

/// Delete for good deleted deployments `ids` of `project`: off the project's deleted list first
/// (so none is restored halfway), then their Lambda versions in every region, records, build logs
/// and the stored files nothing left uses. One the routing table serves again (a restore that
/// didn't finish) is skipped. Returns the ids purged.
pub async fn purge(ctx: &Ctx, project: &str, ids: &[String]) -> Result<Vec<String>> {
    let function = lambda::function_name(project);
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let wanted: BTreeSet<&str> = ids.iter().map(String::as_str).filter(|id| !routes.deployments.contains_key(*id)).collect();
    for id in ids.iter().filter(|id| !wanted.contains(id.as_str())) {
        eprintln!("warning: deleted deployment {id} is in the routing table again: left alone");
    }
    let now = now();
    let mut taken: Vec<Trashed> = Vec::new();
    let p = ctx
        .store
        .update::<Project, _>(&state::project_key(project), |current| {
            let mut p = current.context("no such project")?;
            taken.clear();
            // Only those whose time is still up, as the list is now.
            let (gone, kept): (Vec<Trashed>, Vec<Trashed>) = std::mem::take(&mut p.deleted).into_iter().partition(|t| wanted.contains(t.id.as_str()) && t.until_secs() <= now);
            p.deleted = kept;
            taken = gone;
            Ok(p)
        })
        .await?;
    if taken.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = taken.iter().map(|t| t.id.clone()).collect();
    // Versions still in use: by a kept or still-restorable deployment, or by anything routed.
    let mut in_use: BTreeSet<String> = p.deployments.iter().map(|d| d.version.clone()).chain(p.deleted.iter().map(|t| t.version.clone())).collect();
    // Routed versions in the home region: deployments' own, and replicas of those whose own
    // region is another (a project's functions may run elsewhere).
    let home = ctx.install.home_region.as_str();
    in_use.extend(routes.deployments.values().filter(|d| d.function == function).flat_map(|d| d.versions(home)).filter(|(r, _)| *r == home).map(|(_, v)| v.to_owned()));
    let records = storage::records(ctx, &ids).await?;
    let mut versions = BTreeSet::new();
    let mut replicas: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut logs = Vec::new();
    for record in &records {
        if record.project != project {
            bail!("deployment {} belongs to {}, not {project}", record.id, record.project);
        }
        logs.extend(record.log.as_ref().and_then(|l| l.kept.clone()));
        if record.version.is_empty() {
            continue;
        }
        if record.region.as_deref().is_none_or(|r| r == ctx.install.home_region) {
            versions.insert(record.version.clone());
        } else {
            replicas.entry(record.region.clone().unwrap_or_default()).or_default().insert(record.version.clone());
        }
        for (region, version) in &record.replicas {
            replicas.entry(region.clone()).or_default().insert(version.clone());
        }
    }
    // The same versions elsewhere are still used by the kept, through their records.
    let kept_ids: Vec<String> = p.deployments.iter().map(|d| d.id.clone()).chain(p.deleted.iter().map(|t| t.id.clone())).collect();
    for version in versions.difference(&in_use) {
        ctx.lambda.delete_version(&function, project, version).await?;
    }
    // The kept deployments' versions in other regions: the routing table no longer names the
    // deleted ones', so their records say.
    let kept_records = if replicas.keys().any(|r| *r != ctx.install.home_region) { storage::records(ctx, &kept_ids).await? } else { Vec::new() };
    for (region, versions) in &replicas {
        if *region == ctx.install.home_region {
            continue;
        }
        let in_use: BTreeSet<&str> = routes
            .deployments
            .values()
            .filter(|d| d.function == function)
            .flat_map(|d| d.versions(&ctx.install.home_region))
            .filter(|(r, _)| r == region)
            .map(|(_, v)| v)
            .collect();
        let kept_there = replica_versions(&kept_records, region);
        let lambda = ctx.lambda_in(region);
        for version in versions.iter().filter(|v| !in_use.contains(v.as_str()) && !kept_there.contains(*v)) {
            lambda.delete_version(&function, project, version).await.with_context(|| format!("in {region}"))?;
        }
    }
    for id in &ids {
        ctx.store.delete(&state::deployment_key(id)).await?;
        ctx.store.delete(&forkrail_common::install::cancel_key(id)).await?;
    }
    for key in logs {
        ctx.store.delete(&key).await?;
    }
    let files = storage::release(ctx, project, &records, &kept_ids).await;
    log(&format!("{project}: {} deleted deployment{} purged for good ({}){}", ids.len(), if ids.len() == 1 { "" } else { "s" }, ids.join(", "), if files > 0 { format!(", {files} stored files with them") } else { String::new() }));
    Ok(ids)
}

/// The versions in `region` of the deployments of `records`.
fn replica_versions(records: &[DeploymentRecord], region: &str) -> BTreeSet<String> {
    records
        .iter()
        .flat_map(|r| {
            let home = r.region.as_deref().filter(|h| *h == region).map(|_| r.version.clone());
            r.replicas.get(region).cloned().into_iter().chain(home)
        })
        .collect()
}

/// What a restore put back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Restored {
    pub id: String,
    pub project: String,
    /// The addresses it answers on again.
    pub hosts: Vec<String>,
}

/// Put deleted deployment `id` back, as it was: its entry in the routing table, its own address
/// and any other address that pointed at it and is still free, and its place in the project's
/// list. Its age then counts from now for the policy.
pub async fn restore(ctx: &Ctx, id: &str) -> Result<Restored> {
    if !valid_id(id) {
        bail!("{id:?} isn't a deployment id");
    }
    let rec: DeploymentRecord = ctx.store.get_value(&state::deployment_key(id)).await?.with_context(|| format!("no deployment {id}: it may have been deleted for good"))?;
    let project = rec.project.clone();
    let p = project_record(ctx, &project).await?;
    let Some(t) = p.deleted.iter().find(|t| t.id == id).cloned() else {
        if p.deployments.iter().any(|d| d.id == id) {
            bail!("{id} isn't deleted");
        }
        bail!("{id} isn't among {project}'s deleted deployments");
    };
    if t.until_secs() <= now() {
        bail!("{id} could be restored until {}; it is being deleted for good", t.until);
    }
    let settings = ctx.store.settings().await?;
    let own = format!("{project}-{id}.{}", settings.base_domain);
    let mut given = Vec::new();
    if let Some(route) = &t.route {
        // With the project's cap on requests per worker as it is now, as its other deployments have.
        let mut route = route.clone();
        route.follow_max_inflight(p.max_inflight);
        ctx.store
            .update::<Routes, _>(ROUTES_KEY, |routes| {
                let mut routes = routes.unwrap_or_default();
                given.clear();
                routes.deployments.insert(id.to_owned(), route.clone());
                for host in std::iter::once(&own).chain(t.hosts.iter()) {
                    // Never one that serves something else now (by its name or a wildcard's), or
                    // another project's, or a custom domain no longer this project's.
                    if routes.lookup(host).is_some() || given.contains(host) {
                        continue;
                    }
                    let base = settings.base_domain.as_str();
                    let under_base = host == base || host.ends_with(&format!(".{base}"));
                    if !under_base && !routes.domain(host).is_some_and(|(_, d)| d.project == project) {
                        continue;
                    }
                    if routes.domain(host).is_some_and(|(_, d)| d.project != project) {
                        continue;
                    }
                    routes.hosts.insert(host.clone(), id.to_owned());
                    given.push(host.clone());
                }
                routes.validate().map_err(anyhow::Error::msg)?;
                Ok(routes)
            })
            .await?;
    }
    ctx.store
        .update::<Project, _>(&state::project_key(&project), |current| {
            let mut p = current.context("no such project")?;
            let Some(at) = p.deleted.iter().position(|t| t.id == id) else {
                bail!("{id} was restored or deleted for good meanwhile");
            };
            let t = p.deleted.remove(at);
            if !p.deployments.iter().any(|d| d.id == id) {
                let summary = Summary { id: t.id.clone(), target: t.target, created: t.created.clone(), version: t.version.clone(), status: t.status };
                let at = p.deployments.iter().position(|d| created_secs(&d.created) < created_secs(&t.created)).unwrap_or(p.deployments.len());
                p.deployments.insert(at, summary);
            }
            p.restored.insert(id.to_owned(), rfc3339(now()));
            Ok(p)
        })
        .await?;
    log(&format!("{project}: restored {id}{}", if given.is_empty() { String::new() } else { format!(" at {}", given.join(", ")) }));
    Ok(Restored { id: id.to_owned(), project, hosts: given })
}

/// Change the install's default policy (`project` none) or a project's own. `reset` clears the
/// project's own first (back to the install's), or the install's (back to 30 days everywhere).
pub async fn set_policy(ctx: &Ctx, project: Option<&str>, change: &PolicyChange, reset: bool, actor: Option<&str>) -> Result<Policy> {
    match project {
        Some(project) => {
            check_project(project)?;
            let p = ctx
                .store
                .update::<Project, _>(&state::project_key(project), |current| {
                    let mut p = current.with_context(|| format!("no project {project}"))?;
                    let base = if reset { Policy::default() } else { p.retention.clone() };
                    p.retention = base.changed(change);
                    p.retention.check()?;
                    Ok(p)
                })
                .await?;
            Ok(p.retention)
        }
        None => {
            let saved = ctx
                .store
                .update::<InstallPolicy, _>(RETENTION_KEY, |current| {
                    let current = current.unwrap_or_default();
                    let base = if reset { Policy::default() } else { current.default };
                    let default = base.changed(change);
                    default.check()?;
                    Ok(InstallPolicy { default, changed_by: actor.map(str::to_owned), changed_at: Some(now_rfc3339()) })
                })
                .await?;
            Ok(saved.default)
        }
    }
}

/// What the dashboard, the API and `--aws` ask of retention (control's action `retention`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RetentionOp {
    /// The policy in force, and with a project what it does with each deployment and why, and
    /// the deleted ones.
    Get {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    /// The dry run of a change: what `change` (over the project's own, or the install's default
    /// without `project`) would delete now. Nothing is saved.
    Plan {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(default)]
        change: PolicyChange,
        #[serde(default)]
        reset: bool,
    },
    /// Save a change to a project's policy, or the install's default without `project`.
    Set {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(default)]
        change: PolicyChange,
        #[serde(default)]
        reset: bool,
    },
    /// Run the policy now, on one project or every one; `dry_run` only lists.
    Sweep {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<String>,
        #[serde(default)]
        dry_run: bool,
    },
    /// Put a deleted deployment back.
    Restore { id: String },
}

impl RetentionOp {
    pub fn name(&self) -> &'static str {
        match self {
            RetentionOp::Get { .. } => "get",
            RetentionOp::Plan { .. } => "plan",
            RetentionOp::Set { .. } => "set",
            RetentionOp::Sweep { .. } => "sweep",
            RetentionOp::Restore { .. } => "restore",
        }
    }

    /// Whether it changes anything (else anyone may ask it).
    pub fn changes(&self) -> bool {
        match self {
            RetentionOp::Get { .. } | RetentionOp::Plan { .. } => false,
            RetentionOp::Sweep { dry_run, .. } => !dry_run,
            RetentionOp::Set { .. } | RetentionOp::Restore { .. } => true,
        }
    }

    pub fn project(&self) -> Option<&str> {
        match self {
            RetentionOp::Get { project } | RetentionOp::Plan { project, .. } | RetentionOp::Set { project, .. } | RetentionOp::Sweep { project, .. } => project.as_deref(),
            RetentionOp::Restore { .. } => None,
        }
    }

    /// Whether it acts on the whole install: the install's default, or a sweep of every project.
    pub fn install_wide(&self) -> bool {
        matches!(self, RetentionOp::Set { project: None, .. } | RetentionOp::Sweep { project: None, .. })
    }
}

/// The install's default, and what a change to it would delete in each project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallView {
    pub default: Policy,
    pub periods: Periods,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<String>,
    /// With a plan: per project, what it would delete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectDoomed>,
    #[serde(default)]
    pub proposed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDoomed {
    pub project: String,
    /// Whether the project sets periods of its own, which win over the install's.
    pub own: bool,
    pub delete: Vec<String>,
    pub kept: usize,
}

/// Answer one op.
pub async fn run(ctx: &Ctx, op: RetentionOp, actor: Option<&str>) -> Result<Value> {
    Ok(match op {
        RetentionOp::Get { project: Some(project) } => json!(view(ctx, &project, None).await?),
        RetentionOp::Get { project: None } => json!(install_view(ctx, None, false).await?),
        RetentionOp::Plan { project: Some(project), change, reset } => {
            let change = if reset { reset_change(&change) } else { change };
            json!(view(ctx, &project, Some(&change)).await?)
        }
        RetentionOp::Plan { project: None, change, reset } => json!(install_view(ctx, Some(&change), reset).await?),
        RetentionOp::Set { project, change, reset } => {
            change.check()?;
            set_policy(ctx, project.as_deref(), &change, reset, actor).await?;
            match project {
                Some(project) => json!(view(ctx, &project, None).await?),
                None => json!(install_view(ctx, None, false).await?),
            }
        }
        RetentionOp::Sweep { project: Some(project), dry_run } => json!([sweep(ctx, &project, dry_run).await?]),
        RetentionOp::Sweep { project: None, dry_run } => json!(sweep_all(ctx, dry_run).await?),
        RetentionOp::Restore { id } => json!(restore(ctx, &id).await?),
    })
}

impl PolicyChange {
    pub fn check(&self) -> Result<()> {
        for p in [self.canceled, self.errored, self.preview, self.production].into_iter().flatten().flatten() {
            p.check()?;
        }
        Ok(())
    }
}

/// `change` after a reset: every period not given goes back to what's under it.
fn reset_change(change: &PolicyChange) -> PolicyChange {
    PolicyChange {
        canceled: Some(change.canceled.flatten()),
        errored: Some(change.errored.flatten()),
        preview: Some(change.preview.flatten()),
        production: Some(change.production.flatten()),
    }
}

/// The install's default, and with `change` what it would delete in every project.
async fn install_view(ctx: &Ctx, change: Option<&PolicyChange>, reset: bool) -> Result<InstallView> {
    let saved = install_policy(ctx).await?;
    let default = match change {
        Some(change) if reset => Policy::default().changed(change),
        Some(change) => saved.default.changed(change),
        None => saved.default.clone(),
    };
    let mut view = InstallView {
        periods: default.periods(),
        default: default.clone(),
        changed_by: saved.changed_by,
        changed_at: saved.changed_at,
        projects: Vec::new(),
        proposed: change.is_some(),
    };
    if change.is_some() {
        let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
        let settings = ctx.store.settings().await?;
        let now = now();
        for key in ctx.store.list(forkrail_common::install::PROJECTS_PREFIX).await? {
            let Some(p) = ctx.store.get_value::<Project>(&key).await? else { continue };
            let decisions = plan(&p, &Facts { routes: &routes, base_domain: &settings.base_domain }, &p.retention.over(&default).periods(), now);
            view.projects.push(ProjectDoomed {
                project: p.name.clone(),
                own: !p.retention.is_empty(),
                delete: decisions.iter().filter(|d| d.delete).map(|d| d.id.clone()).collect(),
                kept: decisions.iter().filter(|d| !d.delete).count(),
            });
        }
    }
    Ok(view)
}

/// A project's view for a terminal.
pub fn print_view(v: &ProjectView) {
    let source = |own: Option<Period>, install: Option<Period>| match (own, install) {
        (Some(_), _) => "this project's",
        (None, Some(_)) => "the install's default",
        (None, None) => "default",
    };
    println!("{}{}", v.project, if v.proposed { ": with the proposed policy (not saved)" } else { "" });
    for (name, period, own, install) in [
        ("canceled", v.periods.canceled, v.own.canceled, v.install.canceled),
        ("errored", v.periods.errored, v.own.errored, v.install.errored),
        ("preview", v.periods.preview, v.own.preview, v.install.preview),
        ("production", v.periods.production, v.own.production, v.install.production),
    ] {
        println!("  {name:<11} {:<12} ({})", period.words(), source(own, install));
    }
    println!();
    if v.deployments.is_empty() {
        println!("No deployments.");
    }
    for d in &v.deployments {
        let fate = if d.delete {
            "DELETED by the next sweep".to_owned()
        } else if !d.kept_by.is_empty() && d.expired {
            format!("kept: {}", d.kept_by.iter().map(|e| e.words()).collect::<Vec<_>>().join("; "))
        } else if let Some(at) = &d.expires {
            let kept = if d.kept_by.is_empty() { String::new() } else { format!(" (then kept: {})", d.kept_by.iter().map(|e| e.words()).collect::<Vec<_>>().join("; ")) };
            format!("expires {}{kept}", &at[..10.min(at.len())])
        } else if d.class == Class::Building {
            "building".to_owned()
        } else {
            "kept (unlimited)".to_owned()
        };
        println!("  {}  {:<10} {:<9} {}  {fate}", d.id, class_name(d.class), d.target.as_str(), &d.created[..10.min(d.created.len())]);
    }
    let doomed = v.doomed().len();
    if doomed > 0 {
        println!("\n{doomed} deployment{} would be deleted (each restorable for up to {RECOVERY_DAYS} days).", if doomed == 1 { "" } else { "s" });
    }
    if !v.deleted.is_empty() {
        println!("\nRecently deleted (restore with `forkrail restore ID`):");
        for t in &v.deleted {
            println!("  {}  {:<10} deleted {}  restorable until {}  {}", t.id, t.target.as_str(), &t.deleted_at[..10.min(t.deleted_at.len())], &t.until[..16.min(t.until.len())], t.reason);
        }
    }
}

/// The install's default for a terminal.
pub fn print_install(v: &InstallView) {
    println!("Install default{}:", if v.proposed { " (proposed, not saved)" } else { "" });
    for (name, period, set) in [
        ("canceled", v.periods.canceled, v.default.canceled),
        ("errored", v.periods.errored, v.default.errored),
        ("preview", v.periods.preview, v.default.preview),
        ("production", v.periods.production, v.default.production),
    ] {
        println!("  {name:<11} {}{}", period.words(), if set.is_none() { " (default)" } else { "" });
    }
    if let (Some(by), Some(at)) = (&v.changed_by, &v.changed_at) {
        println!("  changed by {by} at {at}");
    }
    let doomed: usize = v.projects.iter().map(|p| p.delete.len()).sum();
    if v.proposed {
        println!();
        for p in v.projects.iter().filter(|p| !p.delete.is_empty()) {
            println!("  {:<24} {} would be deleted{}: {}", p.project, p.delete.len(), if p.own { " (it has periods of its own)" } else { "" }, p.delete.join(", "));
        }
        println!("{doomed} deployment{} would be deleted in all.", if doomed == 1 { "" } else { "s" });
    }
}

/// Sweeps' results for a terminal.
pub fn print_swept(list: &[Swept]) {
    let mut total = (0, 0);
    for s in list {
        if let Some(e) = &s.error {
            println!("  {:<24} left alone: {e}", s.project);
            continue;
        }
        if s.deleted.is_empty() && s.purged.is_empty() {
            continue;
        }
        let (del, pur) = if s.dry_run { ("would delete", "would purge") } else { ("deleted", "purged") };
        let mut parts = Vec::new();
        if !s.deleted.is_empty() {
            parts.push(format!("{del} {}", s.deleted.join(", ")));
        }
        if !s.purged.is_empty() {
            parts.push(format!("{pur} {}", s.purged.join(", ")));
        }
        println!("  {:<24} {}", s.project, parts.join("; "));
        total.0 += s.deleted.len();
        total.1 += s.purged.len();
    }
    let dry = list.first().is_some_and(|s| s.dry_run);
    println!(
        "{} deployment{} {}, {} deleted one{} {}{}",
        total.0,
        if total.0 == 1 { "" } else { "s" },
        if dry { "would be deleted" } else { "deleted (restorable)" },
        total.1,
        if total.1 == 1 { "" } else { "s" },
        if dry { "would be purged for good" } else { "purged for good" },
        if dry { ". Nothing was changed: add --yes to run it" } else { "" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use forkrail_common::routes::CustomDomain;

    const NOW: u64 = 1_800_000_000;
    const BASE: &str = "forkrail.test";

    fn at(days_ago: u64) -> String {
        rfc3339(NOW - days_ago * DAY_S)
    }

    fn summary(id: &str, target: Target, status: Status, days_ago: u64) -> Summary {
        Summary { id: id.into(), target, created: at(days_ago), version: "1".into(), status }
    }

    fn project(deployments: Vec<Summary>) -> Project {
        let mut deployments = deployments;
        deployments.sort_by_key(|d| std::cmp::Reverse(created_secs(&d.created)));
        Project { name: "p".into(), deployments, ..Default::default() }
    }

    fn decide(p: &Project, routes: &Routes) -> BTreeMap<String, Decision> {
        plan(p, &Facts { routes, base_domain: BASE }, &Policy::default().periods(), NOW).into_iter().map(|d| (d.id.clone(), d)).collect()
    }

    fn doomed(p: &Project, routes: &Routes) -> BTreeSet<String> {
        decide(p, routes).into_values().filter(|d| d.delete).map(|d| d.id).collect()
    }

    /// `n` old ready deployments of `target`, ids `<prefix>0`…, the first 100 days old, each a day
    /// older than the one before.
    fn old(prefix: &str, target: Target, status: Status, n: usize) -> Vec<Summary> {
        (0..n).map(|i| summary(&format!("{prefix}{i:02}"), target, status, 100 + i as u64)).collect()
    }

    #[test]
    fn periods_parse_and_read() {
        assert_eq!(Period::parse("30d").unwrap(), Period::Days(30));
        assert_eq!(Period::parse("2w").unwrap(), Period::Days(14));
        assert_eq!(Period::parse("6m").unwrap(), Period::Days(180));
        assert_eq!(Period::parse("1y").unwrap(), Period::Days(365));
        assert_eq!(Period::parse("45").unwrap(), Period::Days(45));
        assert_eq!(Period::parse("Unlimited").unwrap(), Period::Unlimited);
        assert!(Period::parse("0d").is_err());
        assert!(Period::parse("11y").is_err());
        assert!(Period::parse("3x").is_err());
        assert!(Period::parse("").is_err());
        assert_eq!(Period::Days(365).words(), "1 year");
        assert_eq!(Period::Days(60).words(), "2 months");
        assert_eq!(Period::Days(30).words(), "30 days");
        assert_eq!(Period::Days(7).words(), "1 week");
        let p: Policy = serde_json::from_str(r#"{"preview": 7, "production": "unlimited"}"#).unwrap();
        assert_eq!(p.preview, Some(Period::Days(7)));
        assert_eq!(p.production, Some(Period::Unlimited));
        assert_eq!(serde_json::to_value(&p).unwrap(), json!({"preview": 7, "production": "unlimited"}));
        assert!(serde_json::from_str::<Policy>(r#"{"preview": 0}"#).is_err());
        assert!(serde_json::from_str::<Policy>(r#"{"preview": 99999}"#).is_err());
    }

    #[test]
    fn a_project_over_the_install_over_the_default() {
        let install = Policy { preview: Some(Period::Days(7)), production: Some(Period::Days(90)), ..Default::default() };
        let own = Policy { preview: Some(Period::Days(3)), ..Default::default() };
        let periods = own.over(&install).periods();
        assert_eq!(periods.preview, Period::Days(3));
        assert_eq!(periods.production, Period::Days(90));
        assert_eq!(periods.errored, Period::DEFAULT);
        // A change: given replaces, null goes back to what's under it, absent stays.
        let change: PolicyChange = serde_json::from_str(r#"{"preview": null, "errored": 14}"#).unwrap();
        let changed = own.changed(&change);
        assert_eq!(changed.preview, None);
        assert_eq!(changed.errored, Some(Period::Days(14)));
        assert_eq!(changed.production, None);
    }

    #[test]
    fn young_deployments_stay_until_their_period_ends() {
        // 30 previews, all younger than 30 days: Vercel keeps them all, whatever the count.
        let p = project((0..30).map(|i| summary(&format!("y{i:02}"), Target::Preview, Status::Ready, i)).collect());
        let d = decide(&p, &Routes::default());
        assert!(d.values().all(|d| !d.delete && !d.expired));
        assert_eq!(d["y29"].expires, Some(rfc3339(NOW + DAY_S)));
    }

    #[test]
    fn the_ten_newest_stay() {
        // 15 old canceled deployments: the newest 10 by any count, plus the 10 newest failed.
        let mut list = old("c", Target::Preview, Status::Canceled, 15);
        // Make 12 newer old previews take the "newest" places, so canceled only keep NewestFailed.
        list.extend((0..12).map(|i| summary(&format!("n{i:02}"), Target::Preview, Status::Ready, 40 + i)));
        let p = project(list);
        let d = decide(&p, &Routes::default());
        for i in 0..10 {
            assert!(d[&format!("n{i:02}")].kept_by.contains(&Exception::Newest));
        }
        assert!(!d["n10"].kept_by.contains(&Exception::Newest));
        // n10, n11 are still among the 20 newest ready previews.
        assert!(!d["n11"].delete);
    }

    #[test]
    fn twenty_newest_ready_production_and_previews_stay() {
        let mut list = old("prod", Target::Production, Status::Ready, 25);
        list.extend(old("prev", Target::Preview, Status::Ready, 25));
        let p = project(list);
        let gone = doomed(&p, &Routes::default());
        let d = decide(&p, &Routes::default());
        for i in 0..20 {
            assert!(d[&format!("prod{i:02}")].kept_by.contains(&Exception::NewestProduction));
            assert!(d[&format!("prev{i:02}")].kept_by.contains(&Exception::NewestPreview));
        }
        for i in 20..25 {
            assert!(gone.contains(&format!("prod{i:02}")), "prod{i:02}");
            assert!(gone.contains(&format!("prev{i:02}")), "prev{i:02}");
        }
        assert_eq!(gone.len(), 10);
        // Failed ones don't count among the ready.
        let mut list = old("bad", Target::Production, Status::Error, 30);
        list.extend(old("prod", Target::Production, Status::Ready, 21));
        let d = decide(&project(list), &Routes::default());
        assert!(d["prod19"].kept_by.contains(&Exception::NewestProduction));
        assert!(d["prod20"].delete);
    }

    #[test]
    fn production_and_its_domains_stay() {
        let mut list = old("x", Target::Production, Status::Ready, 30);
        list.push(summary("live", Target::Production, Status::Ready, 400));
        let mut p = project(list);
        p.production = Some("live".into());
        let d = decide(&p, &Routes::default());
        assert!(d["live"].kept_by.contains(&Exception::Production));
        assert!(!d["live"].delete);
        // Production by the routing table (a production domain) also keeps it, even if the project
        // file says otherwise for a moment.
        let mut routes = Routes::default();
        routes.hosts.insert("p.forkrail.test".into(), "x29".into());
        routes.domains.insert("shop.example.com".into(), CustomDomain { project: "p".into(), ..domain() });
        routes.hosts.insert("shop.example.com".into(), "x28".into());
        let d = decide(&p, &routes);
        assert!(d["x29"].kept_by.contains(&Exception::Production));
        assert!(d["x28"].kept_by.contains(&Exception::Production));
        assert!(d["x27"].delete);
    }

    fn domain() -> CustomDomain {
        serde_json::from_value(json!({"project": "p"})).unwrap()
    }

    #[test]
    fn the_last_five_that_served_production_stay() {
        let mut list = old("x", Target::Production, Status::Ready, 30);
        // A preview promoted long ago, rolled back from since: still a rollback target.
        list.push(summary("promoted", Target::Preview, Status::Ready, 500));
        list.extend(old("pv", Target::Preview, Status::Ready, 25));
        let mut p = project(list);
        p.production = Some("x00".into());
        p.production_history = vec!["x00".into(), "promoted".into(), "x27".into(), "x28".into(), "x29".into(), "x26".into()];
        let d = decide(&p, &Routes::default());
        for id in ["promoted", "x27", "x28", "x29"] {
            assert!(d[id].kept_by.contains(&Exception::RollbackTarget), "{id}");
            assert!(!d[id].delete, "{id}");
        }
        // The sixth in the history isn't.
        assert!(d["x26"].delete);
    }

    #[test]
    fn branch_domains_custom_domains_and_branch_urls_keep_theirs() {
        let mut list = old("pv", Target::Preview, Status::Ready, 30);
        list.push(summary("feat", Target::Preview, Status::Ready, 300));
        list.push(summary("staging", Target::Preview, Status::Ready, 301));
        list.push(summary("custom", Target::Preview, Status::Ready, 302));
        let p = project(list);
        let mut routes = Routes::default();
        routes.hosts.insert("p-git-feat.forkrail.test".into(), "feat".into());
        routes.domains.insert("staging.example.com".into(), CustomDomain { branch: Some("staging".into()), ..domain() });
        routes.hosts.insert("staging.example.com".into(), "staging".into());
        // A custom domain the table points at a preview (Vercel's custom alias).
        routes.domains.insert("demo.example.com".into(), domain());
        routes.hosts.insert("demo.example.com".into(), "custom".into());
        let d = decide(&p, &routes);
        assert!(d["feat"].kept_by.contains(&Exception::ActiveBranch));
        assert!(d["staging"].kept_by.contains(&Exception::BranchDomain));
        assert!(d["staging"].kept_by.contains(&Exception::CustomDomain));
        assert!(d["custom"].kept_by.contains(&Exception::CustomDomain));
        for id in ["feat", "staging", "custom"] {
            assert!(!d[id].delete, "{id}");
        }
        // Another project's domain pointing here is nothing of this project's.
        let mut routes = Routes::default();
        routes.domains.insert("theirs.example.com".into(), CustomDomain { project: "other".into(), branch: Some("x".into()), ..domain() });
        routes.hosts.insert("theirs.example.com".into(), "staging".into());
        assert!(decide(&p, &routes)["staging"].delete);
    }

    #[test]
    fn building_and_the_ten_newest_failed_stay() {
        let mut list = old("err", Target::Preview, Status::Error, 12);
        list.extend(old("pv", Target::Preview, Status::Ready, 12).into_iter().map(|mut d| {
            d.created = at(1);
            d
        }));
        // Newer than every failed one, so the failed aren't among the 10 newest.
        list.push(summary("b", Target::Preview, Status::Building, 0));
        let p = project(list);
        let d = decide(&p, &Routes::default());
        for i in 0..10 {
            assert!(d[&format!("err{i:02}")].kept_by.contains(&Exception::NewestFailed));
        }
        assert!(d["err10"].delete && d["err11"].delete);
        assert_eq!(d["b"].class, Class::Building);
        assert!(!d["b"].delete);
        assert!(d["b"].kept_by.contains(&Exception::Building));
    }

    #[test]
    fn periods_are_per_state() {
        let mut list = vec![
            summary("c", Target::Preview, Status::Canceled, 8),
            summary("e", Target::Preview, Status::Error, 8),
            summary("v", Target::Preview, Status::Ready, 8),
            summary("r", Target::Production, Status::Ready, 8),
        ];
        // Push them out of every count.
        list.extend((0..25).map(|i| summary(&format!("fill-c{i:02}"), Target::Preview, Status::Canceled, 1)));
        list.extend((0..25).map(|i| summary(&format!("fill-v{i:02}"), Target::Preview, Status::Ready, 1)));
        list.extend((0..25).map(|i| summary(&format!("fill-r{i:02}"), Target::Production, Status::Ready, 1)));
        list.extend((0..25).map(|i| summary(&format!("fill-e{i:02}"), Target::Preview, Status::Error, 1)));
        let p = project(list);
        let routes = Routes::default();
        let week = |c, e, v, r| Policy { canceled: c, errored: e, preview: v, production: r }.periods();
        let gone = |periods: Periods| -> BTreeSet<String> {
            plan(&p, &Facts { routes: &routes, base_domain: BASE }, &periods, NOW).into_iter().filter(|d| d.delete && d.id.len() == 1).map(|d| d.id).collect()
        };
        let w = Some(Period::Days(7));
        assert_eq!(gone(week(w, None, None, None)), ["c".to_string()].into());
        assert_eq!(gone(week(None, w, None, None)), ["e".to_string()].into());
        assert_eq!(gone(week(None, None, w, None)), ["v".to_string()].into());
        assert_eq!(gone(week(None, None, None, w)), ["r".to_string()].into());
        assert!(gone(week(None, None, None, None)).is_empty());
        let u = Some(Period::Unlimited);
        assert!(gone(week(u, u, u, u)).is_empty());
    }

    #[test]
    fn a_restored_deployment_counts_from_its_restore() {
        let mut list = old("pv", Target::Preview, Status::Ready, 25);
        list.push(summary("back", Target::Preview, Status::Ready, 400));
        let mut p = project(list);
        assert!(decide(&p, &Routes::default())["back"].delete);
        p.restored.insert("back".into(), at(2));
        let d = decide(&p, &Routes::default());
        assert!(!d["back"].delete);
        assert_eq!(d["back"].expires, Some(rfc3339(NOW - 2 * DAY_S + 30 * DAY_S)));
    }

    /// Whatever the fixed counts kept (deploy.rs `retained`, plus branch hosts) the default policy
    /// keeps, at any age: releasing it deletes nothing they kept.
    #[test]
    fn keeps_everything_the_fixed_counts_kept() {
        let mut seed = 7u64;
        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for _ in 0..200 {
            let mut list = Vec::new();
            for i in 0..(10 + next(70)) {
                let target = if next(3) == 0 { Target::Production } else { Target::Preview };
                let status = match next(10) {
                    0 => Status::Error,
                    1 => Status::Canceled,
                    _ => Status::Ready,
                };
                list.push(summary(&format!("d{i:03}"), target, status, 31 + next(900)));
            }
            let mut p = project(list);
            let ready_prod: Vec<String> = p.deployments.iter().filter(|d| d.status == Status::Ready && d.target == Target::Production).map(|d| d.id.clone()).collect();
            if let Some(first) = ready_prod.first() {
                p.production = Some(first.clone());
            }
            let mut history: Vec<String> = p.deployments.iter().filter(|d| d.status == Status::Ready).map(|d| d.id.clone()).collect();
            // Shuffled, as rollbacks and promotes leave it.
            for i in (1..history.len()).rev() {
                history.swap(i, next(i as u64 + 1) as usize);
            }
            history.truncate(next(12) as usize);
            if let Some(prod) = &p.production {
                history.retain(|h| h != prod);
                history.insert(0, prod.clone());
            }
            p.production_history = history;
            let mut routes = Routes::default();
            for d in p.deployments.iter().filter(|_| next(8) == 0) {
                routes.hosts.insert(format!("p-git-b{}.forkrail.test", d.id), d.id.clone());
            }
            let mut legacy = crate::deploy::retained(&p);
            legacy.extend(routes.hosts.iter().filter(|(h, _)| h.contains(BRANCH_INFIX)).map(|(_, id)| id.clone()));
            let gone = doomed(&p, &routes);
            for id in &legacy {
                assert!(!gone.contains(id), "{id} was kept by the fixed counts and would be deleted");
            }
        }
    }

    #[test]
    fn recovery_periods_follow_age_and_policy() {
        let day = DAY_S;
        // 30 days old or more: 30 days, however it was deleted.
        assert_eq!(recovery_s(45 * day, None), 30 * day);
        assert_eq!(recovery_s(45 * day, Some(Period::Days(7))), 30 * day);
        // Younger, by a policy shorter than 30 days: the policy's period.
        assert_eq!(recovery_s(9 * day, Some(Period::Days(7))), 7 * day);
        // By hand: its age, at least a day.
        assert_eq!(recovery_s(3 * day, None), 3 * day);
        assert_eq!(recovery_s(3_600, None), day);
    }

    #[test]
    fn ops_say_what_they_change() {
        let get: RetentionOp = serde_json::from_value(json!({"op": "get", "project": "p"})).unwrap();
        assert!(!get.changes());
        let plan: RetentionOp = serde_json::from_value(json!({"op": "plan", "change": {"preview": 7}})).unwrap();
        assert!(!plan.changes() && !plan.install_wide());
        let dry: RetentionOp = serde_json::from_value(json!({"op": "sweep", "dry_run": true})).unwrap();
        assert!(!dry.changes());
        let sweep: RetentionOp = serde_json::from_value(json!({"op": "sweep"})).unwrap();
        assert!(sweep.changes() && sweep.install_wide());
        let set: RetentionOp = serde_json::from_value(json!({"op": "set", "project": "p", "change": {"production": "1y"}})).unwrap();
        assert!(set.changes() && !set.install_wide());
        let install: RetentionOp = serde_json::from_value(json!({"op": "set", "change": {"production": null}})).unwrap();
        assert!(install.install_wide());
        let restore: RetentionOp = serde_json::from_value(json!({"op": "restore", "id": "abcd1234"})).unwrap();
        assert!(restore.changes());
        assert!(serde_json::from_value::<RetentionOp>(json!({"op": "set", "change": {"preview": 0}})).is_err());
    }

    #[test]
    fn a_trashed_deployment_reads_back() {
        let t = Trashed {
            id: "abcd1234".into(),
            target: Target::Preview,
            created: at(40),
            status: Status::Ready,
            version: "7".into(),
            deleted_at: at(1),
            until: rfc3339(NOW + 29 * DAY_S),
            by: Some(BY_POLICY.into()),
            reason: "older than the policy keeps (preview 30 days)".into(),
            route: None,
            hosts: vec!["p-abcd1234.forkrail.test".into()],
        };
        let mut p = Project { name: "p".into(), ..Default::default() };
        p.deleted.push(t.clone());
        let text = serde_json::to_string(&p).unwrap();
        let back: Project = serde_json::from_str(&text).unwrap();
        assert_eq!(back.deleted, vec![t]);
        // A project without any of it writes none of the new fields.
        let plain = serde_json::to_value(Project { name: "q".into(), ..Default::default() }).unwrap();
        for key in ["deleted", "retention", "restored", "auto_assign_domains"] {
            assert!(plain.get(key).is_none(), "{key}");
        }
    }

    #[test]
    fn a_view_marks_a_proposal() {
        let p = project(old("pv", Target::Preview, Status::Ready, 25));
        let routes = Routes::default();
        let saved = view_of(&p, &Policy::default(), &routes, BASE, None, NOW);
        assert_eq!(saved.doomed().len(), 5);
        let change = PolicyChange { preview: Some(Some(Period::Unlimited)), ..Default::default() };
        let proposed = view_of(&p, &Policy::default(), &routes, BASE, Some(&change), NOW);
        assert!(proposed.proposed);
        assert!(proposed.doomed().is_empty());
    }
}
