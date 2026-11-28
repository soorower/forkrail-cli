//! State in S3, under `state/` in the artifact bucket:
//! - `routes.json`: the routing table, the only file the router reads
//! - `projects/<name>.json`: production, its history, every deployment, the names of its env vars
//!   (their values are in SSM: secrets.rs)
//! - `deployments/<id>.json`: the full record of one deployment
//! - `settings.json`: the base domain, and the regions deployments are also published in
//!
//! Every write is conditional (`If-Match` on the version read, or `If-None-Match: *` to create),
//! so two CLIs running at once can't lose each other's changes: the loser reads again and retries.
//!
//! Project files and deployment records move to the install's table (table.rs) when
//! `state/store.json` says so; `Store` sends each read and write where it belongs, so nothing that
//! uses it changes. Everything else stays in the bucket, where the routers read it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use forkrail_common::install::{ROUTES_KEY, SETTINGS_KEY, deployment_key, project_key};
use forkrail_common::install::{STORE_KEY, state_key, table_item, table_kind};

use crate::table::{Items, Mode, STORE_TTL, StoreConfig};

/// Until there's a real domain, `.test`: reserved, so nothing resolves by accident. Requests name
/// a deployment with the `Host` header.
pub const DEFAULT_BASE_DOMAIN: &str = "forkrail.test";
const MAX_TRIES: u32 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_base_domain")]
    pub base_domain: String,
    /// Regions besides the home region (the CLI's `--region`, where this state lives) with a router
    /// of their own (`forkrail regions`): every deployment's static files and routing rules go there
    /// too, for that router to serve. Where a project's functions run is the project's own choice
    /// (`Project::regions`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
    /// The dashboard's address (`https://app.example.com`), where builds send their log lines as
    /// they run (`forkrail dashboard`). Without it a build's log is kept only when it ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<String>,
    /// The install's public IPv4 addresses (its accelerator's), which a registered domain's A
    /// records name. Without them, what the base domain's wildcard resolves to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    /// Regions where this AWS account lets a function have less memory than forkrail asked for,
    /// with that limit in MB: new accounts start low (512 MB), and AWS raises it as the account
    /// gets used. Functions there get the limit (lambda.rs `fit`); the dashboard says so.
    /// Written by the installer and by publishing, which takes a region off once it allows more.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub memory_limits: BTreeMap<String, u32>,
}

fn default_base_domain() -> String {
    DEFAULT_BASE_DOMAIN.to_owned()
}

impl Default for Settings {
    fn default() -> Self {
        Settings { base_domain: default_base_domain(), regions: Vec::new(), dashboard: None, addresses: Vec::new(), memory_limits: BTreeMap::new() }
    }
}

impl Settings {
    /// A host's URL: `https://`, except under `.test`, which resolves nowhere and has no
    /// certificate.
    pub fn url(&self, host: &str) -> String {
        let scheme = if self.base_domain == "test" || self.base_domain.ends_with(".test") { "http" } else { "https" };
        format!("{scheme}://{host}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Production,
    Preview,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::Production => "production",
            Target::Preview => "preview",
        }
    }
}

/// Where an env var applies: a deployment's target, or Development, which no deployment runs
/// with: `forkrail env pull` writes its values into a local file, for running the app here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvTarget {
    Production,
    Preview,
    Development,
}

impl EnvTarget {
    pub const ALL: [EnvTarget; 3] = [EnvTarget::Production, EnvTarget::Preview, EnvTarget::Development];

    pub fn as_str(self) -> &'static str {
        match self {
            EnvTarget::Production => "production",
            EnvTarget::Preview => "preview",
            EnvTarget::Development => "development",
        }
    }

    /// Whether its values can be read back once set. Development's can (they're for a laptop's
    /// `.env.local`, readable like Vercel's Config variables); production's and previews' never
    /// leave SSM except into a deployment.
    pub fn readable(self) -> bool {
        self == EnvTarget::Development
    }

    /// The deployments it applies to; none for Development.
    pub fn deployed(self) -> Option<Target> {
        match self {
            EnvTarget::Production => Some(Target::Production),
            EnvTarget::Preview => Some(Target::Preview),
            EnvTarget::Development => None,
        }
    }
}

impl From<Target> for EnvTarget {
    fn from(target: Target) -> Self {
        match target {
            Target::Production => EnvTarget::Production,
            Target::Preview => EnvTarget::Preview,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    /// The project's id: its first name, kept for good. Its addresses, functions, stored files,
    /// env values and log rows are all under it, whatever the project is called now.
    pub name: String,
    /// What the project is called now, if it was renamed (project.rs `rename`): shown in the
    /// dashboard and the terminal, and accepted wherever a project is named. Unset, it's `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub function: String,
    pub created: String,
    /// The deployment `<project>.<base>` points at.
    #[serde(default)]
    pub production: Option<String>,
    /// Deployments that have been production, most recent first; the first is production now.
    /// Rollback goes to the second.
    #[serde(default)]
    pub production_history: Vec<String>,
    /// Every deployment still kept, newest first.
    #[serde(default)]
    pub deployments: Vec<Summary>,
    /// The names of its env vars, per target. The values are in SSM (secrets.rs); changes take
    /// effect on the next deploy.
    #[serde(default)]
    pub env_names: EnvNames,
    /// Where values were kept before they moved to SSM. Read only to move them (`forkrail env
    /// migrate`); written as a notice an older forkrail can't parse, so that it refuses this project
    /// rather than deploy it without its env vars.
    #[serde(default, rename = "env")]
    pub legacy_env: LegacyEnv,
    /// Workers the router may run at once for each of the project's deployments; the router's
    /// default if unset. With concurrent compute off that's how many requests run at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_workers: Option<u32>,
    /// The IAM role its functions run as, by name or ARN; the shared `forkrail-worker` if unset.
    /// For apps that call AWS themselves, without keys in their env vars. Takes effect on the
    /// next deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Private targets its apps reach through the router (see the router's `FORKRAIL_EGRESS_TARGETS`).
    /// Takes effect on the next deploy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<String>,
    /// Function settings for its next deployments, as Vercel's project settings are: `vercel.json`
    /// wins over them, and deploy's flags over both. Unset, a deploy uses forkrail's defaults and a
    /// redeploy what the deployment it repeats had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<u64>,
    /// Concurrent compute. Kept under `fluid`, vercel.json's key for it, as every forkrail and
    /// dashboard reads it: one from before the name would drop a key it doesn't know.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "fluid")]
    pub concurrent: Option<bool>,
    /// With concurrent compute on, the most requests one worker takes at once (`MAX_INFLIGHT`);
    /// unset, the router's default (`FORKRAIL_MAX_INFLIGHT`, 10). Unlike the settings above it
    /// applies at once: it's written to its deployments' entries in the routing table too, those
    /// that run concurrent compute (one request per worker stays one), and the router replaces
    /// their pools within seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inflight: Option<u32>,
    /// The AWS regions its functions run in (`forkrail_common::regions`), as Vercel's Function
    /// Regions setting: `vercel.json`'s `regions` wins over it. Empty is the default, the home
    /// region alone. Takes effect on the next deploy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<String>,
    /// The GitHub repository its pushes deploy from (connect.rs), if one is connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitLink>,
    /// Its framework preset, a Vercel slug (`flask`, `express`, `vite`), as Vercel's project
    /// setting: `vercel.json`'s `framework` wins over it; unset, it's found from the project's files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
    /// Schedules turned off, and what was last written to EventBridge Scheduler (`schedules.rs`):
    /// kept across deploys, promotes and rollbacks.
    #[serde(default, skip_serializing_if = "ScheduleStates::is_empty")]
    pub schedules: ScheduleStates,
    /// Secret URLs that build a branch's newest commit of the connected repository when called
    /// (deploy_hooks.rs). Only each secret's SHA-256 is kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deploy_hooks: Vec<DeployHook>,
    /// "Auto-assign custom domains" (Vercel's `autoAssignCustomDomains`): on unless set to false.
    /// Off, every production deployment waits for `promote`, as one published with --skip-domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_assign_domains: Option<bool>,
    /// How long its deployments are kept, over the install's default (retention.rs).
    #[serde(default, skip_serializing_if = "crate::retention::Policy::is_empty")]
    pub retention: crate::retention::Policy,
    /// Deployments deleted and still restorable, with everything of them kept until their time is
    /// up (retention.rs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<crate::retention::Trashed>,
    /// Deployments restored from `deleted`, and when: the policy counts their age from then.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub restored: BTreeMap<String, String>,
    /// Its builds run on the large builder (`forkrail-builder-large`): one of them ran out of the
    /// builder's memory and was built again there (remote.rs `again_large`). Kept, so its next
    /// builds don't run out first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub large_builds: Option<LargeBuilds>,
}

/// Since when, and from which deployment's build, a project builds on the large builder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LargeBuilds {
    /// RFC 3339.
    pub since: String,
    /// The deployment whose build ran out of the builder's memory.
    pub deployment: String,
}

impl Project {
    /// What it's called: its display name, else its id.
    pub fn shown_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// Whether its production deployments go live as they're published (else they wait for
    /// `promote`).
    pub fn auto_assigns_domains(&self) -> bool {
        self.auto_assign_domains != Some(false)
    }
}

/// A project's deploy hook: its URL (`<dashboard>/api/deploy-hooks/<project>/<secret>`) builds
/// `branch` at its newest commit. The secret is shown once, when it's made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployHook {
    pub id: String,
    pub name: String,
    pub branch: String,
    /// SHA-256 of the secret in the URL, in hex.
    pub secret_sha256: String,
    pub created: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// The deploy hook a deployment was made by, as it was named then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRef {
    pub id: String,
    pub name: String,
}

/// A project's connected GitHub repository: every push to it makes a deployment, the production
/// branch's as production and any other branch's as a preview, as on Vercel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLink {
    /// GitHub's id for the repository, which a rename or a transfer doesn't change.
    pub repo_id: u64,
    /// `owner/name`, as it was when connected or last pushed to.
    pub repo: String,
    /// The installation of the install's GitHub App that reaches it.
    pub installation_id: u64,
    pub production_branch: String,
    /// The folder of the repository that holds the project, when that isn't its top.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default)]
    pub private: bool,
    pub connected_at: String,
    /// Who connected it: their GitHub username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_by: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvNames {
    #[serde(default)]
    pub production: BTreeSet<String>,
    #[serde(default)]
    pub preview: BTreeSet<String>,
    /// Left out while empty, so project files without any stay as they were.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub development: BTreeSet<String>,
    /// Previews' values for one Git branch, by branch: each wins over preview's value of the same
    /// name for that branch's previews, as on Vercel. Left out while empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, BTreeSet<String>>,
}

impl EnvNames {
    pub fn get(&self, target: impl Into<EnvTarget>) -> &BTreeSet<String> {
        match target.into() {
            EnvTarget::Production => &self.production,
            EnvTarget::Preview => &self.preview,
            EnvTarget::Development => &self.development,
        }
    }

    pub fn get_mut(&mut self, target: impl Into<EnvTarget>) -> &mut BTreeSet<String> {
        match target.into() {
            EnvTarget::Production => &mut self.production,
            EnvTarget::Preview => &mut self.preview,
            EnvTarget::Development => &mut self.development,
        }
    }

    /// A branch's own preview names; none if it has none.
    pub fn branch(&self, branch: &str) -> &BTreeSet<String> {
        static NONE: BTreeSet<String> = BTreeSet::new();
        self.branches.get(branch).unwrap_or(&NONE)
    }

    /// Add `names` to a branch's own.
    pub fn add_to_branch(&mut self, branch: &str, names: impl IntoIterator<Item = String>) {
        self.branches.entry(branch.to_owned()).or_default().extend(names);
    }

    /// Take `name` from a branch's own, and the branch with it once it has none; whether it was there.
    pub fn remove_from_branch(&mut self, branch: &str, name: &str) -> bool {
        let Some(names) = self.branches.get_mut(branch) else { return false };
        let removed = names.remove(name);
        if names.is_empty() {
            self.branches.remove(branch);
        }
        removed
    }

    /// The names a deployment of `target` gets, for a preview of `branch` its own among them.
    pub fn deployed(&self, target: Target, branch: Option<&str>) -> BTreeSet<String> {
        let mut names = self.get(target).clone();
        if let Some(branch) = branch.filter(|_| target == Target::Preview) {
            names.extend(self.branch(branch).iter().cloned());
        }
        names
    }
}

/// What a project file's `env` says once the values have moved.
pub const ENV_MOVED: &str = "env var values are in SSM Parameter Store now: update the forkrail CLI";

/// A project file's old `env`: the values themselves, until `forkrail env migrate` moves them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LegacyEnv {
    #[default]
    Moved,
    Values { production: BTreeMap<String, String>, preview: BTreeMap<String, String> },
}

impl LegacyEnv {
    pub fn has_values(&self) -> bool {
        matches!(self, LegacyEnv::Values { .. })
    }
}

impl Serialize for LegacyEnv {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            LegacyEnv::Moved => serializer.serialize_str(ENV_MOVED),
            // Kept as they were until moved: a write before the move mustn't lose them.
            LegacyEnv::Values { production, preview } => Values { production: production.clone(), preview: preview.clone() }.serialize(serializer),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Values {
    #[serde(default)]
    production: BTreeMap<String, String>,
    #[serde(default)]
    preview: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for LegacyEnv {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            // Only a string matches: a malformed map fails rather than passing for the notice.
            Notice(#[allow(dead_code)] String),
            Values(Values),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Values(v) if !v.production.is_empty() || !v.preview.is_empty() => {
                LegacyEnv::Values { production: v.production, preview: v.preview }
            }
            Raw::Notice(_) | Raw::Values(_) => LegacyEnv::Moved,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub target: Target,
    pub created: String,
    /// The Lambda version. Each deploy publishes its own, but nothing relies on that: pruning
    /// deletes a version only when no kept deployment and no route uses it. Empty until published.
    pub version: String,
    /// Deployments listed before statuses existed were all published.
    #[serde(default)]
    pub status: Status,
}

/// Where a deployment is, as Vercel names its states. It's recorded as `building` before anything
/// runs, and ends `ready` (published and routed), `error` or `canceled`. `queued` is for builds
/// that wait for a builder (none yet).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Building,
    #[default]
    Ready,
    Error,
    Canceled,
}

/// Builds stop after this long, as Vercel's do.
pub const BUILD_TIMEOUT_S: u64 = 45 * 60;
/// A deployment still `building` this long after it started stopped reporting (its CLI was killed,
/// the machine slept): it reads as `error`.
pub const STALE_AFTER_S: u64 = BUILD_TIMEOUT_S + 5 * 60;

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Building => "building",
            Status::Ready => "ready",
            Status::Error => "error",
            Status::Canceled => "canceled",
        }
    }

    /// Still being made.
    pub fn in_progress(self) -> bool {
        matches!(self, Status::Queued | Status::Building)
    }

    /// The status as it reads at `now` (Unix seconds) for a deployment that started at `since`:
    /// one in progress for longer than any build may run has failed.
    pub fn at(self, since: &str, now: u64) -> Status {
        match parse_rfc3339(since) {
            Some(started) if self.in_progress() && now.saturating_sub(started) > STALE_AFTER_S => Status::Error,
            _ => self,
        }
    }
}

/// Why a deployment ended in `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// The step it failed in (`Building`), when it got that far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    pub message: String,
}

/// What a stale deployment's record says went wrong.
pub const STOPPED_REPORTING: &str = "the build stopped reporting before it finished (was the deploy interrupted?)";

/// How a deployment was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// `forkrail deploy`, `redeploy` or `promote` in a terminal.
    Cli,
    /// A button in the dashboard, through the control function.
    Dashboard,
    /// A push to the project's connected repository.
    Git,
    /// One of the project's deploy hooks (deploy_hooks.rs): its secret URL was called.
    Hook,
    /// A way this forkrail doesn't know: a newer one made the deployment. Read, so that an older
    /// builder or CLI still reads the record; never written by this one.
    #[serde(other)]
    Other,
}

impl Via {
    /// Made from a commit of the project's connected repository: a push, or a deploy hook building
    /// a branch's newest commit. GitHub hears how both end, and both keep their branch's host.
    pub fn from_repository(self) -> bool {
        matches!(self, Via::Git | Via::Hook)
    }
}

/// A deployment's build log: kept in ClickHouse as it runs (sent through the dashboard) and, once
/// it ends, as one file in S3.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogInfo {
    /// The SHA-256 of the key the build sends its lines with. The key itself is never stored.
    pub key_sha256: String,
    /// Lines and bytes kept, once it ends.
    #[serde(default)]
    pub lines: u64,
    #[serde(default)]
    pub bytes: u64,
    /// Lines past Vercel's 4 MB were dropped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
    /// The whole log in S3, gzipped JSON lines, once it ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
}

/// Everything about one deployment. Env values are never stored here, only their names. A
/// deployment still building (or that failed) has only what was known when it started: no version,
/// package or runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub id: String,
    pub project: String,
    pub target: Target,
    pub created: String,
    /// Deployments recorded before statuses existed were all published.
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Failure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    /// When its build (or, for a redeploy, its publish) started and when it ended, to the
    /// millisecond.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub building_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<LogInfo>,
    #[serde(default)]
    pub function: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub entry: String,
    #[serde(default)]
    pub memory_mb: u32,
    #[serde(default)]
    pub max_duration_s: u64,
    /// Many requests share a worker (concurrent compute), or each gets one to itself (`"fluid":
    /// false`). Kept under `fluid`, vercel.json's key for it, as the project's setting is.
    /// Deployments made before this existed were all concurrent.
    #[serde(default = "concurrent_default", rename = "fluid")]
    pub concurrent: bool,
    /// Hash of the project's files and paths, and of the package built from them.
    #[serde(default)]
    pub source_sha256: String,
    #[serde(default)]
    pub package_sha256: String,
    #[serde(default)]
    pub package_key: String,
    #[serde(default)]
    pub package_bytes: u64,
    /// Whether its package's runtime opens sealed env files, for publishing it again with env vars
    /// past Lambda's 4 KB.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reads_sealed_env: bool,
    /// Whether its package's runtime hands the app the project's own AWS credentials.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub app_aws_keys: bool,
    /// Whether its app was given the system variables (system_env.rs): a preview's say `preview`,
    /// so promoting it publishes its build again for production, as Vercel builds it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub system_env: bool,
    #[serde(default)]
    pub env_keys: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    /// The `vercel.json` it was built from.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// The region `version` was published in; the home region for records from before regions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// The same deployment in other regions: region → version of `function` there.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub replicas: BTreeMap<String, String>,
    /// The framework that built it (`TanStack Start`), or that the API is written with (`FastAPI`),
    /// if forkrail knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
    /// Who deployed it: their git `user.name`, else the OS user's name. Never an email address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployer: Option<String>,
    /// The commit the project folder was at, if it's a checkout of a GitHub repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitCommit>,
    /// Its static files' manifest, how many files, and their total size.
    #[serde(default, rename = "static", skip_serializing_if = "Option::is_none")]
    pub static_files: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_bytes: Option<u64>,
    /// Its routing program (`vercel.json`'s redirects, rewrites, headers...), by id:
    /// `static/rules/<id>.json` in each region's bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<String>,
    /// Its Image Optimization settings, by id: `static/images/<id>.json` in each region's bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<String>,
    /// The IAM role and egress targets it was deployed with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<String>,
    /// The deployment whose build this one published again, with the env vars and settings of
    /// the day (a redeploy, or a preview promoted with production's env vars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redeploy_of: Option<String>,
    /// The project's own files, kept for the dashboard at `sources/<project>/<sha256>.zip`.
    #[serde(default, rename = "source", skip_serializing_if = "Option::is_none")]
    pub kept_source: Option<crate::source::Archive>,
    /// The function it was built in (`forkrail-builder`), for a remote build; none when it was built
    /// where it was deployed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder: Option<String>,
    /// The source a remote build was made from, at `uploads/<project>/<sha256>.zip`: what a rebuild
    /// starts from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<crate::source::Archive>,
    /// GitHub's own record of it, for one a push made (connect.rs): the id of the deployment
    /// GitHub shows on the commit and its pull request, told how this one ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_deployment: Option<u64>,
    /// What the project's own files fixed for its function, worked out when it was built. None on
    /// records from before this was kept: `pinned_settings` reads their `config` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<Pinned>,
    /// Which importer read the project into forkrail's build (`build::IMPORTER`). None on records
    /// from before it was kept, all of which `vercel.json`'s importer made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importer: Option<String>,
    /// Its schedules (`crons` and `schedules`), run while it's production.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<forkrail_common::schedules::Schedule>,
    /// The deploy hook that made it, for `via: hook`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy_hook: Option<HookRef>,
    /// Its functions in `api/`, by file (Vercel's zero-config functions), when that's what it runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<String>,
    /// Built with skew protection on (`build::Build::skew_protection`): publishing its build again
    /// tells its app so again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skew_protection: bool,
    /// The name its framework gave it (`build::Build::skew_id`), for the routing table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skew_id: Option<String>,
    /// What it runs, for people to read (`build::Build::outline`). None on records from before it
    /// was kept, and for deployments with none of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<Outline>,
    /// A Next.js build: its version and manifest's hash, whether its caches are the shared
    /// cache, and its seed (`build::NextBuild`). Kept so a redeploy publishes it as it was built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<crate::build::NextBuild>,
    /// A Next.js deployment's entry in the routing table (`build::Build::next_router`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_router: Option<forkrail_common::routes::NextRef>,
}

/// What a deployment runs, as its build found it, for people to read (the dashboard's deployment
/// page): a framework's functions with the paths they answer at and their settings, its
/// prerendered pages (ISR), its Routing Middleware, its image sizes. Names and settings only;
/// forkrail itself never reads it back (the routing program and the package are what run).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outline {
    /// A Build Output's functions, each real one once, by path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<FunctionOutline>,
    /// Its prerenders (Vercel's ISR), by their function's path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prerenders: Vec<PrerenderOutline>,
    /// Its Routing Middleware: the framework's, and the project's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub middleware: Vec<MiddlewareOutline>,
    /// Image Optimization's widths and formats, when it has `images`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImagesOutline>,
}

impl Outline {
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty() && self.prerenders.is_empty() && self.middleware.is_empty() && self.images.is_none()
    }
}

/// One function of a Build Output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionOutline {
    /// `/api/hello`, `/![-]/catchall`, `/_render`.
    pub path: String,
    /// The other names that are links to it (SvelteKit gives each route its own), sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
    /// As its `.vc-config.json` says: `nodejs24.x`, `edge` (run on Node).
    pub runtime: String,
    /// What it asks for; None takes the deployment's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    /// Its own limit in seconds, as the router holds it; None takes the deployment's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<u64>,
    /// Its `.vc-config.json` says it streams its responses.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub streams: bool,
}

/// One prerender (Vercel's ISR).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrerenderOutline {
    /// Its function's path (`/blog/[slug]`).
    pub path: String,
    /// Seconds a page stays fresh once made; None until it's revalidated on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiration_s: Option<u64>,
    /// Prerenders of one group are revalidated together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u64>,
    /// It has a page from the build, served until one is made.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    /// The query parameters that make pages apart: None all of them, empty none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_query: Option<Vec<String>>,
}

/// One Routing Middleware.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MiddlewareOutline {
    /// Its function, as the router names it (`/_middleware`, `/middleware`).
    pub function: String,
    /// The project's file it was built from (`middleware.ts`); None for a framework's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// The paths it runs on, as written (`/api/:path*`, or a framework's pattern); None every path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<Vec<String>>,
    /// Written for the Edge runtime; run on Node all the same.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edge: bool,
}

/// Image Optimization's widths and output formats.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagesOutline {
    pub sizes: Vec<u32>,
    pub formats: Vec<String>,
}

/// A project's schedules' states, apart from any deployment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleStates {
    /// "Disable all": none runs on its own (Run now still does).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Keys turned off one by one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inactive: Vec<String>,
    /// Key → what was last written to Scheduler for it, so a sync rewrites only what changed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub synced: BTreeMap<String, String>,
}

impl ScheduleStates {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether `key` runs on its own.
    pub fn active(&self, key: &str) -> bool {
        !self.disabled && !self.inactive.iter().any(|k| k == key)
    }
}

/// Function settings a deployment's own files fixed (`vercel.json`'s `functions` and `fluid`), which
/// win over the project's settings when it's redeployed. A field is None where the files say
/// nothing, so the project's setting (or the default) applies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pinned {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<u64>,
    /// Concurrent compute, kept under vercel.json's key for it.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "fluid")]
    pub concurrent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<Floor>,
    /// `regions`: the AWS regions its functions run in, by AWS's names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regions: Option<Vec<String>>,
}

/// The least memory and maxDuration a deployment of per-file functions needs when `functions` sets
/// them for some of its functions and not others: the largest any asks for. They share one Lambda
/// function, which gets at least this, and otherwise the project's settings (the functions that
/// set nothing have those, as on Vercel; the router holds each that does to its own maxDuration).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Floor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<u64>,
}

fn concurrent_default() -> bool {
    true
}

impl DeploymentRecord {
    /// What its files fixed: as recorded when it was built, or, for a record from before that was
    /// kept, read from the `vercel.json` it keeps.
    pub fn pinned_settings(&self) -> Pinned {
        self.pinned.clone().unwrap_or_else(|| {
            let (memory_mb, max_duration_s, concurrent) = crate::vercel::settings_of(self.config.as_ref());
            let regions = crate::vercel::regions(self.config.as_ref()).ok().flatten();
            Pinned { memory_mb, max_duration_s, concurrent, floor: None, regions }
        })
    }

    /// Whether it has a function: a static site doesn't.
    pub fn has_function(&self) -> bool {
        !self.function.is_empty()
    }

    /// Whether its build holds what its environment was as it built: a Next.js build inlines
    /// `NEXT_PUBLIC_*` into its client code and renders its prerendered pages with every variable
    /// it reads, so only building it again changes them (records from before `next` was kept
    /// say so by their framework).
    pub fn built_with_env(&self) -> bool {
        self.next.is_some() || self.framework.as_deref() == Some(crate::next::NAME)
    }

    /// Its status now: one left `building` past any build's time limit has failed.
    pub fn status_now(&self) -> Status {
        self.status.at(self.building_at.as_deref().unwrap_or(&self.created), now_secs())
    }

    /// Promote, roll back and redeploy need a published deployment.
    pub fn check_ready(&self) -> anyhow::Result<()> {
        match self.status_now() {
            Status::Ready => Ok(()),
            Status::Queued | Status::Building => bail!("deployment {} is still building", self.id),
            status => bail!("deployment {} is {}: it was never published", self.id, status.as_str()),
        }
    }
}

/// The commit a deployment was made from: HEAD of the project folder's checkout at deploy time.
/// Files changed since that commit are deployed too, and nothing here says so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommit {
    /// `owner/name` on GitHub, from the `origin` remote.
    pub repo: String,
    /// The branch checked out; `HEAD` when none was, as git itself calls that.
    pub branch: String,
    /// The commit's abbreviated hash.
    pub sha: String,
    /// The first line of its message.
    pub message: String,
    /// The whole hash, which builds get as VERCEL_GIT_COMMIT_SHA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_sha: Option<String>,
    /// Who wrote the commit, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

/// The Git branch whose own preview values (`EnvNames::branches`) a deployment of `target` made
/// from `git` gets: a preview's branch, never production's, and none when a commit was checked out
/// rather than a branch (`HEAD`).
pub fn env_branch(target: Target, git: Option<&GitCommit>) -> Option<&str> {
    git.map(|g| g.branch.as_str()).filter(|b| target == Target::Preview && !b.is_empty() && *b != "HEAD")
}

#[derive(Clone)]
pub struct Store {
    pub s3: aws_sdk_s3::Client,
    pub bucket: String,
    /// The install's table, which holds project files and deployment records once
    /// `state/store.json` says so (table.rs). None for a store of another region's bucket.
    pub table: Option<Arc<Table>>,
}

/// The table's items, and what `state/store.json` last said.
pub struct Table {
    pub items: Items,
    config: tokio::sync::Mutex<Option<(Instant, StoreConfig)>>,
    /// Tests fix the config instead of reading it from the bucket.
    pinned: bool,
}

impl Table {
    pub fn new(client: aws_sdk_dynamodb::Client) -> Self {
        Table { items: Items::Dynamo(client), config: tokio::sync::Mutex::new(None), pinned: false }
    }

    /// The table in memory, as `config` says it's used, for tests.
    pub fn memory(config: StoreConfig) -> Self {
        Self::pinned(Items::memory(), config)
    }

    /// `items` used as `config` says, whatever `state/store.json` says: for tests.
    pub fn pinned(items: Items, config: StoreConfig) -> Self {
        Table { items, config: tokio::sync::Mutex::new(Some((Instant::now(), config))), pinned: true }
    }
}

/// Where one state file is read and written.
enum Place {
    /// The bucket only: every file but project files and deployment records, and those too until
    /// the install moves them.
    Bucket,
    /// A project file or deployment record, while the bucket is still the one read (`mirror`), or
    /// once the table is (`dynamodb`).
    Table { kind: &'static str, pk: String, config: StoreConfig },
}

impl Store {
    pub fn new(s3: aws_sdk_s3::Client, bucket: String, dynamodb: Option<aws_sdk_dynamodb::Client>) -> Self {
        Store { s3, bucket, table: dynamodb.map(|client| Arc::new(Table::new(client))) }
    }

    /// Where project files and deployment records are kept: `state/store.json`, read again once
    /// it's [`STORE_TTL`] old, so a switch reaches every running process within that.
    pub async fn store_config(&self) -> Result<StoreConfig> {
        let Some(table) = &self.table else { return Ok(StoreConfig::default()) };
        let mut kept = table.config.lock().await;
        if let Some((at, config)) = kept.as_ref()
            && (table.pinned || at.elapsed() < STORE_TTL)
        {
            return Ok(config.clone());
        }
        let config = self.bucket_get::<StoreConfig>(STORE_KEY).await?.map(|(c, _)| c).unwrap_or_default();
        *kept = Some((Instant::now(), config.clone()));
        Ok(config)
    }

    async fn place(&self, key: &str) -> Result<Place> {
        let (Some(_), Some((kind, pk))) = (&self.table, table_item(key)) else { return Ok(Place::Bucket) };
        let config = self.store_config().await?;
        Ok(match config.mode {
            Mode::S3 => Place::Bucket,
            Mode::Mirror | Mode::Dynamodb => Place::Table { kind, pk, config },
        })
    }

    fn items(&self) -> &Items {
        &self.table.as_ref().expect("a table place has a table").items
    }

    /// The object and its ETag, if it exists. In the table, the ETag is the item's version.
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<(T, String)>> {
        match self.place(key).await? {
            Place::Table { pk, config, .. } if config.mode == Mode::Dynamodb => match self.items().get(&config.table, &pk).await? {
                Some(item) => {
                    let value = serde_json::from_str(&item.doc).with_context(|| format!("parsing {pk} from {}", config.table))?;
                    Ok(Some((value, item.v.to_string())))
                }
                None => Ok(None),
            },
            _ => self.bucket_get(key).await,
        }
    }

    async fn bucket_get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<(T, String)>> {
        match self.s3.get_object().bucket(&self.bucket).key(key).send().await {
            Ok(out) => {
                let etag = out.e_tag().unwrap_or_default().to_owned();
                let bytes = out.body.collect().await.with_context(|| format!("reading s3://{}/{key}", self.bucket))?;
                let value = serde_json::from_slice(&bytes.into_bytes()).with_context(|| format!("parsing s3://{}/{key}", self.bucket))?;
                Ok(Some((value, etag)))
            }
            Err(e) if e.as_service_error().is_some_and(|e| e.is_no_such_key()) => Ok(None),
            Err(e) => bail!("reading s3://{}/{key}: {}", self.bucket, s3_error(&e)),
        }
    }

    pub async fn get_value<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        Ok(self.get(key).await?.map(|(v, _)| v))
    }

    /// Write `value`, if the object is still at `etag` (or doesn't exist, for `None`).
    /// Returns false if someone else wrote it first.
    ///
    /// An ETag read from one store and offered to the other (the mode changed between a read and
    /// its write) never matches: the write is refused as lost, and `update` reads again.
    async fn put_if<T: Serialize>(&self, key: &str, value: &T, etag: Option<&str>) -> Result<bool> {
        match self.place(key).await? {
            Place::Bucket => self.bucket_put_if(key, value, etag).await,
            Place::Table { kind, pk, config } if config.mode == Mode::Mirror => {
                if !self.bucket_put_if(key, value, etag).await? {
                    return Ok(false);
                }
                // The bucket has it, and is what's read: a copy that fails leaves the table behind,
                // which `forkrail state check --repair` mends. The write itself stands.
                if let Err(e) = self.items().copy(&config.table, &pk, kind, &serde_json::to_string(value)?, now_ms()).await {
                    eprintln!("warning: {key} was written, but not copied to {}: {e:#}", config.table);
                }
                Ok(true)
            }
            Place::Table { kind, pk, config } => {
                let expect = match etag {
                    Some(etag) => match etag.parse::<u64>() {
                        Ok(v) => Some(v),
                        Err(_) => return Ok(false),
                    },
                    None => None,
                };
                if !self.items().put_if(&config.table, &pk, kind, &serde_json::to_string(value)?, expect).await? {
                    return Ok(false);
                }
                if config.s3_copy
                    && let Err(e) = self.bucket_put(key, value).await
                {
                    eprintln!("warning: {key} was written to {}, but not copied to the bucket: {e:#}", config.table);
                }
                Ok(true)
            }
        }
    }

    /// Write `value` whatever is there: the bucket's copy of a file the table holds.
    async fn bucket_put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.put_bytes(key, serde_json::to_vec_pretty(value)?, "application/json").await
    }

    async fn bucket_put_if<T: Serialize>(&self, key: &str, value: &T, etag: Option<&str>) -> Result<bool> {
        let body = serde_json::to_vec_pretty(value)?;
        let put = self.s3.put_object().bucket(&self.bucket).key(key).content_type("application/json").body(ByteStream::from(body));
        let put = match etag {
            Some(etag) => put.if_match(etag),
            None => put.if_none_match("*"),
        };
        match put.send().await {
            Ok(_) => Ok(true),
            Err(e) if matches!(e.raw_response().map(|r| r.status().as_u16()), Some(412 | 409)) => Ok(false),
            Err(e) => bail!("writing s3://{}/{key}: {}", self.bucket, s3_error(&e)),
        }
    }

    /// Create the object; false if it already exists.
    pub async fn create<T: Serialize>(&self, key: &str, value: &T) -> Result<bool> {
        self.put_if(key, value, None).await
    }

    /// Overwrite the object if it's still at `etag`; false if someone else wrote it first.
    pub async fn replace<T: Serialize>(&self, key: &str, value: &T, etag: &str) -> Result<bool> {
        self.put_if(key, value, Some(etag)).await
    }

    /// Read, change and write back, retrying from a fresh read when another writer got in first.
    /// `change` gets the current value (`None` if there's none yet) and returns the new one.
    pub async fn update<T, F>(&self, key: &str, mut change: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnMut(Option<T>) -> Result<T>,
    {
        for attempt in 0..MAX_TRIES {
            let current = self.get::<T>(key).await?;
            let etag = current.as_ref().map(|(_, etag)| etag.clone());
            let next = change(current.map(|(v, _)| v))?;
            if self.put_if(key, &next, etag.as_deref()).await? {
                return Ok(next);
            }
            // Someone else wrote it; back off a little, with jitter, and start again.
            let jitter = u64::from(u16::from_str_radix(&forkrail_common::random_hex(2), 16).unwrap_or(0) % 200);
            tokio::time::sleep(Duration::from_millis(50 * u64::from(attempt + 1) + jitter)).await;
        }
        bail!("{key} kept changing under us; try again")
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        match self.place(key).await? {
            Place::Bucket => self.bucket_delete(key).await,
            Place::Table { pk, config, .. } if config.mode == Mode::Mirror => {
                self.bucket_delete(key).await?;
                self.items().delete(&config.table, &pk).await
            }
            Place::Table { pk, config, .. } => {
                self.items().delete(&config.table, &pk).await?;
                if config.s3_copy {
                    self.bucket_delete(key).await?;
                }
                Ok(())
            }
        }
    }

    pub(crate) async fn bucket_delete(&self, key: &str) -> Result<()> {
        self.s3
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("deleting s3://{}/{key}: {}", self.bucket, s3_error(&e)))?;
        Ok(())
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        match self.place(key).await? {
            Place::Table { pk, config, .. } if config.mode == Mode::Dynamodb => Ok(self.items().get(&config.table, &pk).await?.is_some()),
            _ => self.bucket_exists(key).await,
        }
    }

    async fn bucket_exists(&self, key: &str) -> Result<bool> {
        match self.s3.head_object().bucket(&self.bucket).key(key).send().await {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found()) => Ok(false),
            Err(e) => bail!("checking s3://{}/{key}: {}", self.bucket, s3_error(&e)),
        }
    }

    pub async fn upload(&self, key: &str, path: &Path) -> Result<()> {
        upload(&self.s3, &self.bucket, key, path).await
    }

    /// Keys under `prefix`. All the project files, or all the deployment records, come from the
    /// table once it's the one read: its index lags a write by a moment, which a listing of the
    /// bucket didn't.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        if let (Some(table), Some(kind)) = (&self.table, table_kind(prefix)) {
            let config = self.store_config().await?;
            if config.mode == Mode::Dynamodb {
                return Ok(table.items.list(&config.table, kind).await?.into_iter().map(|(pk, _)| state_key(&pk)).collect());
            }
        }
        self.bucket_list(prefix).await
    }

    pub async fn bucket_list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut pages = self.s3.list_objects_v2().bucket(&self.bucket).prefix(prefix).into_paginator().send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("listing s3://{}/{prefix}: {}", self.bucket, s3_error(&e)))?;
            keys.extend(page.contents().iter().filter_map(|o| o.key().map(str::to_owned)));
        }
        Ok(keys)
    }

    pub async fn settings(&self) -> Result<Settings> {
        Ok(self.get_value(SETTINGS_KEY).await?.unwrap_or_default())
    }

    /// Put `body` under `key` as it is.
    pub async fn put_bytes(&self, key: &str, body: Vec<u8>, content_type: &str) -> Result<()> {
        self.s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("writing s3://{}/{key}: {}", self.bucket, s3_error(&e)))?;
        Ok(())
    }

    /// Put `body` under `key` encrypted with the KMS key `kms_key` (the env key): only roles that
    /// may decrypt with it can read the object, which the routers' can't.
    pub async fn put_sealed(&self, key: &str, body: ByteStream, content_type: &str, kms_key: &str) -> Result<()> {
        self.s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
            .ssekms_key_id(kms_key)
            .bucket_key_enabled(true)
            .body(body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("writing s3://{}/{key}: {}", self.bucket, s3_error(&e)))?;
        Ok(())
    }

    /// The object into the file `path`, streamed; false if there's no such object.
    pub async fn download(&self, key: &str, path: &Path) -> Result<bool> {
        match self.s3.get_object().bucket(&self.bucket).key(key).send().await {
            Ok(out) => {
                let mut file = tokio::fs::File::create(path).await.with_context(|| format!("creating {}", path.display()))?;
                let mut body = out.body.into_async_read();
                tokio::io::copy(&mut body, &mut file).await.with_context(|| format!("reading s3://{}/{key}", self.bucket))?;
                Ok(true)
            }
            Err(e) if e.as_service_error().is_some_and(|e| e.is_no_such_key()) => Ok(false),
            Err(e) => bail!("reading s3://{}/{key}: {}", self.bucket, s3_error(&e)),
        }
    }

    /// A URL that GETs `key` for `ttl`, with this client's rights, for someone with none.
    pub async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String> {
        let config = aws_sdk_s3::presigning::PresigningConfig::expires_in(ttl)?;
        let request = self.s3.get_object().bucket(&self.bucket).key(key).presigned(config).await.map_err(|e| anyhow::anyhow!("presigning {key}: {}", s3_error(&e)))?;
        Ok(request.uri().to_owned())
    }

    /// A URL that PUTs `key` for `ttl`.
    pub async fn presign_put(&self, key: &str, ttl: Duration) -> Result<String> {
        let config = aws_sdk_s3::presigning::PresigningConfig::expires_in(ttl)?;
        let request = self.s3.put_object().bucket(&self.bucket).key(key).presigned(config).await.map_err(|e| anyhow::anyhow!("presigning {key}: {}", s3_error(&e)))?;
        Ok(request.uri().to_owned())
    }

    /// A URL that PUTs `key` for `ttl`, for one object only: `len` bytes whose SHA-256 is `sha256`
    /// (hex), stored encrypted under `kms_key` as `put_sealed` stores them. S3 refuses anything
    /// else, so whoever is handed the URL can put there only what they said they would. The
    /// headers are to be sent with the PUT, as given.
    pub async fn presign_put_sealed(&self, key: &str, ttl: Duration, kms_key: &str, sha256: &str, len: u64) -> Result<(String, Vec<(String, String)>)> {
        use base64::Engine;
        let digest = unhex(sha256).filter(|d| d.len() == 32).with_context(|| format!("{sha256:?} isn't a SHA-256"))?;
        let config = aws_sdk_s3::presigning::PresigningConfig::expires_in(ttl)?;
        let request = self
            .s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type("application/zip")
            .content_length(i64::try_from(len).context("too large")?)
            .checksum_sha256(base64::engine::general_purpose::STANDARD.encode(digest))
            .server_side_encryption(aws_sdk_s3::types::ServerSideEncryption::AwsKms)
            .ssekms_key_id(kms_key)
            .bucket_key_enabled(true)
            .presigned(config)
            .await
            .map_err(|e| anyhow::anyhow!("presigning {key}: {}", s3_error(&e)))?;
        let headers = request.headers().filter(|(name, _)| !name.eq_ignore_ascii_case("host")).map(|(n, v)| (n.to_owned(), v.to_owned())).collect();
        Ok((request.uri().to_owned(), headers))
    }

    /// The object's bytes, if it exists.
    pub async fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.s3.get_object().bucket(&self.bucket).key(key).send().await {
            Ok(out) => Ok(Some(out.body.collect().await.with_context(|| format!("reading s3://{}/{key}", self.bucket))?.into_bytes().to_vec())),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_no_such_key()) => Ok(None),
            Err(e) => bail!("reading s3://{}/{key}: {}", self.bucket, s3_error(&e)),
        }
    }
}

/// The bytes a string of hex digits spells, if it is one.
fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

/// An S3 error in one line: S3's code and message when it answered with them, else the whole chain.
pub fn s3_error<E, R>(e: &aws_sdk_s3::error::SdkError<E, R>) -> String
where
    E: aws_sdk_s3::error::ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug,
{
    if let Some(service) = e.as_service_error()
        && let Some(code) = service.code()
    {
        return match service.message() {
            Some(message) => format!("{code}: {message}"),
            None => code.to_owned(),
        };
    }
    DisplayErrorContext(e).to_string()
}

/// Put the file at `path` in `bucket` under `key`.
pub async fn upload(s3: &aws_sdk_s3::Client, bucket: &str, key: &str, path: &Path) -> Result<()> {
    let body = ByteStream::from_path(path).await.with_context(|| format!("reading {}", path.display()))?;
    s3.put_object()
        .bucket(bucket)
        .key(key)
        .content_type("application/zip")
        .body(body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("uploading s3://{bucket}/{key}: {}", s3_error(&e)))?;
    Ok(())
}

/// Unix seconds now.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The current UTC time as RFC 3339, to the second.
pub fn now_rfc3339() -> String {
    rfc3339(now_secs())
}

/// The current UTC time as RFC 3339, to the millisecond.
pub fn now_rfc3339_ms() -> String {
    let ms = now_ms();
    let secs = rfc3339(ms / 1000);
    format!("{}.{:03}Z", secs.trim_end_matches('Z'), ms % 1000)
}

/// Unix seconds of an RFC 3339 UTC time as forkrail writes them (`2026-11-11T10:53:40Z`, with or
/// without milliseconds).
/// Unix milliseconds of an RFC 3339 UTC time, with its fraction if it has one.
pub fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    let secs = parse_rfc3339(text)?;
    let fraction = text.strip_suffix('Z')?.split_once('.').map_or("", |(_, f)| f);
    let ms: String = fraction.chars().chain("000".chars()).take(3).collect();
    Some(secs * 1000 + ms.parse::<u64>().ok()?)
}

pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days since 1970-01-01 from a civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

pub fn rfc3339(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store whose project files and records are in a table in memory, and whose bucket is never
    /// reached (a request to it would fail: these tests make none).
    fn table_store(config: StoreConfig) -> Store {
        let s3 = aws_sdk_s3::Config::builder().behavior_version(aws_config::BehaviorVersion::latest()).region(aws_config::Region::new("us-east-1")).build();
        Store { s3: aws_sdk_s3::Client::from_conf(s3), bucket: "none".into(), table: Some(Arc::new(Table::memory(config))) }
    }

    fn in_table() -> StoreConfig {
        StoreConfig { mode: Mode::Dynamodb, table: "forkrail-state".into(), s3_copy: false }
    }

    #[tokio::test]
    async fn project_files_live_in_the_table() {
        let store = table_store(in_table());
        let key = project_key("pyapp");
        let mut project = crate::deploy::new_project("pyapp");
        assert!(store.create(&key, &project).await.unwrap());
        assert!(!store.create(&key, &project).await.unwrap(), "created twice");
        assert!(store.exists(&key).await.unwrap());
        let (read, etag) = store.get::<Project>(&key).await.unwrap().unwrap();
        assert_eq!((read.name.as_str(), etag.as_str()), ("pyapp", "1"));
        // A writer holding the version before someone else's write loses, and the one holding the
        // new version wins.
        project.production = Some("a1b2c3d4".into());
        assert!(store.replace(&key, &project, "1").await.unwrap());
        assert!(!store.replace(&key, &project, "1").await.unwrap());
        // An ETag from the bucket (the mode changed between a read and its write) is a lost race.
        assert!(!store.replace(&key, &project, "\"9f86d081884c7d65\"").await.unwrap());
        let updated = store
            .update::<Project, _>(&key, |current| {
                let mut p = current.context("gone")?;
                p.production_history = vec!["a1b2c3d4".into()];
                Ok(p)
            })
            .await
            .unwrap();
        assert_eq!(updated.production.as_deref(), Some("a1b2c3d4"));
        let (read, etag) = store.get::<Project>(&key).await.unwrap().unwrap();
        assert_eq!((read.production_history, etag.as_str()), (vec!["a1b2c3d4".to_owned()], "3"));
        // The table's copy is the file as the bucket held it.
        let item = store.table.as_ref().unwrap().items.get("forkrail-state", "projects/pyapp").await.unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Project>(&item.doc).unwrap().name, "pyapp");
    }

    #[tokio::test]
    async fn listings_and_deletes_come_from_the_table() {
        let store = table_store(in_table());
        for name in ["b", "a"] {
            assert!(store.create(&project_key(name), &crate::deploy::new_project(name)).await.unwrap());
        }
        let record = serde_json::json!({"id": "a1b2c3d4", "project": "a", "created": "2026-11-13T10:00:00Z"});
        assert!(store.create(&deployment_key("a1b2c3d4"), &record).await.unwrap());
        assert_eq!(store.list(forkrail_common::install::PROJECTS_PREFIX).await.unwrap(), vec![project_key("a"), project_key("b")]);
        assert_eq!(store.list(forkrail_common::install::DEPLOYMENTS_PREFIX).await.unwrap(), vec![deployment_key("a1b2c3d4")]);
        store.delete(&deployment_key("a1b2c3d4")).await.unwrap();
        assert!(!store.exists(&deployment_key("a1b2c3d4")).await.unwrap());
        assert!(store.get_value::<serde_json::Value>(&deployment_key("a1b2c3d4")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_updates_all_land() {
        let store = table_store(in_table());
        let key = project_key("pyapp");
        store.create(&key, &crate::deploy::new_project("pyapp")).await.unwrap();
        let mut writers = tokio::task::JoinSet::new();
        for i in 0..8 {
            let store = store.clone();
            let key = key.clone();
            writers.spawn(async move {
                store
                    .update::<Project, _>(&key, |current| {
                        let mut p = current.context("gone")?;
                        p.production_history.push(format!("d{i}"));
                        Ok(p)
                    })
                    .await
            });
        }
        while let Some(done) = writers.join_next().await {
            done.unwrap().unwrap();
        }
        let (read, etag) = store.get::<Project>(&key).await.unwrap().unwrap();
        assert_eq!((read.production_history.len(), etag.as_str()), (8, "9"));
    }

    /// Against a real table made like deploy/install/state.yaml's (`forkrail-state-test`, or
    /// FORKRAIL_TEST_TABLE): `AWS_PROFILE=forkrail cargo test -p forkrail-cli live_ -- --ignored`.
    /// Only items under `projects/test-<random>` are written, and they're deleted at the end.
    #[tokio::test]
    #[ignore = "needs AWS credentials and the test table"]
    async fn live_concurrent_updates_in_a_real_table() {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest()).region(aws_config::Region::new("us-east-1")).load().await;
        let table = std::env::var("FORKRAIL_TEST_TABLE").unwrap_or_else(|_| "forkrail-state-test".into());
        let items = Items::Dynamo(aws_sdk_dynamodb::Client::new(&config));
        let mut store = table_store(StoreConfig { mode: Mode::Dynamodb, table: table.clone(), s3_copy: false });
        store.table = Some(Arc::new(Table::pinned(items, StoreConfig { mode: Mode::Dynamodb, table, s3_copy: false })));
        let name = format!("test-{}", forkrail_common::random_hex(4));
        let key = project_key(&name);
        assert!(store.create(&key, &crate::deploy::new_project(&name)).await.unwrap());
        assert!(!store.create(&key, &crate::deploy::new_project(&name)).await.unwrap());
        let mut writers = tokio::task::JoinSet::new();
        for i in 0..8 {
            let store = store.clone();
            let key = key.clone();
            writers.spawn(async move {
                store
                    .update::<Project, _>(&key, |current| {
                        let mut p = current.context("gone")?;
                        p.production_history.push(format!("d{i}"));
                        Ok(p)
                    })
                    .await
            });
        }
        while let Some(done) = writers.join_next().await {
            done.unwrap().unwrap();
        }
        let (read, etag) = store.get::<Project>(&key).await.unwrap().unwrap();
        assert_eq!((read.production_history.len(), etag.as_str()), (8, "9"));
        // The listing index catches up within moments.
        let mut listed = false;
        for _ in 0..20 {
            if store.list(forkrail_common::install::PROJECTS_PREFIX).await.unwrap().contains(&key) {
                listed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(listed, "{key} never listed");
        store.delete(&key).await.unwrap();
        assert!(!store.exists(&key).await.unwrap());
    }

    #[test]
    fn urls_are_https_except_under_test() {
        let settings = |base: &str| Settings { base_domain: base.into(), ..Default::default() };
        assert_eq!(settings("forkrail.com").url("atlas.forkrail.com"), "https://atlas.forkrail.com");
        assert_eq!(settings("forkrail.test").url("pyapp.forkrail.test"), "http://pyapp.forkrail.test");
    }

    #[test]
    fn timestamp_shape() {
        let now = now_rfc3339();
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.starts_with("20") && now.ends_with('Z'));
        let ms = now_rfc3339_ms();
        assert_eq!(ms.len(), 24, "{ms}");
        // Both read back, and the time is now.
        assert!(parse_rfc3339(&now).unwrap().abs_diff(now_secs()) <= 1);
        assert!(parse_rfc3339(&ms).unwrap().abs_diff(now_secs()) <= 1);
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-11-11T10:53:40Z"), Some(1_794_394_420));
        assert_eq!(parse_rfc3339("2024-02-29T23:59:59.999Z"), Some(1_709_251_199));
        assert_eq!(parse_rfc3339_ms("2024-02-29T23:59:59.999Z"), Some(1_709_251_199_999));
        assert_eq!(parse_rfc3339_ms("2024-02-29T23:59:59.5Z"), Some(1_709_251_199_500));
        assert_eq!(parse_rfc3339_ms("2026-11-11T10:53:40Z"), Some(1_794_394_420_000));
        for bad in ["", "2026-11-11", "2026-13-01T00:00:00Z", "2026-11-11T10:53:40", "x-11-11T10:53:40Z"] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_deployments_origin_reads_even_when_its_newer_than_this_forkrail() {
        assert_eq!(serde_json::from_str::<Via>(r#""git""#).unwrap(), Via::Git);
        assert_eq!(serde_json::to_string(&Via::Git).unwrap(), r#""git""#);
        assert_eq!(serde_json::from_str::<Via>(r#""api""#).unwrap(), Via::Other);
    }

    #[test]
    fn statuses_read_as_failed_once_stale() {
        let started = rfc3339(now_secs() - 60);
        assert_eq!(Status::Building.at(&started, now_secs()), Status::Building);
        let long_ago = rfc3339(now_secs() - STALE_AFTER_S - 1);
        assert_eq!(Status::Building.at(&long_ago, now_secs()), Status::Error);
        assert_eq!(Status::Queued.at(&long_ago, now_secs()), Status::Error);
        // A finished one stays as it ended.
        assert_eq!(Status::Ready.at(&long_ago, now_secs()), Status::Ready);
        assert_eq!(Status::Canceled.at(&long_ago, now_secs()), Status::Canceled);
        // Summaries and records from before statuses are published ones.
        let summary: Summary = serde_json::from_str(r#"{"id": "a1b2c3d4", "target": "preview", "created": "", "version": "3"}"#).unwrap();
        assert_eq!(summary.status, Status::Ready);
        assert_eq!(serde_json::to_value(&summary).unwrap()["status"], "ready");
        // A record of a build that's still running parses with nothing published.
        let building = r#"{"id": "a1b2c3d4", "project": "p", "target": "preview", "created": "2026-11-11T10:00:00Z",
            "status": "building", "building_at": "2026-11-11T10:00:00.120Z", "via": "dashboard", "log": {"key_sha256": "ab"}}"#;
        let record: DeploymentRecord = serde_json::from_str(building).unwrap();
        assert_eq!((record.status, record.via, record.version.as_str()), (Status::Building, Some(Via::Dashboard), ""));
        assert_eq!(record.status_now(), Status::Error);
        assert!(record.check_ready().unwrap_err().to_string().contains("never published"));
    }

    #[test]
    fn project_files_keep_names_and_old_values_until_moved() {
        let mut project = Project { name: "p".into(), ..Default::default() };
        project.env_names.get_mut(Target::Preview).insert("K".into());
        let json = serde_json::to_value(&project).unwrap();
        // No values in a file written now, and a notice where an older forkrail expects them.
        assert_eq!(json["env"], ENV_MOVED);
        assert_eq!(json["env_names"]["preview"][0], "K");
        // Development is written only when it has names, so files without any stay as they were.
        assert!(json["env_names"].get("development").is_none());
        let mut with_dev = project.clone();
        with_dev.env_names.get_mut(EnvTarget::Development).insert("LOCAL".into());
        let dev_json = serde_json::to_value(&with_dev).unwrap();
        assert_eq!(dev_json["env_names"]["development"][0], "LOCAL");
        assert_eq!(serde_json::from_value::<Project>(dev_json).unwrap().env_names, with_dev.env_names);
        assert!(EnvTarget::Development.readable() && EnvTarget::Development.deployed().is_none());
        assert!(EnvTarget::ALL.iter().filter(|t| t.readable()).count() == 1);
        // A branch's own preview names: left out until there are some, kept by name, gone with
        // their last.
        assert!(json["env_names"].get("branches").is_none());
        let mut with_branch = project.clone();
        with_branch.env_names.add_to_branch("feature/logs", ["API_URL".to_owned()]);
        let branch_json = serde_json::to_value(&with_branch).unwrap();
        assert_eq!(branch_json["env_names"]["branches"]["feature/logs"][0], "API_URL");
        assert_eq!(serde_json::from_value::<Project>(branch_json).unwrap().env_names, with_branch.env_names);
        let names = &with_branch.env_names;
        assert_eq!(names.deployed(Target::Preview, Some("feature/logs")).into_iter().collect::<Vec<_>>(), ["API_URL", "K"]);
        assert_eq!(names.deployed(Target::Preview, Some("other")).into_iter().collect::<Vec<_>>(), ["K"]);
        assert!(names.deployed(Target::Production, Some("feature/logs")).is_empty());
        assert!(!with_branch.env_names.remove_from_branch("feature/logs", "OTHER"));
        assert!(with_branch.env_names.remove_from_branch("feature/logs", "API_URL"));
        assert!(with_branch.env_names.branches.is_empty() && !with_branch.env_names.remove_from_branch("feature/logs", "API_URL"));
        // Which branch's values a deployment gets: a preview's, never production's, and none for a
        // detached checkout.
        let git = |branch: &str| GitCommit { repo: "a/b".into(), branch: branch.into(), sha: "1".into(), message: String::new(), full_sha: None, author: None };
        assert_eq!(env_branch(Target::Preview, Some(&git("feature/logs"))), Some("feature/logs"));
        assert_eq!(env_branch(Target::Production, Some(&git("feature/logs"))), None);
        assert_eq!((env_branch(Target::Preview, Some(&git("HEAD"))), env_branch(Target::Preview, None)), (None, None));
        let back: Project = serde_json::from_value(json).unwrap();
        assert_eq!((back.env_names, back.legacy_env), (project.env_names, LegacyEnv::Moved));
        assert_eq!(serde_json::to_string(&Target::Production).unwrap(), "\"production\"");

        // A file from before the move: its values are read, and written back as they were until
        // `env migrate` moves them.
        let old = r#"{"name": "p", "function": "f", "created": "", "env": {"production": {"A": "1"}, "preview": {}}}"#;
        let old: Project = serde_json::from_str(old).unwrap();
        assert!(old.legacy_env.has_values());
        assert_eq!(serde_json::to_value(&old).unwrap()["env"]["production"]["A"], "1");
        // One whose maps are empty, or that has none, has nothing to move.
        for none in [r#"{"name": "p", "function": "f", "created": "", "env": {"production": {}}}"#, r#"{"name": "p", "function": "f", "created": ""}"#] {
            assert_eq!(serde_json::from_str::<Project>(none).unwrap().legacy_env, LegacyEnv::Moved);
        }
    }

    #[test]
    fn records_without_the_newer_fields_still_parse() {
        // A record as the CLI wrote it before frameworks, deployers and commits were recorded.
        let old = r#"{"id": "a1b2c3d4", "project": "p", "target": "production", "created": "2026-11-05T00:00:00Z",
            "function": "forkrail-app-p", "version": "3", "runtime": "python3.13", "entry": "api/main.py",
            "memory_mb": 768, "max_duration_s": 300, "source_sha256": "s", "package_sha256": "k",
            "package_key": "packages/p/k.zip", "package_bytes": 1, "env_keys": [], "hosts": []}"#;
        let mut record: DeploymentRecord = serde_json::from_str(old).unwrap();
        assert_eq!((&record.framework, &record.deployer, &record.git), (&None, &None, &None));
        // Written back (as `regions add` does), it gains no keys.
        let written = serde_json::to_value(&record).unwrap();
        assert!(["framework", "deployer", "git"].iter().all(|k| written.get(k).is_none()), "{written}");

        record.framework = Some("FastAPI".into());
        record.deployer = Some("Ada Lovelace".into());
        record.git =
            Some(GitCommit { repo: "ada/engine".into(), branch: "main".into(), sha: "1a2b3c4".into(), message: "Add the mill".into(), full_sha: None, author: None });
        let written = serde_json::to_value(&record).unwrap();
        assert_eq!(written["framework"], "FastAPI");
        assert_eq!(written["deployer"], "Ada Lovelace");
        assert_eq!(
            written["git"],
            serde_json::json!({"repo": "ada/engine", "branch": "main", "sha": "1a2b3c4", "message": "Add the mill"})
        );
        let back: DeploymentRecord = serde_json::from_value(written).unwrap();
        assert_eq!(back.git, record.git);
    }

    #[test]
    fn redeploys_take_the_settings_recorded_at_build_time() {
        let old = r#"{"id": "a1b2c3d4", "project": "p", "target": "production", "created": "2026-11-05T00:00:00Z",
            "memory_mb": 768, "fluid": false,
            "config": {"fluid": false, "functions": {"api/main.py": {"memory": 768, "maxDuration": 60}}}}"#;
        let mut record: DeploymentRecord = serde_json::from_str(old).unwrap();
        // A record from before they were kept: read from the vercel.json it keeps.
        assert_eq!(record.pinned, None);
        assert_eq!(record.pinned_settings(), Pinned { memory_mb: Some(768), max_duration_s: Some(60), concurrent: Some(false), floor: None, regions: None });
        // Once recorded, the record's own values are used, whatever `config` holds.
        record.pinned = Some(Pinned { memory_mb: None, max_duration_s: Some(30), concurrent: None, floor: None, regions: None });
        assert_eq!(record.pinned_settings(), Pinned { memory_mb: None, max_duration_s: Some(30), concurrent: None, floor: None, regions: None });
        let written = serde_json::to_value(&record).unwrap();
        assert_eq!(written["pinned"], serde_json::json!({"max_duration_s": 30}));
        // vercel.json's regions, by AWS's names.
        let with_regions = old.replace(r#""config": {"#, r#""config": {"regions": ["fra1", "iad1"], "#);
        let regional: DeploymentRecord = serde_json::from_str(&with_regions).unwrap();
        assert_eq!(regional.pinned_settings().regions, Some(vec!["eu-central-1".to_owned(), "us-east-1".to_owned()]));
        // A record without them gains no keys when written back.
        let plain: DeploymentRecord = serde_json::from_str(old).unwrap();
        let written = serde_json::to_value(&plain).unwrap();
        assert!(written.get("pinned").is_none() && written.get("importer").is_none(), "{written}");
    }

    #[test]
    fn concurrent_compute_is_kept_under_fluid() {
        // Stored under vercel.json's key, so forkrails and dashboards from before the name read it.
        let old = r#"{"id": "a1b2c3d4", "project": "p", "target": "production", "created": "2026-11-05T00:00:00Z", "fluid": false}"#;
        let mut record: DeploymentRecord = serde_json::from_str(old).unwrap();
        assert!(!record.concurrent);
        record.pinned = Some(Pinned { concurrent: Some(false), ..Pinned::default() });
        let written = serde_json::to_value(&record).unwrap();
        assert_eq!((&written["fluid"], &written["pinned"]), (&serde_json::json!(false), &serde_json::json!({"fluid": false})));
        assert!(written.get("concurrent").is_none(), "{written}");
        // A record from before the setting existed ran concurrent compute.
        let older: DeploymentRecord = serde_json::from_str(r#"{"id": "a1b2c3d4", "project": "p", "target": "production", "created": "2026-11-05T00:00:00Z"}"#).unwrap();
        assert!(older.concurrent);
        // The project's setting too.
        let project: Project = serde_json::from_str(r#"{"name": "p", "function": "f", "created": "", "fluid": false}"#).unwrap();
        assert_eq!(project.concurrent, Some(false));
        let written = serde_json::to_value(&project).unwrap();
        assert!(written["fluid"] == false && written.get("concurrent").is_none(), "{written}");
        // Function regions: left out while the default.
        assert!(written.get("regions").is_none(), "{written}");
        let regional: Project = serde_json::from_str(r#"{"name": "p", "function": "f", "created": "", "regions": ["us-east-1", "eu-central-1"]}"#).unwrap();
        assert_eq!(regional.regions, ["us-east-1", "eu-central-1"]);
    }
}
