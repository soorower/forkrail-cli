//! Spend protection's settings (C15, `docs/deploying.md` "Spend"): the install's monthly budget,
//! projects' own, and pauses. The routers count and enforce (crates/router/src/budgets.rs); this
//! writes what they read, in `state/routes.json`, and does what only AWS can:
//!
//! - A pause is recorded in the table (so only a person resumes it: raising a budget doesn't),
//!   and the project's function gets a reserved concurrency of 0 in every region, so Lambda
//!   itself refuses any invocation, whoever asks.
//! - A resume clears both. One that leaves a project at or over a budget doesn't stick: the
//!   routers pause it again within a second, so the view says to raise the budget first.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use forkrail_common::routes::{BuildSpend, InstallSpend, ProjectSpend, Routes, SpendPause};
use serde::{Deserialize, Serialize};

use crate::deploy::Ctx;
use crate::state::ROUTES_KEY;

/// Dollars → nano-dollars.
const NANO: f64 = 1e9;
/// A new install's budget, in dollars, while nobody has set one (it grows, `docs/spend-protection.md`).
pub const DEFAULT_INSTALL_DOLLARS: f64 = 20.0;
/// The largest budget anyone may type: past it, it's a typo.
const MAX_DOLLARS: f64 = 10_000_000.0;

/// What the dashboard, the CLI and the API ask of spend protection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SpendOp {
    /// The budgets and pauses.
    Get,
    /// The install's monthly budget, in dollars (from then on it no longer grows by itself).
    SetInstall { dollars: f64 },
    /// A project's own monthly budget in dollars; None removes it (the install's still applies).
    SetProject { project: String, dollars: Option<f64> },
    /// Whether the install's budget pauses the project (`false` for the dashboard).
    Exempt { project: String, exempt: bool },
    /// Pause projects by hand.
    Pause { projects: Vec<String> },
    /// Resume `projects`, or with None every paused project but those in `keep`.
    Resume {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        projects: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keep: Vec<String>,
    },
    /// What a router or the guard found (`spend.paused` on control's queue): recorded, and AWS
    /// told. `reserved`: the function's reserved concurrency before the guard set it to 0.
    Paused {
        project: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reserved: Option<u32>,
    },
    /// A month ended (`spend.month_end`, from a router) having spent `spent_n` in all: the
    /// default budget, while nobody has set one, takes a step up for the next month at 90%.
    MonthEnd { month: u32, spent_n: i64 },
    /// A build ended (control, `remote::finished`): what it cost counts toward the project's
    /// month. `build`: the deployment and builder, counted once however often it's said.
    Build { project: String, build: String, month: u32, cost_n: i64 },
    /// A new install's start (`forkrail install`, once the dashboard is up): the default budget
    /// ([`default_install`]) unless one is set, and the dashboard kept running at it, so the
    /// team can always reach it to raise the budget.
    Start,
}

impl SpendOp {
    pub fn changes(&self) -> bool {
        !matches!(self, SpendOp::Get)
    }

    pub fn name(&self) -> &'static str {
        match self {
            SpendOp::Get => "get",
            SpendOp::SetInstall { .. } => "set_install",
            SpendOp::SetProject { .. } => "set_project",
            SpendOp::Exempt { .. } => "exempt",
            SpendOp::Pause { .. } => "pause",
            SpendOp::Resume { .. } => "resume",
            SpendOp::Paused { .. } => "paused",
            SpendOp::MonthEnd { .. } => "month_end",
            SpendOp::Build { .. } => "build",
            SpendOp::Start => "start",
        }
    }

    /// The projects it changes.
    pub fn projects(&self) -> Vec<&str> {
        match self {
            SpendOp::SetProject { project, .. } | SpendOp::Exempt { project, .. } | SpendOp::Paused { project, .. } | SpendOp::Build { project, .. } => vec![project],
            SpendOp::Pause { projects } => projects.iter().map(String::as_str).collect(),
            SpendOp::Resume { projects: Some(p), .. } => p.iter().map(String::as_str).collect(),
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    /// The install's monthly budget, in dollars; None: none set (nothing is paused by it).
    pub install_dollars: Option<f64>,
    /// Whether it's the default that grows by itself.
    pub install_auto: bool,
    pub projects: Vec<ProjectView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectView {
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_dollars: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<SpendPause>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exempt: bool,
    /// What its builds cost this month, in dollars (in no other figure: the routers' ledger
    /// doesn't see builds).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub builds_dollars: f64,
}

fn is_zero(n: &f64) -> bool {
    *n == 0.0
}

/// The last builds a project's count remembers, against a notice said twice.
const COUNTED_BUILDS: usize = 20;

pub fn view(routes: &Routes) -> View {
    let month = forkrail_common::prices::month_of(crate::state::now_secs());
    View {
        install_dollars: routes.spend.as_ref().map(|s| s.budget_n as f64 / NANO),
        install_auto: routes.spend.as_ref().is_some_and(|s| s.auto),
        projects: routes
            .projects
            .iter()
            .filter_map(|(name, p)| {
                let s = p.spend.as_ref()?;
                Some(ProjectView {
                    project: name.clone(),
                    budget_dollars: s.budget_n.map(|n| n as f64 / NANO),
                    paused: s.paused.clone(),
                    exempt: s.exempt,
                    builds_dollars: s.builds_n(month) as f64 / NANO,
                })
            })
            .collect(),
    }
}

fn nanos(dollars: f64) -> Result<i64> {
    if !dollars.is_finite() || dollars < 0.0 || dollars > MAX_DOLLARS {
        bail!("a budget is between $0 and ${MAX_DOLLARS}");
    }
    Ok((dollars * NANO).round() as i64)
}

/// What `apply` did: the projects it paused, and those it resumed with the reserved concurrency
/// each had before its pause.
pub type Moved = (Vec<String>, Vec<(String, Option<u32>)>);

/// The changes `op` makes to the table, in place. `reserved`: each function's reserved
/// concurrency now, for the projects `op` pauses (kept in the pause, put back on resume).
pub fn apply(routes: &mut Routes, op: &SpendOp, now: &str, reserved: &BTreeMap<String, u32>) -> Result<Moved> {
    let (mut paused, mut resumed) = (Vec::new(), Vec::new());
    let entry = |routes: &mut Routes, project: &str, f: &mut dyn FnMut(&mut ProjectSpend)| {
        let p = routes.projects.entry(project.to_owned()).or_default();
        let mut spend = p.spend.take().unwrap_or_default();
        f(&mut spend);
        p.spend = (spend != ProjectSpend::default()).then_some(spend);
        if p.is_empty() {
            routes.projects.remove(project);
        }
    };
    match op {
        SpendOp::Get => {}
        SpendOp::SetInstall { dollars } => routes.spend = Some(InstallSpend { budget_n: nanos(*dollars)?, auto: false, stepped: None }),
        SpendOp::Start => {
            if routes.spend.is_none() {
                routes.spend = Some(default_install());
            }
            entry(routes, forkrail_common::install::DASHBOARD_PROJECT, &mut |s| s.exempt = true);
        }
        SpendOp::MonthEnd { month, spent_n } => {
            if let Some(s) = routes.spend.as_mut()
                && s.auto
                && s.stepped.is_none_or(|m| m < *month)
            {
                s.stepped = Some(*month);
                if *spent_n * 10 >= s.budget_n * 9 {
                    s.budget_n = step_up(s.budget_n);
                }
            }
        }
        SpendOp::Build { project, build, month, cost_n } => {
            entry(routes, project, &mut |s| {
                let b = s.builds.get_or_insert_with(BuildSpend::default);
                if b.month != *month {
                    *b = BuildSpend { month: *month, ..BuildSpend::default() };
                }
                if !b.counted.contains(build) {
                    b.cost_n += cost_n.max(&0);
                    b.counted.push(build.clone());
                    let over = b.counted.len().saturating_sub(COUNTED_BUILDS);
                    b.counted.drain(..over);
                }
            });
        }
        SpendOp::SetProject { project, dollars } => {
            let budget = dollars.map(nanos).transpose()?;
            entry(routes, project, &mut |s| s.budget_n = budget);
        }
        SpendOp::Exempt { project, exempt } => entry(routes, project, &mut |s| s.exempt = *exempt),
        SpendOp::Pause { projects } => {
            for project in projects {
                entry(routes, project, &mut |s| {
                    if s.paused.is_none() {
                        s.paused = Some(SpendPause { at: now.to_owned(), reason: "manual".into(), reserved: reserved.get(project).copied() });
                    }
                });
                paused.push(project.clone());
            }
        }
        SpendOp::Paused { project, reason, .. } => {
            entry(routes, project, &mut |s| {
                if s.paused.is_none() {
                    s.paused = Some(SpendPause { at: now.to_owned(), reason: reason.clone(), reserved: reserved.get(project).copied() });
                }
            });
            paused.push(project.clone());
        }
        SpendOp::Resume { projects, keep } => {
            let all: Vec<String> = routes.projects.iter().filter(|(_, p)| p.spend.as_ref().is_some_and(|s| s.paused.is_some())).map(|(n, _)| n.clone()).collect();
            let chosen: Vec<String> = match projects {
                Some(p) => p.clone(),
                None => all.into_iter().filter(|p| !keep.contains(p)).collect(),
            };
            for project in chosen {
                let mut before = None;
                entry(routes, &project, &mut |s| before = s.paused.take().and_then(|p| p.reserved));
                resumed.push((project, before));
            }
        }
    }
    routes.validate().map_err(anyhow::Error::msg)?;
    Ok((paused, resumed))
}

/// The default budget's next step: $10 more up to $100, then a quarter more (to the dollar).
pub fn step_up(budget_n: i64) -> i64 {
    const DOLLAR: i64 = 1_000_000_000;
    match budget_n < 100 * DOLLAR {
        true => budget_n + 10 * DOLLAR,
        false => (budget_n + budget_n / 4 + DOLLAR - 1) / DOLLAR * DOLLAR,
    }
}

/// A new install's table: the default budget, growing until someone sets one.
pub fn default_install() -> InstallSpend {
    InstallSpend { budget_n: (DEFAULT_INSTALL_DOLLARS * NANO) as i64, auto: true, stepped: None }
}

/// Do `op`: change the table, then what AWS holds (functions' reserved concurrency).
pub async fn run(ctx: &Ctx, op: SpendOp) -> Result<View> {
    for project in op.projects() {
        crate::deploy::check_project(project)?;
    }
    let now = crate::state::now_rfc3339();
    // What a pause will set to 0, so a resume puts it back.
    let mut reserved = BTreeMap::new();
    if let SpendOp::Paused { project, reserved: Some(n), .. } = &op {
        // The guard set it to 0 already, and said what it was.
        reserved.insert(project.clone(), *n);
    } else if matches!(op, SpendOp::Pause { .. } | SpendOp::Paused { .. }) {
        let home = &ctx.lambda_in(&ctx.install.home_region).client;
        for project in op.projects() {
            let function = forkrail_common::install::function_name(project);
            if let Ok(out) = home.get_function_concurrency().function_name(&function).send().await
                && let Some(n) = out.reserved_concurrent_executions().filter(|n| *n > 0)
            {
                reserved.insert(project.to_owned(), n as u32);
            }
        }
    }
    let mut moved: Moved = (Vec::new(), Vec::new());
    let mut raised: Option<(i64, i64)> = None;
    let routes = match op.changes() {
        false => ctx.store.get::<Routes>(ROUTES_KEY).await?.map(|(r, _)| r).unwrap_or_default(),
        true => {
            ctx.store
                .update::<Routes, _>(ROUTES_KEY, |routes| {
                    let mut routes = routes.unwrap_or_default();
                    let before = routes.spend.as_ref().map(|s| s.budget_n);
                    moved = apply(&mut routes, &op, &now, &reserved)?;
                    let after = routes.spend.as_ref().map(|s| s.budget_n);
                    raised = match (&op, before, after) {
                        (SpendOp::MonthEnd { .. }, Some(from), Some(to)) if to > from => Some((from, to)),
                        _ => None,
                    };
                    Ok(routes)
                })
                .await?
        }
    };
    let (paused, resumed) = moved;
    if !paused.is_empty() || !resumed.is_empty() {
        let regions = regions(ctx).await?;
        for project in &paused {
            concurrency(ctx, &regions, project, Some(0)).await;
        }
        for (project, before) in &resumed {
            concurrency(ctx, &regions, project, before.map(|n| n as i32)).await;
        }
    }
    if let (SpendOp::MonthEnd { month, spent_n }, Some((from, to))) = (&op, raised) {
        // The default grew: the team hears what next month's is.
        crate::events::emit(ctx, "budget.raised", serde_json::json!({ "month": month, "spentN": spent_n, "fromN": from, "budgetN": to })).await;
    }
    Ok(view(&routes))
}

/// Every region a project's function may be in: the install's, its home, and every region a
/// project picks for its functions.
async fn regions(ctx: &Ctx) -> Result<BTreeSet<String>> {
    let mut regions: BTreeSet<String> = ctx.store.settings().await?.regions.into_iter().collect();
    regions.insert(ctx.install.home_region.clone());
    for project in crate::project::all(ctx).await? {
        regions.extend(project.regions);
    }
    Ok(regions)
}

/// Set (`Some(0)`) or clear the function's reserved concurrency in each region it's in. A region
/// without it is skipped; a failure is said and left (the routers refuse the project anyway).
async fn concurrency(ctx: &Ctx, regions: &BTreeSet<String>, project: &str, reserved: Option<i32>) {
    let function = forkrail_common::install::function_name(project);
    for region in regions {
        let client = &ctx.lambda_in(region).client;
        let done = match reserved {
            Some(n) => client.put_function_concurrency().function_name(&function).reserved_concurrent_executions(n).send().await.map(|_| ()).map_err(|e| e.into_service_error().to_string()),
            None => client.delete_function_concurrency().function_name(&function).send().await.map(|_| ()).map_err(|e| e.into_service_error().to_string()),
        };
        match done {
            Ok(()) => eprintln!("==> {function} in {region}: reserved concurrency {}", reserved.map_or("cleared".to_owned(), |n| n.to_string())),
            Err(e) if e.contains("ResourceNotFound") || e.contains("Function not found") => {}
            Err(e) => eprintln!("warning: {function} in {region}: setting its concurrency failed: {e}"),
        }
    }
}

/// The view, for a person.
pub fn print_view(v: &View) {
    match v.install_dollars {
        Some(d) => println!("team budget: ${d:.2} a month{}", if v.install_auto { " (the default, growing as it's used)" } else { "" }),
        None => println!("team budget: none set (nothing is paused by it)"),
    }
    for p in &v.projects {
        let budget = p.budget_dollars.map_or("the team's".to_owned(), |d| format!("${d:.2}"));
        let paused = p.paused.as_ref().map_or(String::new(), |s| format!("  PAUSED since {} ({})", s.at, s.reason));
        println!("  {:<28} budget {budget}{}{paused}", p.project, if p.exempt { "  kept running when the team budget is reached" } else { "" });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> BTreeMap<String, u32> {
        BTreeMap::new()
    }

    #[test]
    fn a_new_install_starts_at_the_default_budget_with_the_dashboard_kept_running() {
        let mut routes = Routes::default();
        apply(&mut routes, &SpendOp::Start, "t", &none()).unwrap();
        let v = view(&routes);
        assert_eq!((v.install_dollars, v.install_auto), (Some(DEFAULT_INSTALL_DOLLARS), true));
        assert!(v.projects.iter().any(|p| p.project == forkrail_common::install::DASHBOARD_PROJECT && p.exempt));
        // A budget someone set stays as it is.
        apply(&mut routes, &SpendOp::SetInstall { dollars: 50.0 }, "t", &none()).unwrap();
        apply(&mut routes, &SpendOp::Start, "t", &none()).unwrap();
        assert_eq!((view(&routes).install_dollars, view(&routes).install_auto), (Some(50.0), false));
    }

    #[test]
    fn budgets_and_pauses_in_the_table() {
        let mut routes = Routes::default();
        apply(&mut routes, &SpendOp::SetInstall { dollars: 20.0 }, "t", &none()).unwrap();
        assert_eq!(routes.spend, Some(InstallSpend { budget_n: 20_000_000_000, auto: false, stepped: None }));
        apply(&mut routes, &SpendOp::SetProject { project: "a".into(), dollars: Some(5.5) }, "t", &none()).unwrap();
        apply(&mut routes, &SpendOp::Exempt { project: "web".into(), exempt: true }, "t", &none()).unwrap();
        let reserve: BTreeMap<String, u32> = [("b".to_owned(), 3)].into();
        let (paused, _) = apply(&mut routes, &SpendOp::Pause { projects: vec!["a".into(), "b".into()] }, "t1", &reserve).unwrap();
        assert_eq!(paused, ["a", "b"]);
        let v = view(&routes);
        assert_eq!(v.install_dollars, Some(20.0));
        assert_eq!(v.projects.len(), 3);
        assert_eq!(v.projects[0].budget_dollars, Some(5.5));
        assert_eq!(v.projects[0].paused.as_ref().map(|p| p.reason.as_str()), Some("manual"));
        // Pausing again keeps the first pause's time.
        apply(&mut routes, &SpendOp::Paused { project: "a".into(), reason: "project_budget".into(), reserved: None }, "t2", &none()).unwrap();
        assert_eq!(view(&routes).projects[0].paused.as_ref().map(|p| p.at.as_str()), Some("t1"));
        // Resume all but b.
        let (_, resumed) = apply(&mut routes, &SpendOp::Resume { projects: None, keep: vec!["b".into()] }, "t3", &none()).unwrap();
        assert_eq!(resumed, [("a".to_owned(), None)]);
        assert!(view(&routes).projects.iter().find(|p| p.project == "b").unwrap().paused.is_some());
        // An entry that says nothing more leaves the table.
        apply(&mut routes, &SpendOp::SetProject { project: "a".into(), dollars: None }, "t", &none()).unwrap();
        let (_, resumed) = apply(&mut routes, &SpendOp::Resume { projects: Some(vec!["b".into()]), keep: vec![] }, "t", &none()).unwrap();
        assert_eq!(resumed, [("b".to_owned(), Some(3))], "its reserve, put back");
        assert_eq!(routes.projects.keys().collect::<Vec<_>>(), ["web"]);
    }

    #[test]
    fn the_default_steps_up_when_a_month_comes_close() {
        const D: i64 = 1_000_000_000;
        let mut routes = Routes { spend: Some(default_install()), ..Routes::default() };
        // $17.99 of $20: under 90%, no step.
        apply(&mut routes, &SpendOp::MonthEnd { month: 202611, spent_n: 17_990_000_000 }, "t", &none()).unwrap();
        assert_eq!(routes.spend.as_ref().unwrap().budget_n, 20 * D);
        // The next month's $18 of $20: $30, once however many routers say it.
        for _ in 0..2 {
            apply(&mut routes, &SpendOp::MonthEnd { month: 202612, spent_n: 18 * D }, "t", &none()).unwrap();
        }
        assert_eq!(routes.spend.as_ref().unwrap().budget_n, 30 * D);
        apply(&mut routes, &SpendOp::MonthEnd { month: 202701, spent_n: 27 * D }, "t", &none()).unwrap();
        assert_eq!(routes.spend.as_ref().unwrap().budget_n, 40 * D);
        // An old month said late changes nothing.
        apply(&mut routes, &SpendOp::MonthEnd { month: 202612, spent_n: 40 * D }, "t", &none()).unwrap();
        assert_eq!(routes.spend.as_ref().unwrap().budget_n, 40 * D);
        // Set by a person, it no longer grows.
        apply(&mut routes, &SpendOp::SetInstall { dollars: 40.0 }, "t", &none()).unwrap();
        apply(&mut routes, &SpendOp::MonthEnd { month: 202702, spent_n: 39 * D }, "t", &none()).unwrap();
        assert_eq!(routes.spend.as_ref().unwrap().budget_n, 40 * D);
        assert_eq!(step_up(100 * D), 125 * D);
        assert_eq!(step_up(125 * D), 157 * D);
    }

    #[test]
    fn builds_count_once_each_in_their_month() {
        let mut routes = Routes::default();
        let build = |id: &str, month, n| SpendOp::Build { project: "shop".into(), build: id.into(), month, cost_n: n };
        apply(&mut routes, &build("a1:forkrail-builder", 202611, 30_000_000), "t", &none()).unwrap();
        apply(&mut routes, &build("a1:forkrail-builder", 202611, 30_000_000), "t", &none()).unwrap();
        apply(&mut routes, &build("a1:forkrail-builder-large", 202611, 60_000_000), "t", &none()).unwrap();
        let s = routes.projects["shop"].spend.clone().unwrap();
        assert_eq!((s.builds_n(202611), s.builds_n(202612)), (90_000_000, 0), "the same notice twice counts once; the large builder's run is its own");
        // A new month starts again.
        apply(&mut routes, &build("b2:forkrail-builder", 202612, 10_000_000), "t", &none()).unwrap();
        assert_eq!(routes.projects["shop"].spend.as_ref().unwrap().builds_n(202612), 10_000_000);
        for i in 0..30 {
            apply(&mut routes, &build(&format!("c{i}:forkrail-builder"), 202612, 1), "t", &none()).unwrap();
        }
        assert_eq!(routes.projects["shop"].spend.as_ref().unwrap().builds.as_ref().unwrap().counted.len(), COUNTED_BUILDS);
    }

    #[test]
    fn a_budget_is_a_sane_number() {
        let mut routes = Routes::default();
        for bad in [-1.0, f64::NAN, f64::INFINITY, 1e12] {
            assert!(apply(&mut routes, &SpendOp::SetInstall { dollars: bad }, "t", &none()).is_err());
        }
        assert!(apply(&mut routes, &SpendOp::SetInstall { dollars: 0.0 }, "t", &none()).is_ok());
    }
}
