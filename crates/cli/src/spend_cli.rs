//! `forkrail spend …`: the install's monthly budget, projects' own, pauses and resumes (spend.rs).
//! Signed in, each op goes through the dashboard's API as you; with `--aws`, straight to the
//! install.

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::Value;

use crate::routing_cli::Via;
use crate::spend::{self, SpendOp, View};

#[derive(Subcommand)]
pub enum SpendCommand {
    /// The budgets, and what's paused.
    Show,
    /// Set the team's monthly budget, in dollars: usage-based spend in the calendar month (UTC),
    /// fixed costs left out. At it, every project is paused but those kept running.
    Budget {
        dollars: f64,
        /// A project's own budget instead (the team budget still applies).
        #[arg(long)]
        project: Option<String>,
    },
    /// Remove PROJECT's own budget.
    #[command(name = "no-budget")]
    NoBudget { project: String },
    /// Keep PROJECT running when the team budget is reached (the dashboard), or not.
    Exempt {
        project: String,
        #[arg(long)]
        off: bool,
    },
    /// Pause projects now: no function runs and no file is served until they're resumed.
    Pause {
        #[arg(required = true)]
        projects: Vec<String>,
    },
    /// Resume projects; with none named, every paused project but those after --keep. Raise the
    /// budget first: one still at or over its budget is paused again within a second.
    Resume {
        projects: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        keep: Vec<String>,
    },
    /// What `forkrail install` sets once the dashboard is up: the default team budget, unless one
    /// is set, and the dashboard kept running at it.
    #[command(hide = true)]
    Start,
}

pub async fn run(via: Via<'_>, command: SpendCommand) -> Result<()> {
    let op = match command {
        SpendCommand::Show => SpendOp::Get,
        SpendCommand::Budget { dollars, project: None } => SpendOp::SetInstall { dollars },
        SpendCommand::Budget { dollars, project: Some(project) } => SpendOp::SetProject { project, dollars: Some(dollars) },
        SpendCommand::NoBudget { project } => SpendOp::SetProject { project, dollars: None },
        SpendCommand::Exempt { project, off } => SpendOp::Exempt { project, exempt: !off },
        SpendCommand::Pause { projects } => SpendOp::Pause { projects },
        SpendCommand::Resume { projects, keep } => SpendOp::Resume { projects: (!projects.is_empty()).then_some(projects), keep },
        SpendCommand::Start => SpendOp::Start,
    };
    spend::print_view(&send(&via, op).await?);
    Ok(())
}

async fn send(via: &Via<'_>, op: SpendOp) -> Result<View> {
    match via {
        Via::Aws(ctx) => spend::run(ctx, op).await,
        Via::Api(client) => {
            let body = serde_json::to_value(&op)?;
            let answer: Value = if op.changes() { client.post("spend", body).await? } else { client.ask("spend", body).await? };
            serde_json::from_value(answer).context("the spend settings, as the install gave them")
        }
    }
}
