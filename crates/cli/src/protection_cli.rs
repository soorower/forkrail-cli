//! `forkrail protection …` and `forkrail curl`: a project's deployment protection
//! (protection.rs). Signed in, each op goes
//! through the dashboard's API as you; with `--aws`, straight to the install.

use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use forkrail_common::protection::{ProtectionScope, TrustedIps, TrustedScope};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::protection::{self, ProtectionOp, View};
use crate::routing_cli::Via;

#[derive(Clone, Copy, ValueEnum)]
pub enum ScopeArg {
    /// Every generated URL; the production names stay open.
    Standard,
    /// Every URL, production included.
    All,
    Off,
}

impl From<ScopeArg> for ProtectionScope {
    fn from(s: ScopeArg) -> Self {
        match s {
            ScopeArg::Standard => ProtectionScope::Standard,
            ScopeArg::All => ProtectionScope::All,
            ScopeArg::Off => ProtectionScope::Off,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum TrustedScopeArg {
    Standard,
    All,
    Production,
}

#[derive(Subcommand)]
pub enum ProtectionCommand {
    /// How PROJECT is protected.
    Show { project: String },
    /// Change how PROJECT is protected. The password is set in the dashboard.
    Set {
        project: String,
        #[arg(long, value_enum)]
        scope: Option<ScopeArg>,
        /// Whether the team's members may sign in (`on` or `off`).
        #[arg(long, value_parser = ["on", "off"])]
        members: Option<String>,
        /// Only these addresses (comma-separated CIDRs) may open it, signed in or not.
        #[arg(long, value_delimiter = ',', conflicts_with = "no_trusted_ips")]
        trusted_ips: Option<Vec<String>>,
        /// Which hosts the trusted IPs cover.
        #[arg(long, value_enum, default_value = "standard", requires = "trusted_ips")]
        trusted_scope: TrustedScopeArg,
        #[arg(long)]
        no_trusted_ips: bool,
        /// Paths whose OPTIONS requests pass (comma-separated prefixes; empty clears).
        #[arg(long, value_delimiter = ',')]
        options_paths: Option<Vec<String>>,
        /// Hosts made public (comma-separated; empty clears).
        #[arg(long, value_delimiter = ',')]
        exceptions: Option<Vec<String>>,
        /// End every access given so far: everyone signs in again.
        #[arg(long)]
        sign_out: bool,
    },
    /// Bypass secrets for automation (`x-forkrail-protection-bypass`).
    Bypass {
        #[command(subcommand)]
        command: BypassCommand,
    },
    /// The scope of every project without settings of its own (an owner's).
    Default {
        #[arg(value_enum)]
        scope: ScopeArg,
    },
    /// Give the install its signing key (`--rotate`: a new one) and the dashboard's address.
    /// With `--aws` only.
    Setup {
        /// The dashboard's origin, `https://forkrail-web.example.com`.
        #[arg(long)]
        login: Option<String>,
        #[arg(long)]
        rotate: bool,
    },
}

#[derive(Subcommand)]
pub enum BypassCommand {
    /// Make a secret, print it once. With `--env`, it also becomes the project's
    /// FORKRAIL_AUTOMATION_BYPASS_SECRET from its next deploy.
    Add {
        project: String,
        #[arg(long)]
        env: bool,
    },
    /// Stop a secret working: the start of its hash, as `show` prints it. `--env` also removes
    /// FORKRAIL_AUTOMATION_BYPASS_SECRET.
    Rm {
        project: String,
        hash: String,
        #[arg(long)]
        env: bool,
    },
}

async fn send(via: &Via<'_>, op: ProtectionOp) -> Result<Value> {
    let changes = op.changes();
    match via {
        Via::Aws(ctx) => protection::run(ctx, op).await,
        Via::Api(client) => {
            let body = serde_json::to_value(&op)?;
            if changes { client.post("protection", body).await } else { client.ask("protection", body).await }
        }
    }
}

fn view(answer: Value) -> Result<View> {
    serde_json::from_value(answer).context("the protection settings, as the install gave them")
}

pub async fn run(via: Via<'_>, command: ProtectionCommand) -> Result<()> {
    match command {
        ProtectionCommand::Show { project } => {
            let v = view(send(&via, ProtectionOp::Get { project }).await?)?;
            protection::print_view(&v);
            for h in &v.protection.bypass {
                println!("    {}…", &h[..12]);
            }
        }
        ProtectionCommand::Set { project, scope, members, trusted_ips, trusted_scope, no_trusted_ips, options_paths, exceptions, sign_out } => {
            let trusted = match (trusted_ips, no_trusted_ips) {
                (_, true) => Some(None),
                (Some(addresses), _) => {
                    let scope = match trusted_scope {
                        TrustedScopeArg::Standard => TrustedScope::Standard,
                        TrustedScopeArg::All => TrustedScope::All,
                        TrustedScopeArg::Production => TrustedScope::Production,
                    };
                    Some(Some(TrustedIps { scope, addresses }))
                }
                (None, false) => None,
            };
            let clean = |v: Option<Vec<String>>| v.map(|v| v.into_iter().filter(|s| !s.is_empty()).collect());
            let op = ProtectionOp::Update {
                project,
                scope: scope.map(Into::into),
                members: members.map(|m| m == "on"),
                password: None,
                trusted_ips: trusted,
                options_paths: clean(options_paths),
                exceptions: clean(exceptions),
                sign_out,
            };
            protection::print_view(&view(send(&via, op).await?)?);
        }
        ProtectionCommand::Bypass { command: BypassCommand::Add { project, env } } => {
            let secret = forkrail_common::random_hex(16);
            let hash = forkrail_common::hex(&Sha256::digest(secret.as_bytes()));
            send(&via, ProtectionOp::BypassAdd { project: project.clone(), hash, secret: env.then(|| secret.clone()) }).await?;
            eprintln!("==> {project}: a bypass secret, shown once{}", if env { "; FORKRAIL_AUTOMATION_BYPASS_SECRET from the next deploy" } else { "" });
            println!("{secret}");
        }
        ProtectionCommand::Bypass { command: BypassCommand::Rm { project, hash, env } } => {
            let v = view(send(&via, ProtectionOp::Get { project: project.clone() }).await?)?;
            let found: Vec<&String> = v.protection.bypass.iter().filter(|h| h.starts_with(&hash.to_ascii_lowercase())).collect();
            let [full] = found.as_slice() else { bail!("{} of {project}'s secrets start with {hash:?}: give more of it", found.len()) };
            send(&via, ProtectionOp::BypassRemove { project: project.clone(), hash: (*full).clone(), env }).await?;
            eprintln!("==> {project}: that secret no longer works");
        }
        ProtectionCommand::Default { scope } => {
            send(&via, ProtectionOp::Default { scope: scope.into() }).await?;
            eprintln!("==> projects without settings of their own: {}", ProtectionScope::from(scope).name());
        }
        ProtectionCommand::Setup { login, rotate } => {
            let Via::Aws(ctx) = via else { bail!("setting up the signing key needs the install's AWS credentials: add --aws") };
            let said = protection::setup(ctx, login, rotate).await?;
            eprintln!("==> {}", json!(said));
        }
    }
    Ok(())
}

/// `forkrail curl URL [-- curl's args]`: curl, let in as a team member.
pub async fn curl(via: Via<'_>, url: &str, args: &[String]) -> Result<()> {
    let host = url.split("://").nth(1).unwrap_or(url).split(['/', '?', '#']).next().unwrap_or_default().to_owned();
    if host.is_empty() {
        bail!("{url:?} has no host");
    }
    let token = match &via {
        Via::Aws(ctx) => protection::member_token(ctx, &host).await?,
        Via::Api(client) => {
            let answer: Value = client.post("protection/token", json!({"host": host})).await?;
            answer["token"].as_str().context("no token in the answer")?.to_owned()
        }
    };
    let status = std::process::Command::new("curl")
        .arg("-H")
        .arg(format!("cookie: _forkrail_jwt={token}"))
        .args(args)
        .arg(url)
        .status()
        .context("running curl")?;
    std::process::exit(status.code().unwrap_or(1));
}
