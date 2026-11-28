//! `forkrail`: deploy a directory as an immutable deployment, promote it, roll back, and manage
//! env vars, all in your own AWS account. State lives in S3 (`state/` in the artifact bucket);
//! the router reads `state/routes.json`.
//!
//! The install is found from `--profile` and `--region` (the home region), then `AWS_PROFILE` and
//! `FORKRAIL_HOME_REGION`, then what `forkrail link` saved (config.rs); without a profile, the AWS
//! SDK's default credentials, and without a region us-east-1. The bucket is
//! `forkrail-artifacts-<account>` unless `FORKRAIL_BUCKET` or the saved settings name another.
//! Deployments are also published in the regions `forkrail regions add` names.
//!
//! Someone without AWS credentials signs in to the install's dashboard instead (`forkrail login`,
//! login.rs): deploy, ls, inspect, logs, promote, rollback, redeploy and cancel then go through the
//! install's API as that person (api.rs). `--aws`, `--profile` or FORKRAIL_AWS=1 use AWS
//! credentials although signed in.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use aws_config::{BehaviorVersion, Region};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use forkrail_common::install::Install;
use forkrail_common::routes::Routes;
use forkrail_cli::build::{self, Build};
use forkrail_cli::deploy::{self, Ctx, DeployArgs, PreviewEnv, Redeploy, log, project_record};
use forkrail_cli::project::Change;
use forkrail_cli::publish::{self, Destination, Files};
use forkrail_cli::secrets::Secrets;
use forkrail_cli::state::{self, DeploymentRecord, EnvNames, EnvTarget, LegacyEnv, Project, ROUTES_KEY, SETTINGS_KEY, Settings, Status, Target, Via};
use forkrail_cli::{api, buildlog, config, connect, domains, env as env_vars, git, github, install, login, observe, package, project, regions, remote, report, storage};

#[derive(Parser)]
#[command(name = "forkrail", version, about = "Deploy Vercel-style apps to your own AWS account")]
struct Cli {
    /// AWS profile; defaults to AWS_PROFILE, then the one `forkrail link` saved, then the AWS SDK's
    /// default credentials.
    #[arg(long, global = true)]
    profile: Option<String>,
    /// The install's home region, where its state is; defaults to FORKRAIL_HOME_REGION, then the
    /// one `forkrail link` saved, then us-east-1.
    #[arg(long, global = true)]
    region: Option<String>,
    /// Use this machine's AWS credentials although it's signed in (`forkrail login`): the install's
    /// administrator's way. --profile and FORKRAIL_AWS=1 do the same.
    #[arg(long, global = true)]
    aws: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign this terminal in to an install, by its dashboard's address: a page opens where you
    /// approve it, signed in with GitHub. From then on deploy, ls, inspect, logs, promote, rollback,
    /// redeploy and cancel go through the dashboard as you, and need no AWS credentials.
    Login {
        /// The dashboard, `https://dashboard.example.com`; the one last signed in to if not given.
        url: Option<String>,
        /// Only print the address to open (on another machine, say).
        #[arg(long)]
        no_browser: bool,
    },
    /// Sign this terminal out.
    Logout,
    /// Install forkrail in an AWS account, from nothing: checks the account first, changes nothing
    /// until they pass, and continues where it stopped when run again. An account holds one
    /// install; one already there is finished or left alone, never installed twice.
    Install {
        /// The domain apps get names under: <project>.DOMAIN, and the dashboard dashboard.DOMAIN.
        /// Its DNS must be on Cloudflare.
        #[arg(long)]
        domain: Option<String>,
        /// Where alerts are mailed (budgets, certificates); none if not given.
        #[arg(long)]
        email: Option<String>,
        /// Only run the checks; change nothing.
        #[arg(long)]
        check: bool,
        /// Don't ask: take the flags, FORKRAIL_CLOUDFLARE_TOKEN and the defaults.
        #[arg(long)]
        yes: bool,
        /// Ask in the terminal, never open a browser.
        #[arg(long)]
        terminal: bool,
        /// Install from this folder's release instead of downloading this version's. The Mac app
        /// passes its environment on, so `open --env FORKRAIL_RELEASE=DIR Forkrail.app` does the same.
        #[arg(long, env = "FORKRAIL_RELEASE", value_name = "DIR")]
        release: Option<PathBuf>,
    },
    /// Delete everything `forkrail install` made in the account: the router, the analytics box,
    /// the apps and their data, the dashboard, the bucket, the secrets and the DNS records it made.
    /// An install set up by hand is refused.
    Uninstall {
        /// Don't ask for the domain to confirm.
        #[arg(long)]
        yes: bool,
    },
    /// Who this terminal is signed in as, or which AWS account it reaches.
    Whoami,
    /// Deploy DIR: a preview, or production with --prod. The source is uploaded and built in the
    /// install's builder, signed in (`forkrail login`) or with AWS credentials; --local builds on
    /// this machine (AWS credentials only).
    Deploy {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        prod: bool,
        /// Defaults to the directory's name.
        #[arg(long)]
        project: Option<String>,
        /// Memory size in MB (128-10240) for this deployment only, in every region it runs in.
        /// Without it: vercel.json's, then the project's setting (`forkrail project NAME
        /// --memory`, or the dashboard's Functions settings), then 640.
        #[arg(long, value_parser = clap::value_parser!(u32).range(128..=10_240))]
        memory: Option<u32>,
        /// Concurrent compute: true packs many requests onto each worker; false runs one request
        /// per worker, as classic Lambda does, so requests in flight at once get their own
        /// environments and outbound IPs. Defaults to vercel.json's `fluid`, then the project's
        /// setting, then true. Also `--fluid`.
        #[arg(long, alias = "fluid", value_name = "BOOL", action = clap::ArgAction::Set)]
        concurrent: Option<bool>,
        /// The runtime binary for Linux arm64, for a build on this machine (implies --local). Not
        /// read from FORKRAIL_BOOTSTRAP here, so a shell with it set still builds in the builder.
        #[arg(long)]
        bootstrap: Option<PathBuf>,
        /// Python 3.13, for pip and the bytecode.
        #[arg(long, env = "FORKRAIL_PYTHON", default_value = "python3.13")]
        python: String,
        /// Build without what earlier builds kept: the Python requirements are installed again,
        /// and with --remote the project's build cache isn't restored (a new one is written).
        #[arg(long)]
        no_cache: bool,
        /// Refuse unless the project's files, memory size, maxDuration and mode are production's:
        /// for deploying the same app again on a new runtime.
        #[arg(long)]
        same_source: bool,
        /// For a framework (TanStack Start, Nuxt): deploy its existing build output instead of
        /// building first.
        #[arg(long)]
        prebuilt: bool,
        /// With --prod: build and publish the production deployment but leave the project's
        /// production host and custom domains on the current one. It answers on its own
        /// `<project>-<id>` host, to be checked there, and `forkrail promote <id>` puts it live.
        #[arg(long, requires = "prod")]
        skip_domain: bool,
        /// Build in the install's builder (the default): the source is uploaded, built on Lambda's
        /// own Linux and Python, and published from there, with the log shown here and in the
        /// dashboard as it's written. Closing this terminal doesn't stop it.
        #[arg(long, conflicts_with_all = ["prebuilt", "bootstrap", "local"])]
        remote: bool,
        /// Build on this machine instead. Needs the install's AWS credentials; --prebuilt and
        /// --bootstrap imply it.
        #[arg(long)]
        local: bool,
    },
    /// Stop a deployment building in the install's builder.
    Cancel { id: String },
    /// Build DIR into the folder --out, ready for `forkrail publish`: deploy's build, without
    /// publishing. With --request nothing is read from AWS, so it runs anywhere.
    Build {
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// The folder to build into: empty, or not there yet.
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        prod: bool,
        /// Defaults to the directory's name.
        #[arg(long)]
        project: Option<String>,
        /// What to build, as JSON: id, project, target, base_domain and the env vars, instead of
        /// reading them from the install.
        #[arg(long, conflicts_with_all = ["prod", "project"])]
        request: Option<PathBuf>,
        /// The runtime binary for Linux arm64.
        #[arg(long, env = "FORKRAIL_BOOTSTRAP")]
        bootstrap: Option<PathBuf>,
        /// Python 3.13, for pip and the bytecode.
        #[arg(long, env = "FORKRAIL_PYTHON", default_value = "python3.13")]
        python: String,
        /// Install the Python requirements again, even if they're cached.
        #[arg(long)]
        no_cache: bool,
        /// For a framework: take its existing build output instead of building.
        #[arg(long)]
        prebuilt: bool,
    },
    /// Publish a folder `forkrail build` made: the deployment it was built for.
    Publish {
        out: PathBuf,
        /// As deploy's --memory.
        #[arg(long, value_parser = clap::value_parser!(u32).range(128..=10_240))]
        memory: Option<u32>,
        /// As deploy's --concurrent (also `--fluid`).
        #[arg(long, alias = "fluid", value_name = "BOOL", action = clap::ArgAction::Set)]
        concurrent: Option<bool>,
        /// As deploy's --same-source.
        #[arg(long)]
        same_source: bool,
        /// For a production build: publish it without putting it live (as deploy's).
        #[arg(long)]
        skip_domain: bool,
    },
    /// Show or change a project's settings: the function settings its next deployments use, the
    /// IAM role its functions run as, and the private targets its apps reach through the router.
    /// Changes take effect on the next deploy or redeploy, as on Vercel.
    Project {
        /// The project, by its name or the one it had first.
        project: String,
        /// Call the project NAME: the dashboard and the terminal show it, and take it wherever a
        /// project is named. Its addresses, functions and logs keep the name it had first; that
        /// name back undoes it.
        #[arg(long, value_name = "NAME")]
        rename: Option<String>,
        /// Memory in MB for its next deployments (128-10240), or `default`. A deployment's
        /// vercel.json wins over it, and deploy's --memory over both.
        #[arg(long, value_name = "MB|default", value_parser = parse_setting::<u32>)]
        memory: Option<Setting<u32>>,
        /// How long a request may run, in seconds (1-900), or `default`. vercel.json's maxDuration
        /// wins over it.
        #[arg(long, value_name = "SECONDS|default", value_parser = parse_setting::<u64>)]
        max_duration: Option<Setting<u64>>,
        /// Concurrent compute (true) or one request per worker (false), or `default`. vercel.json's
        /// `fluid` wins over it, and deploy's --concurrent over both. Also `--fluid`.
        #[arg(long, alias = "fluid", value_name = "BOOL|default", value_parser = parse_setting::<bool>)]
        concurrent: Option<Setting<bool>>,
        /// With concurrent compute on, the most requests one worker takes at once (2-50), or
        /// `default` (the router's, 10). Lower keeps fewer requests on each environment and its
        /// outbound IP; more workers start as traffic needs. Applies within seconds to its
        /// deployments that run concurrent compute, without a redeploy.
        #[arg(long, value_name = "N|default", value_parser = parse_setting::<u32>)]
        max_inflight: Option<Setting<u32>>,
        /// The regions its functions run in, comma-separated, by AWS's or Vercel's names
        /// (`us-east-1,eu-central-1` or `iad1,fra1`), or `default` for none: where production runs
        /// now, the home region for a new project. The router nearest each visitor runs the nearest
        /// of them; no router is needed there. vercel.json's `regions` wins over it.
        #[arg(long, value_delimiter = ',', value_name = "REGIONS|default")]
        regions: Option<Vec<String>>,
        /// An IAM role by name or ARN, so the app can call AWS without keys in its env vars, or
        /// `default` for the shared forkrail-worker role.
        #[arg(long)]
        role: Option<String>,
        /// Egress targets, comma-separated (`analytics`), or `none`. Each router maps names to
        /// addresses it reaches (FORKRAIL_EGRESS_TARGETS); the app gets FORKRAIL_EGRESS_<NAME>=host:port.
        #[arg(long, value_delimiter = ',')]
        egress: Option<Vec<String>>,
        /// Its framework preset, by Vercel's name for it (`flask`, `django`, `express`, `hono`,
        /// `go`, `rust`, `vite`, …), or `auto` to find it from the project's files. vercel.json's
        /// `framework` wins over it.
        #[arg(long, value_name = "PRESET|auto")]
        framework: Option<String>,
        /// "Auto-assign custom domains": false makes every production deployment wait for
        /// `forkrail promote` instead of going live; true or `default` puts them live as they're
        /// published.
        #[arg(long, value_name = "BOOL|default", value_parser = parse_setting::<bool>)]
        auto_assign_domains: Option<Setting<bool>>,
        /// Skew protection: `on` serves a request that names one of the project's deployments
        /// (`?dpl=`, `x-deployment-id`, the `__vdpl` cookie) by that deployment while it's younger
        /// than the maximum age, so open pages keep working after a deploy; `off`. Applies at once;
        /// builds and apps are told (VERCEL_SKEW_PROTECTION_ENABLED) from the next deploy.
        #[arg(long, value_name = "on|off", value_parser = parse_on_off)]
        skew_protection: Option<bool>,
        /// How long after its creation a deployment can still serve such requests: `30m`, `12h`,
        /// `7d`, or `default` (12 hours).
        #[arg(long, value_name = "AGE|default", value_parser = parse_skew_age)]
        skew_max_age: Option<Setting<u64>>,
        /// Stop requests reaching this deployment and every one created before it (once a fix
        /// for a bad deployment is out), or `none`.
        #[arg(long, value_name = "ID|none")]
        skew_threshold: Option<String>,
        /// Sites whose pages may send skew-protected requests from elsewhere, comma-separated
        /// (`app.example.com`, `*.example.com` one label deep, `*` for any), at most 12, or `none`.
        #[arg(long, value_delimiter = ',', value_name = "DOMAINS|none")]
        skew_allowed_domains: Option<Vec<String>>,
        /// Environments the routers keep ready per region for its production deployment, so a
        /// request after a quiet spell doesn't wait for a cold start: 0 keeps none, `auto` as many
        /// as it had in use at once at its busiest in the last day (at least 1), `default` is 1.
        /// Applies within seconds, without a redeploy.
        #[arg(long, value_name = "N|auto|default", value_parser = parse_setting::<forkrail_common::routes::KeepWarm>)]
        keep_warm: Option<Setting<forkrail_common::routes::KeepWarm>>,
        /// Web Analytics: `on` counts page views and events from pages that load the script
        /// (`@vercel/analytics`, `@forkrail/analytics` or a script tag); `off`, the default, keeps
        /// nothing. Applies within seconds, without a redeploy.
        #[arg(long, value_name = "on|off", value_parser = parse_on_off)]
        analytics: Option<bool>,
    },
    /// Make deployment ID production. A preview keeps its preview env vars, so one whose preview
    /// env differs from production's is refused; `forkrail redeploy ID --prod` publishes its build
    /// with production's instead.
    Promote {
        id: String,
        /// Promote a preview even though its env vars differ from production's.
        #[arg(long)]
        allow_preview_env: bool,
    },
    /// Make deployment ID again as a new deployment, with the project's env vars and settings as
    /// they are now: built again from its source if it was built in the builder (--remote), else
    /// its build published again.
    Redeploy {
        id: String,
        /// A production deployment, whatever ID is; by default the same kind as ID.
        #[arg(long)]
        prod: bool,
        /// With a production deployment: leave production where it is, as deploy's --skip-domain.
        #[arg(long)]
        skip_domain: bool,
        /// When it's built again: without the project's build cache.
        #[arg(long)]
        no_cache: bool,
    },
    /// Point production back at the previous production deployment, or at --to.
    Rollback {
        /// Defaults to the current directory's name.
        project: Option<String>,
        /// A deployment that has been production before.
        #[arg(long)]
        to: Option<String>,
    },
    /// List projects, or one project's deployments.
    Ls { project: Option<String> },
    /// Check that the account has a forkrail install, then save the profile and home region used
    /// (--profile, --region) so later commands needn't name them.
    Link,
    /// Show a deployment's record.
    Inspect {
        id: String,
        /// Its build log instead, once it has ended.
        #[arg(long)]
        logs: bool,
    },
    /// Show or set the dashboard's address. Builds send their log lines there as they run, so
    /// the dashboard shows them live; FORKRAIL_DASHBOARD_URL overrides it for one command.
    Dashboard {
        /// `https://app.example.com`, or `none`.
        url: Option<String>,
    },
    /// Remove a deployment that isn't production; with --project, delete a whole project: its
    /// deployments, domains, schedules, env vars, functions in every region and stored files. It
    /// stops being served at once, and it can't be undone.
    Rm {
        /// The deployment's id.
        #[arg(required_unless_present = "project", conflicts_with = "project")]
        id: Option<String>,
        /// Delete this project.
        #[arg(long)]
        project: Option<String>,
        /// With --project: don't ask to type its name first.
        #[arg(long)]
        yes: bool,
    },
    /// How long deployments are kept, as on Vercel: a period for canceled, errored, preview and
    /// production deployments (30 days each unless set), per project over the install's default.
    /// Some are kept whatever their age: production, the 10 newest, the 20 newest ready of each
    /// target, rollback targets, the newest failed, branches' latest. Deleted deployments can be
    /// restored for up to 30 days. Without a subcommand: the policy in force (--project: and what
    /// it does with each deployment).
    Retention {
        #[command(subcommand)]
        command: Option<RetentionCommand>,
        #[arg(long)]
        project: Option<String>,
    },
    /// Bring a deleted deployment back, exactly as it was, while it can still be restored
    /// (`forkrail retention --project NAME` lists them).
    Restore { id: String },
    /// List the stored packages, sources and uploads that no deployment uses any more, in every
    /// region's bucket, and (for the whole install) the static files, manifests, routing programs,
    /// bulk redirects and firewall configurations nothing names; with --yes, delete them.
    /// Deployments removed take their own files with them: this is for what's left besides.
    Sweep {
        /// Only this project's.
        project: Option<String>,
        /// Delete what's listed.
        #[arg(long)]
        yes: bool,
    },
    /// A project's env vars: production's and previews' (used by deployments made after a change,
    /// never read back) and Development's (for running the app here: `forkrail env pull`). Values
    /// come from stdin, or a prompt, and are never printed.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Show or set a project's limits. A new value applies to its deployments within seconds, no
    /// redeploy needed, and to later deploys.
    Limits {
        project: String,
        /// Workers at once per deployment (1-1000), or `default` for no cap of its own: as many
        /// as the account's Lambda concurrency allows. A worker takes up to 10 requests, or one
        /// with concurrent compute off.
        #[arg(long, value_name = "N|default", value_parser = parse_max_workers)]
        max_workers: Option<MaxWorkers>,
    },
    /// The install's GitHub App: how people sign in to the dashboard, and how a project is
    /// connected to a repository so that every push deploys.
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },
    /// The GitHub accounts that own this install. They can always sign in to the dashboard as
    /// owners, whatever its members list says, so this is also the way back in.
    Owners {
        #[command(subcommand)]
        command: OwnersCommand,
    },
    /// A project's own bulk redirects, run before anything else on every request to any of its
    /// deployments. Changes are staged until `publish`; no redeploy needed.
    Redirects {
        #[command(subcommand)]
        command: forkrail_cli::routing_cli::RedirectsCommand,
    },
    /// A project's own routing rules (rewrites, redirects, statuses, header and query changes),
    /// run in front of its deployments' own. Changes are staged until `publish`; no redeploy needed.
    Routes {
        #[command(subcommand)]
        command: forkrail_cli::routing_cli::RoutesCommand,
    },
    /// Custom domains, which serve a project's production deployment.
    Domains {
        #[command(subcommand)]
        command: DomainsCommand,
    },
    /// A project's schedules (`crons` and `schedules` in vercel.json), which call its production
    /// deployment on a cron expression: list, run now, turn off and on.
    #[command(alias = "crons")]
    Schedules {
        #[command(subcommand)]
        command: SchedulesCommand,
    },
    /// A project's deploy hooks: secret URLs that build a branch of its connected repository when
    /// called (a CMS that publishes, a cron service). A URL is shown once, when it's made.
    #[command(name = "deploy-hooks", alias = "hooks")]
    DeployHooks {
        #[command(subcommand)]
        command: DeployHooksCommand,
    },
    /// Spend protection: the install's monthly budget, projects' own, and what's paused.
    Spend {
        #[command(subcommand)]
        command: forkrail_cli::spend_cli::SpendCommand,
    },
    /// Who may open a project's deployments: sign-in, password, trusted IPs, bypass secrets.
    Protection {
        #[command(subcommand)]
        command: forkrail_cli::protection_cli::ProtectionCommand,
    },
    /// A project's firewall (or the install's, with --install): custom rules and IP blocks,
    /// drafted, then published.
    Firewall {
        #[command(subcommand)]
        command: forkrail_cli::firewall_cli::FirewallCommand,
    },
    /// A project's response cache: invalidate or delete what it kept by tag, or purge it all
    /// (Vercel's `vercel cache`).
    Cache {
        #[command(subcommand)]
        command: forkrail_cli::purge::CacheCommand,
    },
    /// curl a protected deployment, let in as a team member: `forkrail curl URL -- -i`.
    Curl {
        url: String,
        /// curl's own arguments, after `--`.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Regions with a router besides the home region (--region), and where projects' functions
    /// run. A project picks its own function regions: `forkrail project NAME --regions`.
    Regions {
        #[command(subcommand)]
        command: RegionsCommand,
    },
    /// Where project files and deployment records are kept: the bucket, or the install's table
    /// (deploy/install/state.yaml), and moving them from one to the other.
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
    /// A project's requests, newest last, from the analytics store: time (UTC), status, ms, the
    /// router's region, path, how many app lines each request wrote, and its id. With --app, the
    /// app's output instead; with --request, one request in full with its output.
    Logs {
        project: String,
        /// How far back: 30s, 15m, 2h, 7d, or a UTC time (2026-11-08 14:00).
        #[arg(long, default_value = "1h")]
        since: String,
        /// Up to (UTC time or age); default now.
        #[arg(long)]
        until: Option<String>,
        /// A status class (2xx, 4xx, 5xx) or code (404).
        #[arg(long)]
        status: Option<String>,
        /// Paths starting with this.
        #[arg(long)]
        path: Option<String>,
        /// One hostname.
        #[arg(long)]
        host: Option<String>,
        /// The newest N rows.
        #[arg(long, short = 'n', default_value_t = 100)]
        limit: u32,
        /// Keep printing new rows as they come (every 2 seconds).
        #[arg(long, short)]
        follow: bool,
        /// One request by its id (x-forkrail-request-id), with its invocation and app output.
        #[arg(long)]
        request: Option<String>,
        /// The app's stdout and stderr instead of requests.
        #[arg(long)]
        app: bool,
        /// With --app: lines containing this (any case).
        #[arg(long)]
        search: Option<String>,
        /// With --app: lines of this level (error, warning, info).
        #[arg(long)]
        level: Option<String>,
    },
    /// A project's traffic: requests by status, latency, workers and cold starts, billed time,
    /// regions, by hour (or day), and the busiest paths. Public HTTPS requests only.
    Stats {
        project: String,
        /// How far back: 2h, 24h, 7d, or a UTC time.
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// What each project used and about what it cost on AWS, against an estimate of Vercel's price
    /// for the same traffic. Public HTTPS requests only.
    Usage {
        /// One project; default all.
        project: Option<String>,
        /// How far back: 24h, 7d, 30d, or a UTC time.
        #[arg(long, default_value = "30d")]
        since: String,
        /// One row per day instead of per project.
        #[arg(long)]
        daily: bool,
    },
    /// Each router instance: CPU, memory, disk, network, connections and requests, and what the
    /// router and Vector themselves use. From the analytics store.
    Routers {
        /// How far back: 15m, 1h, 1d, or a UTC time.
        #[arg(long, default_value = "15m")]
        since: String,
    },
    /// Copy each deployment's environment, runtime, entry and creation time from its record into
    /// the routing table, for entries made before the table had them. Run once; deploys write them.
    #[command(hide = true)]
    FillRouteDetails,
    /// Give each project without function settings its production deployment's memory,
    /// maxDuration and mode, so its next deploys keep them. Run once; safe to run again.
    #[command(hide = true)]
    FillProjectSettings,
}

/// A project setting's new value, or `default` to clear it.
#[derive(Clone, Copy, Debug)]
struct Setting<T>(Option<T>);

fn parse_on_off(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" => Ok(true),
        "off" | "false" => Ok(false),
        _ => Err(format!("{value:?}: on or off")),
    }
}

fn parse_skew_age(value: &str) -> Result<Setting<u64>, String> {
    forkrail_cli::skew::parse_age(value).map(Setting)
}

/// The skew protection flags of `forkrail project` as a change.
fn skew_change(enabled: Option<bool>, max_age: Option<Setting<u64>>, threshold: Option<String>, domains: Option<Vec<String>>) -> forkrail_cli::skew::Change {
    forkrail_cli::skew::Change {
        enabled,
        max_age_s: max_age.map(|s| s.0),
        threshold: threshold.map(|t| Some(t.trim().to_owned()).filter(|t| t != "none" && !t.is_empty())),
        allowed_domains: domains.map(|d| d.into_iter().map(|d| d.trim().to_owned()).filter(|d| !d.is_empty() && d != "none").collect()),
    }
}

fn parse_setting<T: std::str::FromStr>(value: &str) -> Result<Setting<T>, String> {
    if value == "default" {
        return Ok(Setting(None));
    }
    value.parse::<T>().map(|v| Setting(Some(v))).map_err(|_| format!("{value:?}: a value, or `default`"))
}

/// `--regions`: the names given, or `default` (None) for no setting: where production runs.
fn regions_setting(names: Vec<String>) -> Option<Vec<String>> {
    let names: Vec<String> = names.into_iter().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).collect();
    (!(names.is_empty() || names.iter().any(|n| n == "default"))).then_some(names)
}

#[derive(Subcommand)]
enum RegionsCommand {
    /// The regions with a router or functions, how many deployments have a version in each, and
    /// how many projects pick each.
    Ls,
    /// A router runs in REGION: send it the files (static files, routing rules) of every
    /// deployment production may use (production and its rollback targets), and of every deploy
    /// from now on. Functions run there for projects that pick it. Run it again to retry any that
    /// failed.
    Add {
        // Not `region`, which would clash with the global --region (the home region).
        #[arg(value_name = "REGION")]
        name: String,
        /// Previews too.
        #[arg(long)]
        all: bool,
    },
    /// No router in REGION any more: stop sending it deployments' files. Projects that pick it
    /// keep their functions there.
    Rm {
        #[arg(value_name = "REGION")]
        name: String,
    },
    /// Make REGION ready for functions: its code bucket and env var key. Picking it for a project
    /// (in the dashboard, or `forkrail project NAME --regions`) does the same.
    Setup {
        #[arg(value_name = "REGION")]
        name: String,
    },
    /// Publish just the deployments ID... in REGION, functions and files, whatever regions their
    /// project picks: a preview to try there, say.
    Copy {
        #[arg(value_name = "REGION")]
        name: String,
        #[arg(required = true)]
        ids: Vec<String>,
    },
}

#[derive(Subcommand)]
enum GitCommand {
    /// Create the install's GitHub App. Your browser opens GitHub with the app filled in; you
    /// click Create, and its keys are stored in SSM. You become the install's first owner.
    Setup {
        /// Create the app under this organization instead of your own account.
        #[arg(long)]
        org: Option<String>,
        /// The install's first owner, by GitHub username. Needed with --org; otherwise it's you.
        #[arg(long)]
        owner: Option<String>,
        /// The app's name on GitHub; defaults to "forkrail (<base domain>)".
        #[arg(long)]
        name: Option<String>,
        /// Another address the dashboard is, or will be, served at (repeatable), such as
        /// https://app.example.com: GitHub sends people back only to addresses the app lists.
        #[arg(long = "callback", value_name = "URL")]
        callbacks: Vec<String>,
        /// Make a new app although the install has one.
        #[arg(long)]
        replace: bool,
        /// Store the app `forkrail install` made earlier, kept in FILE, instead of making one.
        #[arg(long, hide = true, value_name = "FILE")]
        from: Option<PathBuf>,
    },
    /// Show the app, where its keys are kept, the accounts it's installed on, and the install's
    /// owners.
    Status,
    /// List the repositories the app reaches (the ones it was given on GitHub), the most recently
    /// pushed first.
    Repos {
        /// Only repositories whose name has these words.
        query: Option<String>,
    },
    /// Connect PROJECT to a GitHub repository: from then on every push deploys, the production
    /// branch to production and any other branch to a preview.
    Connect {
        project: String,
        /// OWNER/NAME, one of `forkrail git repos`.
        repo: String,
        /// The branch whose pushes go to production; the repository's default branch if not given.
        #[arg(long)]
        branch: Option<String>,
        /// The project's folder in the repository, if not its top (a monorepo).
        #[arg(long, value_name = "DIR")]
        root: Option<String>,
        /// Make PROJECT from the repository: a new project, connected, with its production
        /// branch deployed at once. Refused if PROJECT exists.
        #[arg(long)]
        create: bool,
    },
    /// Stop deploying PROJECT from its repository. Its deployments stay.
    Disconnect { project: String },
    /// Deploy PROJECT from its connected repository now, as a push would: the newest commit of
    /// its production branch, or of --branch.
    Deploy {
        project: String,
        #[arg(long)]
        branch: Option<String>,
    },
}

#[derive(Subcommand)]
enum StateCommand {
    /// Where they're kept now, and how many there are in the bucket and in the table.
    Status,
    /// Keep them in MODE from now on: `s3` (the bucket only), `mirror` (the bucket is read, and
    /// every write is copied to the table) or `dynamodb` (the table is read; the bucket keeps a
    /// copy of every write unless --no-s3-copy). Refused when a change would lose writes.
    Mode {
        mode: forkrail_cli::table::Mode,
        /// With `dynamodb`: stop copying writes to the bucket. Going back is then refused.
        #[arg(long)]
        no_s3_copy: bool,
        /// Change it even though the bucket and the table differ.
        #[arg(long)]
        force: bool,
    },
    /// Copy every project file and deployment record the table doesn't have yet (in `mirror`).
    Migrate,
    /// Compare the bucket with the table; --repair makes the one not read like the one read.
    Check {
        #[arg(long)]
        repair: bool,
    },
}

#[derive(Subcommand)]
enum OwnersCommand {
    /// List the install's owners.
    Ls,
    /// Make a GitHub account an owner, by username.
    Add { login: String },
    /// Stop a GitHub account being one of the install's owners. The last one stays.
    Rm { login: String },
}

#[derive(Subcommand)]
enum DomainsCommand {
    /// Serve DOMAIN (`api.example.com`, `example.com` or `*.example.com`) from PROJECT's production
    /// deployment, a branch's newest preview, or redirect it to another of PROJECT's domains. The
    /// router gets it a certificate: it prints the DNS records that make that work.
    Add {
        domain: String,
        /// The project; also as `--project`.
        #[arg(required_unless_present = "project_flag")]
        project: Option<String>,
        #[arg(long = "project", conflicts_with = "project")]
        project_flag: Option<String>,
        /// Redirect every request to this domain of the project, path and query kept.
        #[arg(long)]
        redirect: Option<String>,
        /// The redirect's status: 301, 302, 307 or 308.
        #[arg(long, default_value_t = 308, requires = "redirect")]
        status: u16,
        /// Follow this git branch's newest ready preview instead of production.
        #[arg(long, conflicts_with = "redirect")]
        branch: Option<String>,
    },
    /// Change what DOMAIN does: redirect it, follow a branch, or (`--production`) serve production
    /// again.
    Update {
        domain: String,
        #[arg(long)]
        redirect: Option<String>,
        #[arg(long, default_value_t = 308, requires = "redirect")]
        status: u16,
        #[arg(long, conflicts_with = "redirect")]
        branch: Option<String>,
        /// Neither a redirect nor a branch: serve production.
        #[arg(long, conflicts_with_all = ["redirect", "branch"])]
        production: bool,
    },
    /// Stop serving DOMAIN.
    Rm { domain: String },
    /// List custom domains, and how each one's certificate stands.
    Ls { project: Option<String> },
    /// One domain: what it serves, its certificate, and the DNS records it needs.
    Inspect { domain: String },
    /// Have the router look at DOMAIN's DNS and certificate again now, instead of at its next try.
    Check { domain: String },
}

#[derive(Subcommand)]
enum RetentionCommand {
    /// The policy in force; with --project, what it does with each deployment and why, and the
    /// deleted ones.
    Show {
        #[arg(long)]
        project: Option<String>,
    },
    /// Change --project's periods, or the install's default without it. Each is like 30d, 2w,
    /// 6m, 1y, `unlimited`, or `default` for what's under it (the install's, then 30 days). What
    /// the new policy would delete is listed first.
    Set {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, value_name = "PERIOD|default")]
        canceled: Option<String>,
        #[arg(long, value_name = "PERIOD|default")]
        errored: Option<String>,
        #[arg(long, value_name = "PERIOD|default")]
        preview: Option<String>,
        #[arg(long, value_name = "PERIOD|default")]
        production: Option<String>,
        /// Clear the periods first: a project's back to the install's, the install's to 30 days.
        #[arg(long)]
        reset: bool,
        /// Only list what the new policy would delete; nothing is saved.
        #[arg(long)]
        dry_run: bool,
        /// Save it without asking, even if it would delete deployments.
        #[arg(long)]
        yes: bool,
    },
    /// Run the policy now, on --project or every project: lists what it deletes; with --yes deletes
    /// it (restorably) and deletes for good the deleted deployments whose time is up.
    Sweep {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        yes: bool,
    },
}

/// One retention op, through the install or its API.
async fn retention_ask(via: &DomainsVia<'_>, op: forkrail_cli::retention::RetentionOp) -> Result<serde_json::Value> {
    match via {
        DomainsVia::Aws(ctx) => forkrail_cli::retention::run(ctx, op, None).await,
        DomainsVia::Api(client) => {
            let changes = op.changes();
            let body = serde_json::to_value(&op)?;
            if changes { client.post("retention", body).await } else { client.ask("retention", body).await }
        }
    }
}

/// A period as typed: `default` goes back to what's under it.
fn period_arg(value: Option<String>) -> Result<Option<Option<forkrail_cli::retention::Period>>> {
    value.map(|v| if v.trim() == "default" { Ok(None) } else { forkrail_cli::retention::Period::parse(&v).map(Some) }).transpose()
}

/// `forkrail retention …`, through the install or its API.
async fn retention_command(via: DomainsVia<'_>, command: Option<RetentionCommand>, project: Option<String>) -> Result<()> {
    use forkrail_cli::retention::{self, InstallView, PolicyChange, ProjectView, RetentionOp, Swept};
    let show = |answer: serde_json::Value, project: bool| -> Result<()> {
        if project {
            retention::print_view(&serde_json::from_value::<ProjectView>(answer).context("the policy, as the install gave it")?);
        } else {
            retention::print_install(&serde_json::from_value::<InstallView>(answer).context("the policy, as the install gave it")?);
        }
        Ok(())
    };
    match command.unwrap_or(RetentionCommand::Show { project }) {
        RetentionCommand::Show { project } => {
            let one = project.is_some();
            show(retention_ask(&via, RetentionOp::Get { project }).await?, one)
        }
        RetentionCommand::Set { project, canceled, errored, preview, production, reset, dry_run, yes } => {
            let change = PolicyChange { canceled: period_arg(canceled)?, errored: period_arg(errored)?, preview: period_arg(preview)?, production: period_arg(production)? };
            if change.is_empty() && !reset {
                bail!("say what changes: --canceled, --errored, --preview or --production (a period like 30d, 6m, 1y, unlimited, or default), or --reset");
            }
            let one = project.is_some();
            let plan = retention_ask(&via, RetentionOp::Plan { project: project.clone(), change: change.clone(), reset }).await?;
            let doomed: usize = if one {
                serde_json::from_value::<ProjectView>(plan.clone()).map(|v| v.doomed().len()).unwrap_or(0)
            } else {
                serde_json::from_value::<InstallView>(plan.clone()).map(|v| v.projects.iter().map(|p| p.delete.len()).sum()).unwrap_or(0)
            };
            show(plan, one)?;
            if dry_run {
                eprintln!("==> nothing saved (--dry-run)");
                return Ok(());
            }
            if doomed > 0 && !yes {
                if !std::io::stdin().is_terminal() {
                    bail!("this policy deletes {doomed} deployment(s) at its next run: add --yes to save it");
                }
                eprint!("Save it? Its next run (the next deploy, or the daily sweep) deletes {doomed} deployment(s), each restorable for a while. [y/N] ");
                let mut typed = String::new();
                std::io::stdin().read_line(&mut typed)?;
                if !matches!(typed.trim(), "y" | "Y" | "yes") {
                    bail!("not saved");
                }
            }
            let saved = retention_ask(&via, RetentionOp::Set { project, change, reset }).await?;
            eprintln!("==> saved");
            show(saved, one)
        }
        RetentionCommand::Sweep { project, yes } => {
            let answer = retention_ask(&via, RetentionOp::Sweep { project, dry_run: !yes }).await?;
            retention::print_swept(&serde_json::from_value::<Vec<Swept>>(answer).context("the sweep, as the install gave it")?);
            Ok(())
        }
    }
}

/// `forkrail restore ID`, through the install or its API.
async fn restore_command(via: DomainsVia<'_>, id: String) -> Result<()> {
    let answer = retention_ask(&via, forkrail_cli::retention::RetentionOp::Restore { id }).await?;
    let restored: forkrail_cli::retention::Restored = serde_json::from_value(answer).context("the restore, as the install gave it")?;
    let at = if restored.hosts.is_empty() { String::new() } else { format!(": {}", restored.hosts.join(", ")) };
    eprintln!("==> {}: {} is back{at}", restored.project, restored.id);
    Ok(())
}

#[derive(Subcommand)]
enum SchedulesCommand {
    /// PROJECT's schedules, from its production deployment: on or off, and their next runs.
    Ls { project: String },
    /// Run schedule NAME (its name, a cron's key from `ls`) on production now, whatever its state.
    Run { project: String, name: String },
    /// Turn schedule NAME back on, or with `--all` every one ("Enable all").
    Enable {
        project: String,
        #[arg(required_unless_present = "all")]
        name: Option<String>,
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
    /// Turn schedule NAME off, or with `--all` every one; it stays off across deploys and rollbacks.
    Disable {
        project: String,
        #[arg(required_unless_present = "all")]
        name: Option<String>,
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
    /// Write the scheduler again from production and the states, after a failed update.
    Sync { project: String },
}

#[derive(Subcommand)]
enum DeployHooksCommand {
    /// PROJECT's deploy hooks: names, branches, who made them (never their URLs).
    Ls { project: String },
    /// A new hook NAME that builds BRANCH (the production branch if not given); prints its URL,
    /// the only time it's shown.
    Create {
        project: String,
        name: String,
        #[arg(long, visible_alias = "ref")]
        branch: Option<String>,
    },
    /// Remove hook ID (from `ls`): its URL stops working at once.
    Rm { project: String, id: String },
}

/// `forkrail deploy-hooks …`, through the install or its API.
async fn deploy_hooks_command(via: DomainsVia<'_>, command: DeployHooksCommand) -> Result<()> {
    use forkrail_cli::deploy_hooks::{self, DeployHooksOp};
    let op = match command {
        DeployHooksCommand::Ls { project } => DeployHooksOp::List { project },
        DeployHooksCommand::Create { project, name, branch } => DeployHooksOp::Create { project, name, branch },
        DeployHooksCommand::Rm { project, id } => DeployHooksOp::Remove { project, id },
    };
    let (name, project) = (op.name(), op.project().to_owned());
    let answer: serde_json::Value = match via {
        DomainsVia::Aws(ctx) => deploy_hooks::run(ctx, op, git::deployer(&std::env::current_dir()?).as_deref()).await?,
        DomainsVia::Api(client) => {
            let path = format!("projects/{project}/deploy-hooks");
            match op {
                DeployHooksOp::List { .. } => client.get(&path).await?,
                DeployHooksOp::Create { name, branch, .. } => client.post(&path, serde_json::json!({"op": "create", "name": name, "branch": branch})).await?,
                DeployHooksOp::Remove { id, .. } => client.post(&path, serde_json::json!({"op": "remove", "id": id})).await?,
            }
        }
    };
    match name {
        "list" => {
            let hooks = answer["hooks"].as_array().cloned().unwrap_or_default();
            if answer["connected"] == false {
                eprintln!("{project} has no connected repository: a deploy hook builds a branch of one (`forkrail git connect`)");
            }
            if hooks.is_empty() {
                eprintln!("{project} has no deploy hooks");
                return Ok(());
            }
            println!("{:<14} {:<24} {:<20} {:<22} BY", "ID", "NAME", "BRANCH", "CREATED");
            for h in hooks {
                let s = |k: &str| h[k].as_str().unwrap_or("-").to_owned();
                println!("{:<14} {:<24} {:<20} {:<22} {}", s("id"), s("name"), s("branch"), s("created"), s("created_by"));
            }
        }
        "create" => {
            let hook = &answer["hook"];
            eprintln!("==> deploy hook {} ({}) builds {} of {project}. Anyone with this URL can deploy: keep it secret; it isn't shown again.", hook["name"].as_str().unwrap_or("?"), hook["id"].as_str().unwrap_or("?"), hook["branch"].as_str().unwrap_or("?"));
            eprintln!("    Call it with GET or POST; add ?buildCache=false to build without the build cache. 60 calls an hour per project.");
            match answer["url"].as_str() {
                Some(url) => println!("{url}"),
                None => {
                    eprintln!("note: the install has no dashboard address (`forkrail dashboard URL`), and the dashboard is what answers hooks: the URL is <dashboard>{}/{project}/<secret>", deploy_hooks::PATH);
                    println!("{}", answer["secret"].as_str().unwrap_or_default());
                }
            }
        }
        _ => eprintln!("==> removed deploy hook {} of {project}", answer["id"].as_str().unwrap_or("?")),
    }
    Ok(())
}

/// `forkrail schedules …`, through the install or its API.
async fn schedules_command(via: DomainsVia<'_>, command: SchedulesCommand) -> Result<()> {
    use forkrail_cli::schedules::{self, SchedulesOp};
    let op = match command {
        SchedulesCommand::Ls { project } => SchedulesOp::List { project },
        SchedulesCommand::Run { project, name } => SchedulesOp::Run { project, key: name },
        SchedulesCommand::Enable { project, name, all: _ } => SchedulesOp::Enable { project, key: name },
        SchedulesCommand::Disable { project, name, all: _ } => SchedulesOp::Disable { project, key: name },
        SchedulesCommand::Sync { project } => SchedulesOp::Sync { project },
    };
    let (name, changes) = (op.name(), op.changes());
    let answer: serde_json::Value = match via {
        DomainsVia::Aws(ctx) => schedules::run(ctx, op, None).await?,
        DomainsVia::Api(client) => {
            let body = serde_json::to_value(&op)?;
            if changes { client.post("schedules", body).await? } else { client.ask("schedules", body).await? }
        }
    };
    if name == "run" {
        let started: schedules::Started = serde_json::from_value(answer).context("the run, as the install gave it")?;
        eprintln!("==> {} of {}: run {} sent; `forkrail logs {} --since 5m` shows it within seconds", started.key, started.project, started.run, started.project);
        return Ok(());
    }
    let view = if name == "sync" { answer["schedules"].clone() } else { answer };
    let view: schedules::View = serde_json::from_value(view).context("the schedules, as the install gave them")?;
    schedules::print_view(&view);
    Ok(())
}

impl DomainsCommand {
    /// The op it asks for (domains.rs), for the install or its API.
    fn op(self) -> Result<domains::DomainsOp> {
        use domains::DomainsOp;
        let redirect = |to: Option<String>, status: u16| to.map(|to| forkrail_common::routes::DomainRedirect { to: forkrail_common::routes::normalize_host(&to), status });
        Ok(match self {
            DomainsCommand::Add { domain, project, project_flag, redirect: to, status, branch } => DomainsOp::Add {
                domain,
                project: project.or(project_flag).context("name the project")?,
                redirect: redirect(to, status),
                branch,
            },
            DomainsCommand::Update { domain, redirect: to, status, branch, production } => match (redirect(to, status), branch) {
                (Some(r), _) => DomainsOp::Update { domain, redirect: Some(Some(r)), branch: Some(None) },
                (None, Some(b)) => DomainsOp::Update { domain, redirect: Some(None), branch: Some(Some(b)) },
                (None, None) if production => DomainsOp::Update { domain, redirect: Some(None), branch: Some(None) },
                (None, None) => bail!("say what changes: --redirect DOMAIN, --branch BRANCH or --production"),
            },
            DomainsCommand::Rm { domain } => DomainsOp::Remove { domain },
            DomainsCommand::Ls { project } => DomainsOp::List { project },
            DomainsCommand::Inspect { domain } => DomainsOp::Inspect { domain },
            DomainsCommand::Check { domain } => DomainsOp::Check { domain },
        })
    }
}

/// `forkrail domains …`, through the install or its API: the op, then what it answers, printed.
async fn domains_command(via: DomainsVia<'_>, command: DomainsCommand) -> Result<()> {
    let what = match &command {
        DomainsCommand::Add { .. } => "added",
        DomainsCommand::Update { .. } => "changed",
        DomainsCommand::Rm { .. } => "removed",
        DomainsCommand::Check { .. } => "asked to be checked again",
        _ => "",
    };
    let op = command.op()?;
    let changes = op.changes();
    let listing = matches!(op, domains::DomainsOp::List { .. });
    let answer = match via {
        DomainsVia::Aws(ctx) => domains::run(ctx, op, None).await?,
        DomainsVia::Api(client) => {
            let body = serde_json::to_value(&op)?;
            if changes { client.post("domains", body).await? } else { client.ask("domains", body).await? }
        }
    };
    if listing {
        let views: Vec<domains::View> = serde_json::from_value(answer["domains"].clone()).context("the domains, as the install gave them")?;
        domains::print_list(&views);
        return Ok(());
    }
    let view: domains::View = serde_json::from_value(answer).context("the domain, as the install gave it")?;
    if !what.is_empty() {
        eprintln!("==> {}: {what}", view.name);
    }
    if what == "removed" {
        eprintln!("==> the routers stop serving it within seconds; point its DNS elsewhere, or clients get 404 and then certificate errors");
        return Ok(());
    }
    domains::print_view(&view);
    if what == "asked to be checked again" {
        eprintln!("\n==> the router looks within seconds: `forkrail domains inspect {}` shows what it found", view.name);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum DomainsVia<'a> {
    Aws(&'a Ctx),
    Api(&'a api::Client),
}

#[derive(Subcommand)]
enum EnvCommand {
    /// Each environment's variable names, and each Git branch's own preview names.
    Ls {
        project: String,
        /// Only this Git branch's own preview names.
        #[arg(long)]
        git_branch: Option<String>,
    },
    /// Set KEY, its value from stdin (or typed at a prompt): for production and previews unless
    /// --target says otherwise. Production's and previews' take effect on the next deploy.
    Add {
        project: String,
        key: String,
        /// production, preview, development, or several comma-separated. Defaults to production
        /// and preview.
        #[arg(long, short = 'e', value_enum, value_delimiter = ',', alias = "environment")]
        target: Vec<TargetArg>,
        /// For this Git branch's previews only: it wins over preview's value of the same name
        /// there, and they keep every other preview value.
        #[arg(long)]
        git_branch: Option<String>,
    },
    /// Remove KEY, from every environment unless --target names some. A branch's own value goes
    /// only with --git-branch.
    Rm {
        project: String,
        key: String,
        #[arg(long, short = 'e', value_enum, value_delimiter = ',', alias = "environment")]
        target: Vec<TargetArg>,
        /// Remove this Git branch's own preview value instead.
        #[arg(long)]
        git_branch: Option<String>,
    },
    /// Write the Development variables (or --environment's) into FILE, for `next dev` and the
    /// like. A file that's there is merged: variables only in it are kept. Production's and
    /// previews' values can't be read back, so those come as names to fill in.
    Pull {
        /// Defaults to .env.local.
        file: Option<PathBuf>,
        /// Defaults to the folder's name, as deploy names it.
        #[arg(long)]
        project: Option<String>,
        /// Defaults to development, or to preview with --git-branch.
        #[arg(long, short = 'e', value_enum, alias = "target")]
        environment: Option<TargetArg>,
        /// What this Git branch's previews get: preview's variables and the branch's own.
        #[arg(long)]
        git_branch: Option<String>,
        /// Update a file that's there without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Run a command with the Development variables (or --environment's), writing no file:
    /// `forkrail env run -- next dev`. A variable already set here keeps its value.
    Run {
        #[arg(long)]
        project: Option<String>,
        /// Defaults to development, or to preview with --git-branch.
        #[arg(long, short = 'e', value_enum, alias = "target")]
        environment: Option<TargetArg>,
        /// What this Git branch's previews get: preview's variables and the branch's own.
        #[arg(long)]
        git_branch: Option<String>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Move env var values out of project files into SSM (one SecureString each, under the
    /// install's KMS key), and encrypt every project function's environment with that key. Safe
    /// to run again.
    Migrate {
        /// Only this project, and only its function.
        project: Option<String>,
    },
}

/// `--max-workers`: a number, or `default` to clear it.
#[derive(Clone, Copy, Debug)]
struct MaxWorkers(Option<u32>);

fn parse_max_workers(value: &str) -> Result<MaxWorkers, String> {
    if value == "default" {
        return Ok(MaxWorkers(None));
    }
    match value.parse::<u32>() {
        Ok(n @ 1..=1000) => Ok(MaxWorkers(Some(n))),
        _ => Err("a whole number from 1 to 1000, or `default`".into()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TargetArg {
    Production,
    Preview,
    Development,
}

impl From<TargetArg> for EnvTarget {
    fn from(target: TargetArg) -> Self {
        match target {
            TargetArg::Production => EnvTarget::Production,
            TargetArg::Preview => EnvTarget::Preview,
            TargetArg::Development => EnvTarget::Development,
        }
    }
}

/// The environments named, each once, or `default` when none is.
fn targets(named: &[TargetArg], default: &[EnvTarget]) -> Vec<EnvTarget> {
    if named.is_empty() {
        return default.to_vec();
    }
    let chosen: std::collections::BTreeSet<EnvTarget> = named.iter().map(|&t| t.into()).collect();
    chosen.into_iter().collect()
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse_from(deploy_by_default(std::env::args_os().collect()))).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// `forkrail --prod`, as `vercel --prod` is written: one of deploy's flags with no command before
/// it (after any of the global ones) is deploy's, so `deploy` goes in front of it. Anything else
/// is clap's to answer: a command, --help, --version, or an error.
fn deploy_by_default(mut args: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let cli = Cli::command();
    let Some(deploy) = cli.find_subcommand("deploy") else { return args };
    // The global flags, and whether each takes a value (`--profile NAME`, not `--aws`).
    let globals: Vec<(&str, bool)> = cli.get_arguments().filter_map(|a| Some((a.get_long()?, a.get_action().takes_values()))).collect();
    let mut i = 1;
    while let Some(arg) = args.get(i).and_then(|a| a.to_str()) {
        let Some(flag) = arg.strip_prefix("--") else { break };
        let name = flag.split_once('=').map_or(flag, |(name, _)| name);
        match globals.iter().find(|(long, _)| *long == name) {
            Some((_, takes_value)) => i += if *takes_value && !flag.contains('=') { 2 } else { 1 },
            None => {
                if deploy.get_arguments().any(|a| a.get_long() == Some(name)) {
                    args.insert(i, "deploy".into());
                }
                break;
            }
        }
    }
    args
}

async fn run(cli: Cli) -> Result<()> {
    // A build from a request file needs nothing from AWS, not even credentials.
    if let Command::Build { request: Some(request), dir, out, bootstrap, python, no_cache, prebuilt, .. } = cli.command {
        let text = std::fs::read(&request).with_context(|| format!("reading {}", request.display()))?;
        let request: build::Request = serde_json::from_slice(&text).with_context(|| format!("{} isn't a build request", request.display()))?;
        return build_into(dir, out, request, bootstrap, python, no_cache, prebuilt).await;
    }
    match &cli.command {
        Command::Login { url, no_browser } => return login::login(url.clone(), *no_browser).await,
        Command::Install { domain, email, check, yes, terminal, release } => {
            let options = install::Options {
                profile: cli.profile.clone(),
                region: cli.region.clone(),
                domain: domain.clone(),
                email: email.clone(),
                check_only: *check,
                yes: *yes,
                terminal: *terminal,
                release: release.clone(),
            };
            return install::install(options).await;
        }
        Command::Uninstall { yes } => {
            let options = install::uninstall::Options { profile: cli.profile.clone(), region: cli.region.clone(), yes: *yes };
            return install::uninstall::uninstall(options).await;
        }
        Command::Logout => return login::logout().await,
        _ => {}
    }
    // Signed in, the commands the install's API serves go through it, as that person: nothing
    // here then needs AWS credentials. An administrator's own credentials are used when asked for.
    let direct = cli.aws || cli.profile.is_some() || std::env::var("FORKRAIL_AWS").is_ok_and(|v| !v.is_empty() && v != "0");
    let signed_in = if direct { None } else { login::resolve(|name| std::env::var(name).ok(), login::load()?)? };
    if let Some(saved) = &signed_in
        && through_api(&cli.command)
    {
        return via_api(&api::Client::new(saved)?, cli.command).await;
    }
    let resolved = config::resolve(cli.profile, cli.region, |name| std::env::var(name).ok(), &config::load()?);
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(Region::new(resolved.region.clone()));
    if let Some(profile) = &resolved.profile {
        loader = loader.profile_name(profile);
    }
    // A browser sign-in (`aws login`, or the installer's) is refreshed where it began.
    if let Some(provider) = forkrail_cli::aws_signin::pinned(resolved.profile.as_deref()) {
        loader = loader.credentials_provider(provider);
    }
    let config = loader.load().await;
    let ctx = Ctx::connect(config, &resolved.region, resolved.bucket.as_deref()).await.map_err(|e| {
        let who = resolved.profile.as_deref().map_or_else(|| "the default AWS credentials".to_owned(), |p| format!("AWS profile {p}"));
        let hint = match (&signed_in, login::load().ok().flatten()) {
            (Some(_), _) => "\nthis command isn't served through a login yet: it needs the install's own AWS credentials",
            (None, None) if !direct => "\nwithout AWS credentials: `forkrail login https://<the install's dashboard>` signs this terminal in instead",
            _ => "",
        };
        anyhow::anyhow!("{who}: {e}\nsign in again (aws login, aws sso login), or name another profile with --profile; `forkrail link` saves it{hint}")
    })?;

    let mut command = cli.command;
    resolve_project_name(&ctx, &mut command).await?;
    match command {
        Command::Login { .. } | Command::Logout | Command::Install { .. } | Command::Uninstall { .. } => unreachable!("answered before AWS was asked"),
        Command::Whoami => {
            let Install { account, home_region, bucket, .. } = &ctx.install;
            let with = resolved.profile.as_deref().map_or_else(|| "the default AWS credentials".to_owned(), |p| format!("AWS profile {p}"));
            println!("AWS account {account} with {with}: the install in {home_region}, s3://{bucket}");
            match login::load()? {
                Some(saved) if direct => println!("also signed in to {} as {} (not used: AWS credentials were asked for)", saved.api, saved.login),
                Some(saved) => println!("also signed in to {} as {}", saved.api, saved.login),
                None => println!("not signed in to a dashboard (`forkrail login`)"),
            }
            Ok(())
        }
        Command::Link => link(&ctx, &resolved).await,
        // Built in the builder unless asked for this machine (--local), or given what only a
        // local build uses (--prebuilt, --bootstrap).
        Command::Deploy { dir, prod, project, memory, concurrent, same_source, skip_domain, no_cache, local: false, prebuilt: false, bootstrap: None, .. } => {
            remote::deploy(&ctx, remote::RemoteDeploy { dir, prod, project, memory, concurrent, same_source, skip_domain, no_cache }).await
        }
        Command::Deploy { dir, prod, project, memory, concurrent, bootstrap, python, no_cache, same_source, prebuilt, skip_domain, .. } => {
            let bootstrap = bootstrap.unwrap_or_else(default_bootstrap);
            deploy::deploy(&ctx, DeployArgs { dir, prod, project, memory, concurrent, bootstrap, python, no_cache, same_source, prebuilt, skip_domain }).await
        }
        Command::Cancel { id } => {
            let rec = remote::cancel(&ctx, &id).await?;
            eprintln!("==> {}: deployment {id} canceled; the builder stops it within seconds", rec.project);
            Ok(())
        }
        Command::Build { dir, out, prod, project, request: _, bootstrap, python, no_cache, prebuilt } => {
            let dir = dir.canonicalize().with_context(|| format!("{}", dir.display()))?;
            let project = match project {
                Some(name) => name,
                None => deploy::project_from_dir(&dir)?,
            };
            let target = if prod { Target::Production } else { Target::Preview };
            // The commit first: a preview of a branch gets the branch's own preview values.
            let git = forkrail_cli::git::commit(&dir);
            let dest = Destination::read(&ctx, &project, target, git.as_ref()).await?;
            let mut request = dest.request(&forkrail_common::random_hex(4));
            request.git = git;
            build_into(dir, out, request, bootstrap, python, no_cache, prebuilt).await
        }
        Command::Publish { out, memory, concurrent, same_source, skip_domain } => {
            let started = Instant::now();
            let built = Build::read(&out)?;
            let dest = Destination::read(&ctx, &built.project, built.target, built.git.as_ref()).await?;
            let opts = publish::Options { memory, concurrent, skip_domain, same_source, redeploy_of: None };
            publish::publish(&ctx, &dest, Files::Folder(&out), &built, &opts, started).await.map(|_| ())
        }
        Command::Project { project, rename, memory, max_duration, concurrent, max_inflight, regions, role, egress, framework, auto_assign_domains, skew_protection, skew_max_age, skew_threshold, skew_allowed_domains, keep_warm, analytics } => {
            let egress = egress.map(|names| {
                names.into_iter().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty() && n != "none").collect::<Vec<_>>()
            });
            let change = Change {
                memory_mb: memory.map(|s| s.0),
                max_duration_s: max_duration.map(|s| s.0),
                concurrent: concurrent.map(|s| s.0),
                max_inflight: max_inflight.map(|s| s.0),
                role: role.map(|r| Some(r).filter(|r| r != "default")),
                egress,
                framework: framework.map(|f| Some(f.trim().to_owned()).filter(|f| f != "auto" && !f.is_empty())),
                auto_assign_domains: auto_assign_domains.map(|s| s.0),
                skew: skew_change(skew_protection, skew_max_age, skew_threshold, skew_allowed_domains),
                keep_warm: keep_warm.map(|s| s.0),
                analytics,
                regions: regions.map(regions_setting),
            };
            let project = project::resolve(&ctx, &project).await?;
            if let Some(name) = rename {
                // It says what it did itself.
                project::rename(&ctx, &project, &name).await?;
                if change.is_empty() {
                    return Ok(());
                }
            }
            project_settings(&ctx, &project, &change).await
        }
        Command::Promote { id, allow_preview_env } => {
            let preview_env = if allow_preview_env { PreviewEnv::Keep } else { PreviewEnv::Refuse };
            deploy::promote(&ctx, &id, preview_env, None, Via::Cli).await.map(|_| ())
        }
        Command::Redeploy { id, prod, skip_domain, no_cache } => {
            let actor = git::deployer(&std::env::current_dir()?);
            let target = prod.then_some(Target::Production);
            match deploy::redeploy_any(&ctx, Redeploy { id, target, skip_domain, actor, via: Via::Cli, no_cache }).await? {
                deploy::Redeployed::Published(_) => Ok(()),
                deploy::Redeployed::Building(started) => remote::watch(&ctx, &started).await,
            }
        }
        Command::Rollback { project, to } => {
            let project = match project {
                Some(p) => p,
                None => deploy::project_from_dir(&std::env::current_dir()?)?,
            };
            deploy::rollback(&ctx, &project, to.as_deref()).await.map(|_| ())
        }
        Command::Ls { project: None } => list_projects(&ctx).await,
        Command::Ls { project: Some(project) } => list_deployments(&ctx, &project).await,
        Command::Inspect { id, logs: false } => inspect(&ctx, &id).await,
        Command::Inspect { id, logs: true } => inspect_logs(&ctx, &id).await,
        Command::Dashboard { url } => dashboard(&ctx, url).await,
        Command::Rm { id: Some(id), .. } => deploy::remove(&ctx, &id).await,
        Command::Rm { project: Some(project), yes, .. } => {
            confirm_delete(&project, yes)?;
            forkrail_cli::project::delete(&ctx, &project).await?;
            // What the dashboard keeps of it, which a delete through the dashboard removes itself.
            if let Err(e) = forkrail_cli::project::forget_dashboard_items(&ctx, &project).await {
                eprintln!("warning: the dashboard's protection settings for {project} are left: {e:#}");
            }
            Ok(())
        }
        Command::Rm { .. } => unreachable!("clap asks for an id or --project"),
        Command::Sweep { project, yes } => storage::sweep(&ctx, project.as_deref(), yes).await,
        Command::Retention { command, project } => retention_command(DomainsVia::Aws(&ctx), command, project).await,
        Command::Restore { id } => restore_command(DomainsVia::Aws(&ctx), id).await,
        Command::Env { command } => env(DomainsVia::Aws(&ctx), command).await,
        Command::Limits { project, max_workers } => deploy::limits(&ctx, &project, max_workers.map(|m| m.0)).await,
        Command::Git { command: GitCommand::Setup { from: Some(file), .. } } => github::setup_from(&ctx, &file).await,
        Command::Git { command: GitCommand::Setup { org, owner, name, callbacks, replace, from: None } } => {
            github::setup(&ctx, github::Setup { org, owner, name, callbacks, replace, opened_by_page: false }).await
        }
        Command::Git { command: GitCommand::Status } => github::status(&ctx).await,
        Command::Git { command: GitCommand::Repos { query } } => connect::print_repos(&ctx, query.as_deref()).await,
        Command::Git { command: GitCommand::Connect { project, repo, branch, root, create: true } } => {
            let api = connect::Api::connect(&ctx).await?;
            let actor = git::deployer(&std::env::current_dir()?);
            let made = connect::create(&ctx, &api, connect::Create { project: project.clone(), repo, branch, root, env: Default::default(), actor, framework: None }).await?;
            let root = made.git.root.as_deref().map(|r| format!(" (the folder {r})")).unwrap_or_default();
            log(&format!("{project} is made, and deploys from {}{root}: pushes to {} go to production, other branches to previews", made.git.repo, made.git.production_branch));
            match (made.deployment, made.env_error, made.deploy_error) {
                (Some(started), _, _) => remote::watch(&ctx, &started).await,
                (None, Some(error), _) => bail!("its env vars weren't saved, so nothing was deployed: {error}"),
                (None, None, error) => bail!("its first deployment couldn't be started: {}", error.unwrap_or_else(|| "no reason given".to_owned())),
            }
        }
        Command::Git { command: GitCommand::Connect { project, repo, branch, root, create: false } } => {
            let api = connect::Api::connect(&ctx).await?;
            let actor = git::deployer(&std::env::current_dir()?);
            let link = connect::connect(&ctx, &api, connect::Connect { project: project.clone(), repo, branch, root, actor }).await?;
            let root = link.root.as_deref().map(|r| format!(" (the folder {r})")).unwrap_or_default();
            log(&format!("{project} deploys from {}{root}: pushes to {} go to production, other branches to previews", link.repo, link.production_branch));
            Ok(())
        }
        Command::Git { command: GitCommand::Disconnect { project } } => {
            match connect::disconnect(&ctx, &project).await? {
                Some(link) => log(&format!("{project} no longer deploys from {}", link.repo)),
                None => log(&format!("{project} had no connected repository")),
            }
            Ok(())
        }
        Command::Git { command: GitCommand::Deploy { project, branch } } => {
            let Some(link) = project_record(&ctx, &project).await?.git else { bail!("{project} has no connected repository: `forkrail git connect {project} OWNER/NAME`") };
            let api = connect::Api::connect(&ctx).await?;
            let push = connect::Push { project, repo_id: link.repo_id, branch: branch.unwrap_or(link.production_branch), sha: None, before: None, pusher: git::deployer(&std::env::current_dir()?), hook: None, no_cache: false, webhook: false, pusher_role: None };
            match connect::pushed(&ctx, &api, push).await? {
                // The builder's end goes to the install's control function, which publishes it and
                // sets the commit's status, as for a push.
                connect::Pushed::Started(started) => remote::watch(&ctx, &started).await,
                connect::Pushed::Skipped(why) => {
                    log(&format!("nothing deployed: {why}"));
                    Ok(())
                }
            }
        }
        Command::Owners { command: OwnersCommand::Ls } => github::owners_list(&ctx).await,
        Command::Owners { command: OwnersCommand::Add { login } } => github::owners_add(&ctx, &login).await,
        Command::Owners { command: OwnersCommand::Rm { login } } => github::owners_remove(&ctx, &login).await,
        Command::Domains { command } => domains_command(DomainsVia::Aws(&ctx), command).await,
        Command::Schedules { command } => schedules_command(DomainsVia::Aws(&ctx), command).await,
        Command::DeployHooks { command } => deploy_hooks_command(DomainsVia::Aws(&ctx), command).await,
        Command::Protection { command } => forkrail_cli::protection_cli::run(forkrail_cli::routing_cli::Via::Aws(&ctx), command).await,
        Command::Spend { command } => forkrail_cli::spend_cli::run(forkrail_cli::routing_cli::Via::Aws(&ctx), command).await,
        Command::Curl { url, args } => forkrail_cli::protection_cli::curl(forkrail_cli::routing_cli::Via::Aws(&ctx), &url, &args).await,
        Command::Firewall { command } => forkrail_cli::firewall_cli::run(forkrail_cli::routing_cli::Via::Aws(&ctx), command).await,
        Command::Cache { command } => forkrail_cli::purge::run_cli(forkrail_cli::routing_cli::Via::Aws(&ctx), command).await,
        Command::Regions { command: RegionsCommand::Ls } => regions::list(&ctx).await,
        Command::Regions { command: RegionsCommand::Add { name, all } } => regions::add(&ctx, &name, all).await,
        Command::Regions { command: RegionsCommand::Rm { name } } => regions::remove(&ctx, &name).await,
        Command::Regions { command: RegionsCommand::Copy { name, ids } } => regions::copy(&ctx, &name, &ids).await,
        Command::Regions { command: RegionsCommand::Setup { name } } => regions::setup(&ctx, &name).await,
        Command::State { command: StateCommand::Status } => forkrail_cli::state_store::status(&ctx).await,
        Command::State { command: StateCommand::Mode { mode, no_s3_copy, force } } => forkrail_cli::state_store::set_mode(&ctx, mode, !no_s3_copy, force).await,
        Command::State { command: StateCommand::Migrate } => forkrail_cli::state_store::migrate(&ctx).await,
        Command::State { command: StateCommand::Check { repair } } => {
            if forkrail_cli::state_store::check(&ctx, repair).await?.none() || repair {
                Ok(())
            } else {
                bail!("they differ: `forkrail state check --repair` makes the one not read like the one read")
            }
        }
        Command::Logs { project, since, until, status, path, host, limit, follow, request, app, search, level } => {
            let args = observe::LogsArgs { project, since, until, status, path, host, limit, follow, request, app, search, level };
            observe::logs(&observe::Source::Direct(forkrail_cli::analytics::Analytics::connect(&ctx).await?), args).await
        }
        Command::Stats { project, since } => observe::stats(&ctx, &project, &since).await,
        Command::Usage { project, since, daily } => observe::usage(&ctx, project.as_deref(), &since, daily).await,
        Command::Routers { since } => observe::routers(&ctx, &since).await,
        Command::Redirects { command } => forkrail_cli::routing_cli::redirects(forkrail_cli::routing_cli::Via::Aws(&ctx), command).await,
        Command::Routes { command } => forkrail_cli::routing_cli::routes(forkrail_cli::routing_cli::Via::Aws(&ctx), command).await,
        Command::FillRouteDetails => deploy::fill_route_details(&ctx).await,
        Command::FillProjectSettings => fill_project_settings(&ctx).await,
    }
}

/// Whether the install's API serves a command, for someone signed in.
fn through_api(command: &Command) -> bool {
    matches!(
        command,
        Command::Whoami
            | Command::Deploy { .. }
            | Command::Cancel { .. }
            | Command::Promote { .. }
            | Command::Redeploy { .. }
            | Command::Rollback { .. }
            | Command::Ls { .. }
            | Command::Inspect { .. }
            | Command::Logs { .. }
            | Command::Redirects { .. }
            | Command::Routes { .. }
            | Command::Domains { .. }
            | Command::Schedules { .. }
            | Command::DeployHooks { .. }
            | Command::Protection { .. }
            | Command::Spend { .. }
            | Command::Firewall { .. }
            | Command::Cache { .. }
            | Command::Curl { .. }
            | Command::Rm { .. }
            | Command::Project { .. }
            | Command::Retention { .. }
            | Command::Restore { .. }
    ) || matches!(command, Command::Env { command } if !matches!(command, EnvCommand::Migrate { .. }))
}

/// Deleting a project can't be undone: its name is typed again first, unless `yes`.
fn confirm_delete(project: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("deleting {project} can't be undone: add --yes to delete it without asking");
    }
    eprint!("Deleting {project} takes its deployments, domains, schedules, env vars and functions with it, and can't be undone.\nType {project} to delete it: ");
    let mut typed = String::new();
    std::io::stdin().read_line(&mut typed)?;
    if typed.trim() != project {
        bail!("not deleted: that isn't {project}");
    }
    Ok(())
}

/// A command of someone signed in, through the install's API (api.rs).
async fn via_api(client: &api::Client, command: Command) -> Result<()> {
    match command {
        Command::Whoami => api::whoami(client).await,
        Command::Deploy { local, prebuilt, .. } if local || prebuilt => {
            bail!("building on this machine isn't served through a login yet. Without --local{} the source is uploaded and built in the install's builder; with the install's AWS credentials, add --aws", if prebuilt { " and --prebuilt" } else { "" })
        }
        Command::Deploy { dir, prod, project, memory, concurrent, same_source, skip_domain, no_cache, .. } => {
            api::deploy(client, remote::RemoteDeploy { dir, prod, project, memory, concurrent, same_source, skip_domain, no_cache }).await
        }
        Command::Cancel { id } => api::cancel(client, &id).await,
        Command::Rm { id: Some(id), .. } => api::remove(client, &id).await,
        Command::Rm { project: Some(project), yes, .. } => {
            confirm_delete(&project, yes)?;
            api::delete_project(client, &project).await
        }
        Command::Promote { id, allow_preview_env } => api::promote(client, &id, allow_preview_env).await,
        Command::Redeploy { id, prod, skip_domain, no_cache } => api::redeploy(client, &id, prod, skip_domain, no_cache).await,
        Command::Rollback { project, to } => {
            let project = match project {
                Some(p) => p,
                None => deploy::project_from_dir(&std::env::current_dir()?)?,
            };
            api::rollback(client, &project, to.as_deref()).await
        }
        Command::Ls { project } => api::ls(client, project.as_deref()).await,
        Command::Inspect { id, logs: false } => api::inspect(client, &id).await,
        Command::Inspect { id, logs: true } => api::inspect_logs(client, &id).await,
        Command::Logs { project, since, until, status, path, host, limit, follow, request, app, search, level } => {
            let args = observe::LogsArgs { project, since, until, status, path, host, limit, follow, request, app, search, level };
            observe::logs(&observe::Source::Api(client), args).await
        }
        Command::Redirects { command } => forkrail_cli::routing_cli::redirects(forkrail_cli::routing_cli::Via::Api(client), command).await,
        Command::Routes { command } => forkrail_cli::routing_cli::routes(forkrail_cli::routing_cli::Via::Api(client), command).await,
        Command::Domains { command } => domains_command(DomainsVia::Api(client), command).await,
        Command::Schedules { command } => schedules_command(DomainsVia::Api(client), command).await,
        Command::DeployHooks { command } => deploy_hooks_command(DomainsVia::Api(client), command).await,
        Command::Retention { command, project } => retention_command(DomainsVia::Api(client), command, project).await,
        Command::Restore { id } => restore_command(DomainsVia::Api(client), id).await,
        Command::Protection { command } => forkrail_cli::protection_cli::run(forkrail_cli::routing_cli::Via::Api(client), command).await,
        Command::Spend { command } => forkrail_cli::spend_cli::run(forkrail_cli::routing_cli::Via::Api(client), command).await,
        Command::Curl { url, args } => forkrail_cli::protection_cli::curl(forkrail_cli::routing_cli::Via::Api(client), &url, &args).await,
        Command::Firewall { command } => forkrail_cli::firewall_cli::run(forkrail_cli::routing_cli::Via::Api(client), command).await,
        Command::Cache { command } => forkrail_cli::purge::run_cli(forkrail_cli::routing_cli::Via::Api(client), command).await,
        Command::Env { command } => env(DomainsVia::Api(client), command).await,
        Command::Project { project, rename, memory, max_duration, concurrent, max_inflight, regions, role, egress, framework, auto_assign_domains, skew_protection, skew_max_age, skew_threshold, skew_allowed_domains, keep_warm, analytics } => {
            let change = Change {
                memory_mb: memory.map(|s| s.0),
                max_duration_s: max_duration.map(|s| s.0),
                concurrent: concurrent.map(|s| s.0),
                max_inflight: max_inflight.map(|s| s.0),
                role: role.map(|r| Some(r).filter(|r| r != "default")),
                egress: egress.map(|_| Vec::new()),
                framework: framework.map(|f| Some(f.trim().to_owned()).filter(|f| f != "auto" && !f.is_empty())),
                auto_assign_domains: auto_assign_domains.map(|s| s.0),
                skew: skew_change(skew_protection, skew_max_age, skew_threshold, skew_allowed_domains),
                keep_warm: keep_warm.map(|s| s.0),
                analytics,
                regions: regions.map(regions_setting),
            };
            if let Some(name) = rename {
                let r = api::rename_project(client, &project, &name).await?;
                println!("{}: called {} (was {})", r.project, r.name, r.previous);
                if change.is_empty() {
                    return Ok(());
                }
            }
            api::project_settings(client, &project, &change).await
        }
        _ => unreachable!("only commands `through_api` names come here"),
    }
}

/// A project named by what it was renamed to means that project: these commands act on its id
/// (`forkrail project NAME --rename`). A name no project has stays as it is.
async fn resolve_project_name(ctx: &Ctx, command: &mut Command) -> Result<()> {
    let slot = match command {
        Command::Deploy { project: Some(p), .. }
        | Command::Build { project: Some(p), .. }
        | Command::Rollback { project: Some(p), .. }
        | Command::Ls { project: Some(p) }
        | Command::Rm { project: Some(p), .. }
        | Command::Retention { project: Some(p), .. }
        | Command::Sweep { project: Some(p), .. }
        | Command::Usage { project: Some(p), .. } => p,
        Command::Limits { project, .. } | Command::Logs { project, .. } | Command::Stats { project, .. } => project,
        _ => return Ok(()),
    };
    *slot = forkrail_cli::project::resolve(ctx, slot).await?;
    Ok(())
}

/// `forkrail build`: the build, into `out`, then where it is.
async fn build_into(
    dir: PathBuf,
    out: PathBuf,
    request: build::Request,
    bootstrap: Option<PathBuf>,
    python: String,
    no_cache: bool,
    prebuilt: bool,
) -> Result<()> {
    let dir = dir.canonicalize().with_context(|| format!("{}", dir.display()))?;
    let opts = build::Options {
        package: package::Options { bootstrap: bootstrap.unwrap_or_else(default_bootstrap), python, no_cache },
        prebuilt,
        install: forkrail_cli::framework::Install::IfMissing,
    };
    let built = {
        let out = out.clone();
        tokio::task::spawn_blocking(move || build::build(&dir, &request, &opts, &out, &buildlog::Terminal)).await??
    };
    log(&format!("built {} ({}, deployment {}) in {}", built.project, built.target.as_str(), built.id, out.display()));
    println!("{}", out.display());
    Ok(())
}

/// Save how to reach the install, once its bucket is found in the account.
async fn link(ctx: &Ctx, resolved: &config::Resolved) -> Result<()> {
    let Install { account, home_region, bucket, .. } = &ctx.install;
    ctx.store.s3.head_bucket().bucket(bucket).send().await.map_err(|e| {
        anyhow::anyhow!(
            "no forkrail install found in account {account}, {home_region}: s3://{bucket}: {}",
            aws_sdk_s3::error::DisplayErrorContext(&e)
        )
    })?;
    let saved = config::Config { profile: resolved.profile.clone(), region: Some(home_region.clone()), bucket: resolved.bucket.clone() };
    let path = config::save(&saved)?;
    let profile = resolved.profile.as_deref().map_or_else(|| "the default AWS credentials".to_owned(), |p| format!("AWS profile {p}"));
    println!("linked: account {account}, home region {home_region}, s3://{bucket}, with {profile}");
    println!("saved in {}", path.display());
    Ok(())
}

/// The runtime built for Lambda, next to this binary's target directory.
fn default_bootstrap() -> PathBuf {
    let relative = "aarch64-unknown-linux-gnu/release/forkrail-runtime";
    let beside_exe = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.join(relative)));
    let in_cwd = PathBuf::from("target").join(relative);
    beside_exe.into_iter().chain([in_cwd]).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("target").join(relative))
}

async fn list_projects(ctx: &Ctx) -> Result<()> {
    let settings = ctx.store.settings().await?;
    let keys = ctx.store.list(forkrail_common::install::PROJECTS_PREFIX).await?;
    if keys.is_empty() {
        println!("no projects yet");
        return Ok(());
    }
    println!("{:<24} {:<10} {:>11}  URL", "PROJECT", "PRODUCTION", "DEPLOYMENTS");
    for key in keys {
        let Some(p) = ctx.store.get_value::<Project>(&key).await? else { continue };
        // Renamed, the name it has first stays in its addresses: both are shown.
        let name = match &p.display_name {
            Some(shown) => format!("{shown} ({})", p.name),
            None => p.name.clone(),
        };
        println!(
            "{:<24} {:<10} {:>11}  {}",
            name,
            p.production.as_deref().unwrap_or("-"),
            p.deployments.len(),
            if p.production.is_some() { settings.url(&format!("{}.{}", p.name, settings.base_domain)) } else { String::new() }
        );
    }
    Ok(())
}

async fn list_deployments(ctx: &Ctx, project: &str) -> Result<()> {
    let p = project_record(ctx, project).await?;
    let settings = ctx.store.settings().await?;
    // Deployments made before the base domain changed keep their old hosts.
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    println!("{:<10} {:<11} {:<9} {:<21} {:>7}  URL", "ID", "TARGET", "STATUS", "CREATED", "VERSION");
    let now = state::now_secs();
    for d in &p.deployments {
        let status = d.status.at(&d.created, now);
        let production = if p.production.as_deref() == Some(d.id.as_str()) {
            "  (production)"
        } else if d.target == Target::Production && !p.production_history.contains(&d.id) && staged(ctx, &d.id, &format!("{}.{}", p.name, settings.base_domain)).await {
            // Published without its domains, never live: waiting for `promote`.
            "  (staged)"
        } else {
            ""
        };
        let own = format!("{}-{}.", p.name, d.id);
        let host = routes
            .hosts_of(&d.id)
            .find(|h| h.starts_with(&own))
            .map_or_else(|| format!("{own}{}", settings.base_domain), str::to_owned);
        // Only a published deployment answers on its URL.
        if status != Status::Ready {
            println!("{:<10} {:<11} {:<9} {:<21} {:>7}  -", d.id, d.target.as_str(), status.as_str(), d.created, "-");
            continue;
        }
        println!(
            "{:<10} {:<11} {:<9} {:<21} {:>7}  {}{production}",
            d.id,
            d.target.as_str(),
            status.as_str(),
            d.created,
            if d.version.is_empty() { "-" } else { d.version.as_str() },
            Settings { base_domain: host.split_once('.').map_or("", |(_, b)| b).to_owned(), ..Default::default() }.url(&host)
        );
    }
    if !p.deleted.is_empty() {
        println!("\n{} deleted, still restorable: `forkrail retention --project {}` lists them", p.deleted.len(), p.name);
    }
    Ok(())
}

/// Whether production deployment `id` was published without the production host (`--skip-domain`,
/// or "Auto-assign custom domains" off): its record says, a rolled-back-from one isn't.
async fn staged(ctx: &Ctx, id: &str, production_host: &str) -> bool {
    matches!(ctx.store.get_value::<DeploymentRecord>(&state::deployment_key(id)).await, Ok(Some(r)) if !r.hosts.iter().any(|h| h == production_host))
}

async fn inspect(ctx: &Ctx, id: &str) -> Result<()> {
    let record = deploy::record(ctx, id).await?;
    let routes = ctx.store.get_value::<Routes>(ROUTES_KEY).await?.unwrap_or_default();
    let serving: Vec<&str> = routes.hosts_of(id).collect();
    println!("{}", serde_json::to_string_pretty(&record)?);
    let now = record.status_now();
    if now != record.status {
        println!("status now: {} ({})", now.as_str(), state::STOPPED_REPORTING);
    }
    println!("serving: {}", if serving.is_empty() { "nothing".to_owned() } else { serving.join(", ") });
    Ok(())
}

/// A deployment's build log, from the copy kept in S3 when it ended.
async fn inspect_logs(ctx: &Ctx, id: &str) -> Result<()> {
    let record = deploy::record(ctx, id).await?;
    let Some(key) = record.log.as_ref().and_then(|l| l.kept.clone()) else {
        match record.status_now() {
            Status::Queued | Status::Building => bail!("deployment {id} is still building; the dashboard shows its log as it runs"),
            _ => bail!("deployment {id} has no build log: it was made before forkrail kept them"),
        }
    };
    let bytes = ctx.store.get_bytes(&key).await?.with_context(|| format!("deployment {id}'s build log is gone from {key}"))?;
    let lines = report::gunzip_lines(&bytes)?;
    let start = lines.first().map_or(0, |l| l.t);
    for line in &lines {
        let at = line.t.saturating_sub(start) as f64 / 1000.0;
        match line.stream.as_str() {
            "step" => println!("{at:>7.1}s  == {}", line.text),
            "forkrail" => println!("{at:>7.1}s  ==> {}", line.text),
            _ => println!("{at:>7.1}s  {}", line.text),
        }
    }
    if let Some(failure) = &record.error {
        println!("error{}: {}", failure.step.as_deref().map(|s| format!(" in {s}")).unwrap_or_default(), failure.message);
    }
    Ok(())
}

/// Show or set where builds send their live log lines.
async fn dashboard(ctx: &Ctx, url: Option<String>) -> Result<()> {
    let Some(url) = url else {
        let settings = ctx.store.settings().await?;
        println!("{}", settings.dashboard.as_deref().unwrap_or("none: builds' logs are kept only when they end"));
        return Ok(());
    };
    let url = url.trim().trim_end_matches('/').to_owned();
    let url = match url.as_str() {
        "none" | "" => None,
        u if u.starts_with("https://") || u.starts_with("http://localhost") || u.starts_with("http://127.0.0.1") => Some(url),
        _ => bail!("{url}: the dashboard's address starts with https://"),
    };
    ctx.store
        .update::<Settings, _>(SETTINGS_KEY, |current| {
            let mut s = current.unwrap_or_default();
            s.dashboard = url.clone();
            Ok(s)
        })
        .await?;
    match url {
        Some(url) => log(&format!("builds send their logs to {url}/api/builds/<id>/log as they run")),
        None => log("builds keep their logs only when they end"),
    }
    Ok(())
}

/// `forkrail env …`, with the install's AWS credentials or through a login (all but `migrate`).
async fn env(via: DomainsVia<'_>, command: EnvCommand) -> Result<()> {
    let pulled = |project: Option<String>, environment: Option<TargetArg>, branch: Option<String>| async move {
        let target = match (environment, branch.as_deref()) {
            (None, None) => EnvTarget::Development,
            (environment, branch) => env_vars::pull_target(environment.map(Into::into), branch)?,
        };
        let project = match project {
            Some(p) => p,
            None => deploy::project_from_dir(&std::env::current_dir()?)?,
        };
        match via {
            DomainsVia::Aws(ctx) => env_vars::pull(ctx, &project, target, branch.as_deref()).await,
            DomainsVia::Api(client) => api::env_pull(client, &project, target, branch.as_deref()).await,
        }
    };
    match command {
        EnvCommand::Ls { project, git_branch } => {
            let names = match via {
                DomainsVia::Aws(ctx) => {
                    let p = project_record(ctx, &project).await?;
                    if p.legacy_env.has_values() {
                        log(&format!("{project}'s values are still in its project file: `forkrail env migrate` moves them"));
                    }
                    p.env_names
                }
                DomainsVia::Api(client) => api::env_names(client, &project).await?,
            };
            print!("{}", env_listing(&names, git_branch.as_deref()));
            if let Some(branch) = &git_branch {
                log(&format!("{branch}'s previews also get every other preview variable"));
            }
            Ok(())
        }
        EnvCommand::Add { project, key, target, git_branch } => {
            deploy::check_project(&project)?;
            deploy::check_user_env_key(&key)?;
            let branch = git_branch.as_deref();
            let targets = targets(&target, if branch.is_some() { &[] } else { &[EnvTarget::Production, EnvTarget::Preview] });
            // Refused before the value is asked for.
            env_vars::scopes(&targets, branch)?;
            let value = read_value(&key)?;
            let values = [(key.clone(), value)].into();
            match via {
                DomainsVia::Aws(ctx) => env_vars::set(ctx, &project, &values, &targets, branch, true).await?,
                DomainsVia::Api(client) => api::env_set(client, &project, &values, &targets, branch).await?,
            }
            log(&format!("{project}: set {key} for {}; {}", scope_words(&targets, branch), applies(&targets, branch)));
            Ok(())
        }
        EnvCommand::Rm { project, key, target, git_branch } => {
            let branch = git_branch.as_deref();
            let targets = targets(&target, if branch.is_some() { &[] } else { &EnvTarget::ALL });
            env_vars::scopes(&targets, branch)?;
            let removed = match via {
                DomainsVia::Aws(ctx) => env_vars::remove(ctx, &project, &key, &targets, branch).await?,
                DomainsVia::Api(client) => api::env_remove(client, &project, &key, &targets, branch).await?,
            };
            if !removed {
                bail!("{project} has no {key} for {}", scope_words(&targets, branch));
            }
            log(&format!("{project}: removed {key}; {}", applies(&targets, branch)));
            Ok(())
        }
        EnvCommand::Pull { file, project, environment, git_branch, yes } => {
            let pulled = pulled(project, environment, git_branch).await?;
            forkrail_cli::dotenv::write_pulled(&pulled, &file.unwrap_or_else(|| PathBuf::from(".env.local")), yes)
        }
        EnvCommand::Run { project, environment, git_branch, command } => {
            let pulled = pulled(project, environment, git_branch).await?;
            std::process::exit(forkrail_cli::dotenv::run(&pulled, &command)?);
        }
        EnvCommand::Migrate { project } => match via {
            DomainsVia::Aws(ctx) => migrate_env(ctx, project.as_deref()).await,
            DomainsVia::Api(_) => bail!("`env migrate` needs the install's AWS credentials: add --aws"),
        },
    }
}

/// `production and preview`, `development`.
fn and_list(targets: &[EnvTarget]) -> String {
    let names: Vec<&str> = targets.iter().map(|t| t.as_str()).collect();
    match names.split_last() {
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// `production and preview`, or a branch's: `preview (feature/logs)`.
fn scope_words(targets: &[EnvTarget], branch: Option<&str>) -> String {
    match branch {
        Some(branch) => format!("preview ({branch})"),
        None => and_list(targets),
    }
}

/// `forkrail env ls`: a line per environment, then one per Git branch with its own preview names;
/// with `only`, that branch's line alone.
fn env_listing(names: &EnvNames, only: Option<&str>) -> String {
    let list = |names: &std::collections::BTreeSet<String>| if names.is_empty() { "-".to_owned() } else { names.iter().map(String::as_str).collect::<Vec<_>>().join(" ") };
    let mut lines: Vec<(String, String)> = Vec::new();
    match only {
        Some(branch) => lines.push((format!("preview ({branch})"), list(names.branch(branch)))),
        None => {
            lines.extend(EnvTarget::ALL.map(|t| (t.as_str().to_owned(), list(names.get(t)))));
            lines.extend(names.branches.iter().map(|(branch, own)| (format!("preview ({branch})"), list(own))));
        }
    }
    let width = lines.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0).max(11);
    lines.iter().map(|(label, names)| format!("{label:<width$} {names}\n")).collect()
}

/// When a change to `targets` (or to a branch's) is seen: deployments' at their next deploy,
/// Development's at the next pull.
fn applies(targets: &[EnvTarget], branch: Option<&str>) -> &'static str {
    if branch.is_some() {
        return "takes effect on the branch's next preview";
    }
    match (targets.iter().any(|t| t.deployed().is_some()), targets.contains(&EnvTarget::Development)) {
        (true, true) => "takes effect on the next deploy, and locally on the next `forkrail env pull`",
        (true, false) => "takes effect on the next deploy",
        _ => "`forkrail env pull` brings it to a local file",
    }
}

/// `forkrail env migrate`: each project's values into SSM, then the project file without them;
/// then every project function's `$LATEST` environment encrypted with its region's key (published
/// versions keep theirs until the next deploy). Values are never printed.
async fn migrate_env(ctx: &Ctx, only: Option<&str>) -> Result<()> {
    let store = Secrets::new(ctx);
    let mut moved = 0;
    let keys = match only {
        Some(project) => vec![state::project_key(project)],
        None => ctx.store.list(forkrail_common::install::PROJECTS_PREFIX).await?,
    };
    for key in keys {
        let Some(p) = ctx.store.get_value::<Project>(&key).await? else { continue };
        let LegacyEnv::Values { production, preview } = &p.legacy_env else {
            // Nothing to move, but an old `env` map, even an empty one, would let an older forkrail
            // deploy the project as if it had no env vars: write the notice in its place.
            let raw = ctx.store.get_value::<serde_json::Value>(&key).await?;
            if raw.as_ref().is_some_and(|r| !r["env"].is_string()) {
                ctx.store.update::<Project, _>(&key, |current| current.context("the project file went away")).await?;
                log(&format!("{}: no values; the notice written", p.name));
            }
            continue;
        };
        // Every value is in SSM before the file loses any.
        for (target, values) in [(Target::Production, production), (Target::Preview, preview)] {
            store.put_all(&p.name, target, values).await?;
        }
        ctx.store
            .update::<Project, _>(&key, |current| {
                let mut c = current.context("the project file went away")?;
                if c.legacy_env != p.legacy_env && c.legacy_env.has_values() {
                    bail!("{}'s env vars changed while they were being moved; run `forkrail env migrate` again", p.name);
                }
                c.env_names.production.extend(production.keys().cloned());
                c.env_names.preview.extend(preview.keys().cloned());
                c.legacy_env = LegacyEnv::Moved;
                Ok(c)
            })
            .await?;
        log(&format!("{}: moved {} production and {} preview values", p.name, production.len(), preview.len()));
        moved += 1;
    }
    log(&format!("{moved} projects had values to move"));

    let settings = ctx.store.settings().await?;
    let mut regions = vec![ctx.install.home_region.clone()];
    regions.extend(settings.regions.iter().filter(|r| **r != ctx.install.home_region).cloned());
    for region in &regions {
        let kms_key = ctx.env_key(region).await?;
        let lambda = ctx.lambda_in(region);
        let functions = match only {
            Some(project) => vec![forkrail_cli::lambda::function_name(project)],
            None => lambda.project_functions().await?,
        };
        let mut encrypted = 0;
        for name in &functions {
            let project = name.strip_prefix(forkrail_common::install::FUNCTION_PREFIX).unwrap_or(name);
            match lambda.check(name, project).await {
                Ok(true) => {}
                // Not published in this region.
                Ok(false) => continue,
                Err(e) => {
                    eprintln!("warning: {e:#}; left as it is");
                    continue;
                }
            }
            if lambda.encrypt_latest(name, &kms_key).await? {
                encrypted += 1;
            }
        }
        log(&format!("{region}: {encrypted} of {} functions' $LATEST newly encrypted with {kms_key}", functions.len()));
    }
    Ok(())
}

/// An env value from stdin, without echoing it. A trailing newline is dropped.
fn read_value(key: &str) -> Result<String> {
    let value = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("value for {key}: "))?
    } else {
        let mut value = String::new();
        std::io::stdin().read_to_string(&mut value)?;
        value.strip_suffix('\n').map(|v| v.strip_suffix('\r').unwrap_or(v)).unwrap_or(&value).to_owned()
    };
    if value.is_empty() {
        bail!("empty value for {key}");
    }
    Ok(value)
}

/// Show a project's settings, after changing what was asked. A project that doesn't exist yet is
/// created, so it can be set up before its first deploy.
async fn project_settings(ctx: &Ctx, name: &str, change: &Change) -> Result<()> {
    let p = project::update(ctx, name, change, true).await?;
    let or_default = |v: Option<String>, default: &str| v.unwrap_or_else(|| format!("default ({default})"));
    println!("memory:       {}", or_default(p.memory_mb.map(|m| format!("{m} MB")), "640 MB"));
    println!("maxDuration:  {}", or_default(p.max_duration_s.map(|s| format!("{s} s")), "300 s"));
    println!("concurrent:   {}", or_default(p.concurrent.map(|f| f.to_string()), "true"));
    println!("per worker:   {}", or_default(p.max_inflight.map(|n| format!("up to {n} requests at once")), "up to 10"));
    let home = &ctx.install.home_region;
    println!("regions:      {}", if p.regions.is_empty() { format!("default (where production runs; {} for a new project)", regions::describe(std::slice::from_ref(home))) } else { regions::describe(&p.regions) });
    println!("role:         {}", ctx.install.role_arn(p.role.as_deref()));
    println!("egress:       {}", if p.egress.is_empty() { "none".to_owned() } else { p.egress.join(", ") });
    println!("framework:    {}", p.framework.as_deref().unwrap_or("auto (found from the project's files)"));
    println!("auto-assign:  {}", api::auto_assign_words(p.auto_assigns_domains()));
    println!("skew:         {}", forkrail_cli::skew::describe(&forkrail_cli::skew::get(ctx, name).await?));
    println!("keep warm:    {}", forkrail_cli::warm::keep_words(forkrail_cli::warm::keep_get(ctx, name).await?));
    println!("analytics:    {}", forkrail_cli::web_analytics::words(forkrail_cli::web_analytics::get(ctx, name).await?));
    if change.analytics.is_some() {
        log(forkrail_cli::web_analytics::NOTE);
        if change.only_analytics() {
            return Ok(());
        }
    }
    if change.only_skew() {
        log("skew protection applies at once; builds and apps are told it's on (VERCEL_SKEW_PROTECTION_ENABLED) from the next deploy");
        return Ok(());
    }
    if change.only_keep_warm() {
        log(forkrail_cli::warm::KEEP_NOTE);
        return Ok(());
    }
    if change.only_max_inflight() {
        return Ok(());
    }
    if change.auto_assign_domains.is_some() && change.framework.is_none() && change.memory_mb.is_none() && change.max_duration_s.is_none() && change.concurrent.is_none() && change.role.is_none() && change.egress.is_none() && change.regions.is_none() {
        log(&api::auto_assign_note(p.auto_assigns_domains()));
        return Ok(());
    }
    if !change.is_empty() {
        log("takes effect on the next deploy or redeploy; a deployment's vercel.json wins over memory, maxDuration, concurrent compute (its `fluid`) and regions");
    }
    Ok(())
}

/// `forkrail fill-project-settings`: each project without function settings gets its production
/// deployment's, so a deploy without flags keeps what production runs (768 MB, say).
async fn fill_project_settings(ctx: &Ctx) -> Result<()> {
    let mut filled = 0;
    for key in ctx.store.list(forkrail_common::install::PROJECTS_PREFIX).await? {
        let Some(p) = ctx.store.get_value::<Project>(&key).await? else { continue };
        if p.memory_mb.is_some() || p.max_duration_s.is_some() || p.concurrent.is_some() {
            continue;
        }
        let Some(production) = &p.production else { continue };
        let Some(rec) = ctx.store.get_value::<DeploymentRecord>(&state::deployment_key(production)).await? else { continue };
        let change = Change { memory_mb: Some(Some(rec.memory_mb)), max_duration_s: Some(Some(rec.max_duration_s)), concurrent: Some(Some(rec.concurrent)), ..Default::default() };
        project::update(ctx, &p.name, &change, false).await?;
        log(&format!("{}: {} MB, maxDuration {} s, {}", p.name, rec.memory_mb, rec.max_duration_s, if rec.concurrent { "concurrent compute" } else { "one request per worker" }));
        filled += 1;
    }
    log(&format!("{filled} projects given their production deployment's settings"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_workers_values() {
        assert_eq!(parse_max_workers("50").unwrap().0, Some(50));
        assert_eq!(parse_max_workers("default").unwrap().0, None);
        for bad in ["0", "1001", "-1", "lots", ""] {
            assert!(parse_max_workers(bad).is_err(), "{bad}");
        }
        // The flag parses through clap the same way.
        let cli = Cli::try_parse_from(["forkrail", "limits", "harbor", "--max-workers", "60"]).unwrap();
        assert!(matches!(cli.command, Command::Limits { max_workers: Some(MaxWorkers(Some(60))), .. }));
        assert!(Cli::try_parse_from(["forkrail", "limits", "harbor", "--max-workers", "0"]).is_err());
    }

    #[test]
    fn deploys_flags_alone_deploy() {
        let args = |line: &str| deploy_by_default(line.split(' ').map(Into::into).collect());
        let parsed = |line: &str| Cli::try_parse_from(args(line)).map(|c| c.command);
        assert!(matches!(parsed("forkrail --prod"), Ok(Command::Deploy { prod: true, .. })));
        assert!(matches!(parsed("forkrail --prod app"), Ok(Command::Deploy { prod: true, ref dir, .. }) if dir.as_os_str() == "app"));
        assert!(matches!(parsed("forkrail --project shop --prod"), Ok(Command::Deploy { prod: true, project: Some(ref p), .. }) if p == "shop"));
        // After the global flags, with or without their values.
        let cli = Cli::try_parse_from(args("forkrail --profile work --aws --region=eu-central-1 --prod")).unwrap();
        assert!(cli.aws && cli.profile.as_deref() == Some("work") && cli.region.as_deref() == Some("eu-central-1"));
        assert!(matches!(cli.command, Command::Deploy { prod: true, .. }));
        // Commands, help, version and flags deploy doesn't have are left as they were.
        for line in ["forkrail deploy --prod", "forkrail ls", "forkrail --help", "forkrail --version", "forkrail --profile work ls", "forkrail --frobnicate", "forkrail"] {
            assert_eq!(args(line), line.split(' ').map(std::ffi::OsString::from).collect::<Vec<_>>(), "{line}");
        }
        assert!(matches!(parsed("forkrail --profile work ls"), Ok(Command::Ls { .. })));
    }

    #[test]
    fn concurrent_flags_take_the_old_name() {
        let deploy = |flag: &str| match Cli::try_parse_from(["forkrail", "deploy", ".", flag, "false"]).unwrap().command {
            Command::Deploy { concurrent, .. } => concurrent,
            _ => unreachable!(),
        };
        assert_eq!(deploy("--concurrent"), Some(false));
        assert_eq!(deploy("--fluid"), Some(false));
        let project = |flag: &str, value: &str| match Cli::try_parse_from(["forkrail", "project", "harbor", flag, value]).unwrap().command {
            Command::Project { concurrent, .. } => concurrent.map(|s| s.0),
            _ => unreachable!(),
        };
        assert_eq!(project("--concurrent", "false"), Some(Some(false)));
        assert_eq!(project("--fluid", "true"), Some(Some(true)));
        assert_eq!(project("--concurrent", "default"), Some(None));
    }

    #[test]
    fn env_arguments() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["forkrail", "env"], args].concat()).unwrap().command;
        let Command::Env { command: EnvCommand::Add { target, .. } } = parse(&["add", "shop", "KEY", "-e", "preview,development"]) else { panic!("add") };
        assert_eq!(targets(&target, &[]), [EnvTarget::Preview, EnvTarget::Development]);
        let Command::Env { command: EnvCommand::Add { target, .. } } = parse(&["add", "shop", "KEY"]) else { panic!("add") };
        assert_eq!(targets(&target, &[EnvTarget::Production, EnvTarget::Preview]), [EnvTarget::Production, EnvTarget::Preview]);
        let Command::Env { command: EnvCommand::Rm { target, .. } } = parse(&["rm", "shop", "KEY", "--environment", "production", "--target", "production"]) else { panic!("rm") };
        assert_eq!(targets(&target, &EnvTarget::ALL), [EnvTarget::Production]);
        let Command::Env { command: EnvCommand::Pull { file, project, environment, git_branch, yes } } = parse(&["pull"]) else { panic!("pull") };
        assert!(file.is_none() && project.is_none() && environment.is_none() && git_branch.is_none() && !yes);
        let Command::Env { command: EnvCommand::Pull { file, environment, .. } } = parse(&["pull", ".env", "-e", "preview", "--yes"]) else { panic!("pull") };
        assert!(file == Some(PathBuf::from(".env")) && environment == Some(TargetArg::Preview));
        // A Git branch's: preview's alone, named or not.
        let Command::Env { command: EnvCommand::Add { target, git_branch, .. } } = parse(&["add", "shop", "KEY", "--git-branch", "feature/logs"]) else { panic!("add") };
        assert_eq!((targets(&target, &[]), git_branch.as_deref()), (vec![], Some("feature/logs")));
        assert!(env_vars::scopes(&[], git_branch.as_deref()).is_ok());
        let Command::Env { command: EnvCommand::Add { target, git_branch, .. } } = parse(&["add", "shop", "KEY", "-e", "production", "--git-branch", "b"]) else { panic!("add") };
        assert!(env_vars::scopes(&targets(&target, &[]), git_branch.as_deref()).is_err());
        let Command::Env { command: EnvCommand::Rm { git_branch, .. } } = parse(&["rm", "shop", "KEY", "--git-branch", "b"]) else { panic!("rm") };
        assert_eq!(git_branch.as_deref(), Some("b"));
        let Command::Env { command: EnvCommand::Pull { environment, git_branch, .. } } = parse(&["pull", "--git-branch", "b"]) else { panic!("pull") };
        assert_eq!(env_vars::pull_target(environment.map(Into::into), git_branch.as_deref()).unwrap(), EnvTarget::Preview);
        let Command::Env { command: EnvCommand::Run { environment, git_branch, command, .. } } = parse(&["run", "-e", "preview", "--git-branch", "b", "--", "npm", "test"]) else { panic!("run") };
        assert!(environment == Some(TargetArg::Preview) && git_branch.as_deref() == Some("b") && command == ["npm", "test"]);
        let Command::Env { command: EnvCommand::Ls { git_branch, .. } } = parse(&["ls", "shop", "--git-branch", "b"]) else { panic!("ls") };
        assert_eq!(git_branch.as_deref(), Some("b"));
        // Listed after the environments, a line per branch, the labels lined up.
        let mut names = EnvNames::default();
        names.preview.insert("API_URL".into());
        names.add_to_branch("feature/logs", ["API_URL".to_owned()]);
        assert_eq!(
            env_listing(&names, None),
            "production             -\npreview                API_URL\ndevelopment            -\npreview (feature/logs) API_URL\n"
        );
        assert_eq!(env_listing(&names, Some("other")), "preview (other) -\n");
        let Command::Env { command: EnvCommand::Run { command, .. } } = parse(&["run", "--", "next", "dev", "-p", "3001"]) else { panic!("run") };
        assert_eq!(command, ["next", "dev", "-p", "3001"]);
        assert!(Cli::try_parse_from(["forkrail", "env", "run"]).is_err(), "run needs a command");
        assert!(Cli::try_parse_from(["forkrail", "env", "add", "shop", "KEY", "-e", "staging"]).is_err());
        // Through a login: everything but `migrate`, and the project's settings.
        assert!(through_api(&parse(&["pull"])) && through_api(&parse(&["ls", "shop"])) && !through_api(&parse(&["migrate"])));
        assert!(through_api(&Cli::try_parse_from(["forkrail", "project", "shop", "--memory", "512"]).unwrap().command));
    }

    #[test]
    fn deploy_memory_is_within_lambdas_limits() {
        let memory = |mb: &str| match Cli::try_parse_from(["forkrail", "deploy", "--memory", mb]).map(|c| c.command) {
            Ok(Command::Deploy { memory, .. }) => memory,
            _ => None,
        };
        assert_eq!((memory("128"), memory("768"), memory("10240")), (Some(128), Some(768), Some(10_240)));
        assert!(Cli::try_parse_from(["forkrail", "deploy", "--memory", "64"]).is_err());
        assert!(Cli::try_parse_from(["forkrail", "publish", "out", "--memory", "10241"]).is_err());
    }

    #[test]
    fn region_arguments_leave_the_home_region_alone() {
        let cli = Cli::try_parse_from(["forkrail", "regions", "add", "us-west-1"]).unwrap();
        assert_eq!(cli.region, None);
        assert!(matches!(cli.command, Command::Regions { command: RegionsCommand::Add { ref name, all: false } } if name == "us-west-1"));
        let cli = Cli::try_parse_from(["forkrail", "--region", "eu-central-1", "regions", "copy", "us-west-1", "a1b2c3d4"]).unwrap();
        assert_eq!(cli.region.as_deref(), Some("eu-central-1"));
        assert!(matches!(cli.command, Command::Regions { command: RegionsCommand::Copy { ref name, .. } } if name == "us-west-1"));
        let cli = Cli::try_parse_from(["forkrail", "regions", "setup", "eu-central-1"]).unwrap();
        assert!(matches!(cli.command, Command::Regions { command: RegionsCommand::Setup { ref name } } if name == "eu-central-1"));
    }

    #[test]
    fn project_regions() {
        let regions = |args: &[&str]| match Cli::try_parse_from([&["forkrail", "project", "api"], args].concat()).unwrap().command {
            Command::Project { regions, .. } => regions.map(regions_setting),
            _ => panic!("not project"),
        };
        assert_eq!(regions(&["--regions", "iad1,fra1"]), Some(Some(vec!["iad1".to_owned(), "fra1".to_owned()])));
        assert_eq!(regions(&["--regions", "us-east-1, eu-central-1"]), Some(Some(vec!["us-east-1".to_owned(), "eu-central-1".to_owned()])));
        assert_eq!(regions(&["--regions", "default"]), Some(None));
        assert_eq!(regions(&[]), None);
    }
}
