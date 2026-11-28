//! What the guard decides, apart from AWS (main.rs reads and acts): each project's spend this month
//! as AWS's own numbers show it, against the budgets in the routing table.
//!
//! AWS's figure is a floor, never more than was spent: Lambda's Duration (it leaves out start-up)
//! at the arm64 price and the smallest memory size any of the project's deployments has, and for
//! the install the internet egress Global Accelerator counted at EC2's price alone. So a budget
//! the floor reaches is surely reached, and the guard never pauses on a guess. The routers pause
//! long before (within a second, from their own ledger); the guard acts only when they haven't:
//! a router down or its meter wrong.

use std::collections::{BTreeMap, BTreeSet};

use forkrail_common::prices::{self, Meter, Nanos};
use forkrail_common::routes::Routes;
use serde::{Deserialize, Serialize};

/// What a project's function and memory are, from the routing table.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub project: String,
    /// The smallest memory size of its deployments, in MB.
    pub memory_mb: u32,
}

/// Each function the routing table names (`forkrail-app-<project>`), with its project and the
/// smallest memory size among its deployments.
pub fn functions(routes: &Routes) -> BTreeMap<String, Function> {
    let mut out: BTreeMap<String, Function> = BTreeMap::new();
    for d in routes.deployments.values() {
        if d.function.is_empty() || d.memory_mb == 0 {
            continue;
        }
        out.entry(d.function.clone())
            .and_modify(|f| f.memory_mb = f.memory_mb.min(d.memory_mb))
            .or_insert(Function { project: d.project.clone(), memory_mb: d.memory_mb });
    }
    out
}

/// A function's month of Duration in one region, as CloudWatch summed it (ms).
#[derive(Debug, Clone, PartialEq)]
pub struct Duration {
    pub function: String,
    pub region: String,
    pub ms: f64,
}

/// Each project's floor this month, from the functions' durations.
pub fn floor(functions: &BTreeMap<String, Function>, durations: &[Duration]) -> BTreeMap<String, Nanos> {
    let mut out: BTreeMap<String, Nanos> = BTreeMap::new();
    for d in durations {
        let Some(f) = functions.get(&d.function) else { continue };
        let mb_ms = (d.ms.max(0.0) * f64::from(f.memory_mb)) as u64;
        *out.entry(f.project.clone()).or_default() += prices::cost(Meter::FnMbMs, &d.region, None, mb_ms);
    }
    out
}

/// The install's egress floor: bytes Global Accelerator sent visitors, at EC2's internet price
/// alone (the accelerator's premium comes on top).
pub fn egress_floor(bytes: f64, region: &str) -> Nanos {
    prices::cost(Meter::FnEgressBytes, region, None, bytes.max(0.0) as u64)
}

/// Why the guard pauses a project.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Why {
    ProjectBudget,
    InstallBudget,
}

/// The projects the guard pauses: not paused in the table yet, and at a budget by AWS's floor.
/// `install_extra`: the install's spend no project carries (egress).
pub fn decide(routes: &Routes, floor: &BTreeMap<String, Nanos>, install_extra: Nanos) -> Vec<(String, Why)> {
    let paused = |p: &str| routes.projects.get(p).and_then(|r| r.spend.as_ref()).is_some_and(|s| s.paused.is_some());
    let exempt = |p: &str| routes.projects.get(p).and_then(|r| r.spend.as_ref()).is_some_and(|s| s.exempt);
    let mut out = Vec::new();
    let mut done = BTreeSet::new();
    for (project, r) in &routes.projects {
        if let Some(budget) = r.spend.as_ref().and_then(|s| s.budget_n)
            && floor.get(project).copied().unwrap_or(0) >= budget
            && !paused(project)
        {
            out.push((project.clone(), Why::ProjectBudget));
            done.insert(project.clone());
        }
    }
    let total: Nanos = floor.values().sum::<Nanos>() + install_extra;
    if let Some(budget) = routes.spend.as_ref().map(|s| s.budget_n)
        && total >= budget
    {
        // Every project the table knows of, and every one AWS shows spending.
        let all: BTreeSet<&String> = routes.deployments.values().map(|d| &d.project).chain(floor.keys()).collect();
        for project in all {
            if project.is_empty() || done.contains(project) || paused(project) || exempt(project) {
                continue;
            }
            out.push((project.clone(), Why::InstallBudget));
        }
    }
    out
}

/// What a router publishes of its ledger (`state/spend/<region>.json`): every router's month to
/// date as it last read it, per project, in all and of function time alone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    /// Unix seconds when written.
    pub at: u64,
    /// `202611`.
    pub month: u32,
    /// The first hour it has this month (unix seconds): later than the month's first, the ledger
    /// began mid-month (an install's first), and AWS's month can't be held against it.
    #[serde(default)]
    pub since: Option<u64>,
    pub projects: BTreeMap<String, LedgerProject>,
}

impl Ledger {
    /// Whether it has the whole month so far.
    pub fn whole(&self) -> bool {
        self.since.is_some_and(|s| s <= month_start(self.month) + 3600)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LedgerProject {
    pub total_n: Nanos,
    pub fn_n: Nanos,
}

/// A gap: AWS's floor of a project's function time over what the ledger counted.
#[derive(Debug, Clone, PartialEq)]
pub struct Gap {
    pub project: String,
    pub aws_n: Nanos,
    pub ledger_n: Nanos,
}

/// A dollar and a tenth: below either, a gap is noise (the metrics lag a few minutes, and an
/// invocation's Duration comes only when it ends, up to 15 minutes later).
const GAP_FLOOR_N: Nanos = 1_000_000_000;
const GAP_SHARE: f64 = 0.10;

/// Projects whose AWS floor of function time is over the ledger's by more than 10% and $1: the
/// meter is missing something, said to the team. `ledger`: the freshest router's.
pub fn gaps(floor: &BTreeMap<String, Nanos>, ledger: &Ledger) -> Vec<Gap> {
    floor
        .iter()
        // The install's own time (the builders, under no project) is no router's to meter: builds
        // count from their records.
        .filter(|(project, _)| !project.is_empty())
        .filter_map(|(project, aws_n)| {
            let ledger_n = ledger.projects.get(project).map_or(0, |p| p.fn_n);
            let over = aws_n - ledger_n;
            (over > GAP_FLOOR_N && (*aws_n as f64) > ledger_n as f64 * (1.0 + GAP_SHARE)).then(|| Gap { project: project.clone(), aws_n: *aws_n, ledger_n })
        })
        .collect()
}

/// The freshest of the routers' ledgers this month, if any.
pub fn freshest(ledgers: Vec<Ledger>, month: u32) -> Option<Ledger> {
    ledgers.into_iter().filter(|l| l.month == month).max_by_key(|l| l.at)
}

/// Whether AWS's bill line (Cost Explorer's usage type, e.g. `USW1-Lambda-GB-Second-ARM`) is one
/// the team's spend counts: function time and requests, data transfer of every kind (the
/// accelerator's too), S3 and KMS requests (the ledger), and what's kept in S3, the registry and
/// the tables, with the tables' units (forkrail's own, `own.rs`). Fixed costs (instance hours,
/// disks, snapshots, addresses, keys, CPU credits, alarms, metrics) aren't in budgets, so they're
/// left out of the comparison.
pub fn metered(usage_type: &str) -> bool {
    let u = usage_type;
    if u.contains("Lambda-Web-") || u.contains("Lambda-Storage") || u.contains("Provisioned") {
        return false;
    }
    u.contains("Lambda-GB-Second")
        || u.ends_with("Request")
        || u.ends_with("Request-ARM")
        || u.contains("Bytes")
        || u.contains("KMS-Requests")
        || u.contains("Requests-Tier")
        || u.contains("TimedStorage-ByteHrs")
        || u.contains("TimedPITRStorage-ByteHrs")
        || u.ends_with("ReadRequestUnits")
        || u.ends_with("WriteRequestUnits")
}

/// What the guard keeps to compare AWS's bill with the ledger a day at a time: the ledger's
/// month-to-date total (every router's, as the freshest publishes it) when each UTC day began,
/// and each finished day's total from that.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Days {
    /// The day being counted (unix days) and the ledger's total when it began; 0: none yet.
    pub day: u64,
    pub start_n: Nanos,
    pub month: u32,
    /// The last total seen, and its month (a month's last day ends on it).
    pub last_n: Nanos,
    pub last_month: u32,
    /// Whether `day` was seen from its start: the first day the guard sees is only part of one.
    pub whole: bool,
    /// Each finished day's ledger total.
    pub done: BTreeMap<u64, Nanos>,
    /// The last day held against the bill.
    pub checked: u64,
}

impl Days {
    /// A run at `today` (unix days) saw the ledger's month to date `total_n` for `month`.
    pub fn saw(&mut self, today: u64, total_n: Nanos, month: u32) {
        if self.day == 0 {
            *self = Days { day: today, start_n: total_n, month, last_n: total_n, last_month: month, whole: false, ..std::mem::take(self) };
            return;
        }
        if today > self.day {
            // The day just ended went from its start to the last total of its month.
            if self.whole && today == self.day + 1 && self.last_month == self.month {
                self.done.insert(self.day, (self.last_n - self.start_n).max(0));
            }
            // A new month's total starts again from nothing.
            let start = if month == self.last_month { self.last_n } else { 0 };
            self.day = today;
            self.start_n = start;
            self.month = month;
            self.whole = true;
            let keep = today.saturating_sub(40);
            self.done.retain(|d, _| *d >= keep);
        }
        self.last_n = total_n;
        self.last_month = month;
    }
}

/// A day's bill against the ledger: AWS's metered lines over what the ledger counted by more than
/// 10% and $1 (the ledger takes no free allowances off, so it's usually the higher).
pub fn bill_gap(bill_n: Nanos, ledger_n: Nanos) -> bool {
    bill_n - ledger_n > GAP_FLOOR_N && (bill_n as f64) > ledger_n as f64 * (1.0 + GAP_SHARE)
}

/// `YYYY-MM-DD` of a unix day.
pub fn date_of(day: u64) -> String {
    let month = month_of(day * 86_400);
    let first = month_start(month) / 86_400;
    format!("{}-{:02}-{:02}", month / 100, month % 100, day - first + 1)
}

pub use forkrail_common::prices::{month_of, month_start};

#[cfg(test)]
mod tests {
    use super::*;
    use forkrail_common::routes::{InstallSpend, ProjectRouting, ProjectSpend, SpendPause};

    const D: Nanos = 1_000_000_000;

    fn routes() -> Routes {
        serde_json::from_value(serde_json::json!({
            "deployments": {
                "a1": {"project": "shop", "function": "forkrail-app-shop", "qualifier": "3", "memory_mb": 1024, "max_duration_s": 300},
                "a2": {"project": "shop", "function": "forkrail-app-shop", "qualifier": "4", "memory_mb": 2048, "max_duration_s": 300},
                "b1": {"project": "blog", "function": "forkrail-app-blog", "qualifier": "1", "memory_mb": 512, "max_duration_s": 300},
                "w1": {"project": "forkrail-web", "function": "forkrail-app-forkrail-web", "qualifier": "9", "memory_mb": 1024, "max_duration_s": 300},
                "s1": {"project": "site", "function": "", "qualifier": "", "memory_mb": 0, "max_duration_s": 0}
            },
            "hosts": {}
        }))
        .unwrap()
    }

    #[test]
    fn the_floor_takes_the_smallest_memory() {
        let f = functions(&routes());
        assert_eq!(f["forkrail-app-shop"].memory_mb, 1024);
        assert!(!f.contains_key(""), "static files have no function");
        // An hour of one 1 GB environment: 3.6M ms × 1024 MB, $0.048.
        let floor = floor(&f, &[Duration { function: "forkrail-app-shop".into(), region: "us-east-1".into(), ms: 3_600_000.0 }]);
        assert!((floor["shop"] - 48_000_240).abs() < 1000, "{}", floor["shop"]);
    }

    #[test]
    fn a_budget_the_floor_reaches_pauses() {
        let mut r = routes();
        r.spend = Some(InstallSpend { budget_n: 100 * D, auto: false, stepped: None });
        r.projects.insert("shop".into(), ProjectRouting { spend: Some(ProjectSpend { budget_n: Some(5 * D), ..Default::default() }), ..Default::default() });
        r.projects.insert("forkrail-web".into(), ProjectRouting { spend: Some(ProjectSpend { exempt: true, ..Default::default() }), ..Default::default() });
        let floor: BTreeMap<String, Nanos> = [("shop".to_owned(), 5 * D), ("blog".to_owned(), 10 * D)].into();
        assert_eq!(decide(&r, &floor, 0), [("shop".to_owned(), Why::ProjectBudget)]);
        // The install's reached: every project but the exempt, and none twice.
        assert_eq!(
            decide(&r, &floor, 85 * D),
            [("shop".to_owned(), Why::ProjectBudget), ("blog".to_owned(), Why::InstallBudget), ("site".to_owned(), Why::InstallBudget)]
        );
        // Already paused in the table: control has it.
        r.projects.get_mut("shop").unwrap().spend.as_mut().unwrap().paused = Some(SpendPause { at: "t".into(), reason: "project_budget".into(), reserved: None });
        assert_eq!(decide(&r, &floor, 0), []);
        // No budgets, nothing.
        assert_eq!(decide(&routes(), &floor, 1000 * D), []);
    }

    #[test]
    fn what_the_bill_and_the_ledger_both_count() {
        for yes in [
            "Lambda-GB-Second-ARM",
            "USW1-Request-ARM",
            "DataTransfer-Out-Bytes",
            "USW1-USE1-AWS-Out-Bytes",
            "NA-EU-OUT-Bytes-Internet",
            "us-east-1-KMS-Requests",
            "Requests-Tier1",
            // What's kept, and the tables' units: forkrail's own (own.rs).
            "TimedStorage-ByteHrs",
            "USW1-TimedStorage-ByteHrs",
            "TimedPITRStorage-ByteHrs",
            "ReadRequestUnits",
            "WriteRequestUnits",
        ] {
            assert!(metered(yes), "{yes}");
        }
        for no in ["EBS:VolumeUsage.gp3", "EBS:SnapshotUsage", "CPUCredits:t4g", "us-east-1-KMS-Keys", "BoxUsage:t4g.small", "PublicIPv4:InUseAddress", "Health-Check-Option-AWS", "CW:GMD-Metrics", "Lambda-Web-vCPU-Second-ARM"] {
            assert!(!metered(no), "{no}");
        }
        assert!(bill_gap(5 * D, 3 * D));
        assert!(!bill_gap(3 * D + D / 2, 3 * D), "under a dollar over");
        assert!(!bill_gap(30 * D, 28 * D), "within 10%");
        assert!(!bill_gap(2 * D, 9 * D), "the ledger higher: free allowances");
    }

    #[test]
    fn a_day_of_the_ledger() {
        let day = month_start(202611) / 86_400 + 18; // 2026-11-19
        assert_eq!(date_of(day), "2026-11-19");
        let mut d = Days::default();
        d.saw(day, 10 * D, 202611); // first seen mid-day: not whole
        d.saw(day, 12 * D, 202611);
        d.saw(day + 1, 13 * D, 202611);
        assert!(d.done.is_empty(), "the first day was only partly seen");
        d.saw(day + 1, 20 * D, 202611);
        d.saw(day + 2, 21 * D, 202611);
        assert_eq!(d.done.get(&(day + 1)), Some(&(8 * D)), "from its start (12) to its last (20)");
        // A month's last day, then the 1st: the new month starts from nothing.
        let last = month_start(202612) / 86_400 - 1;
        let mut m = Days { day: last, start_n: 100 * D, month: 202611, last_n: 100 * D, last_month: 202611, whole: true, ..Days::default() };
        m.saw(last, 140 * D, 202611);
        m.saw(last + 1, D, 202612);
        assert_eq!(m.done.get(&last), Some(&(40 * D)));
        m.saw(last + 1, 5 * D, 202612);
        m.saw(last + 2, 6 * D, 202612);
        assert_eq!(m.done.get(&(last + 1)), Some(&(5 * D)), "the 1st from zero");
    }

    #[test]
    fn a_gap_is_more_than_noise() {
        let ledger = Ledger { at: 1, month: 202611, since: Some(month_start(202611)), projects: [("shop".to_owned(), LedgerProject { total_n: 9 * D, fn_n: 5 * D })].into() };
        let floor: BTreeMap<String, Nanos> = [("shop".to_owned(), 5 * D + D / 2), ("blog".to_owned(), 2 * D), ("tiny".to_owned(), D / 2)].into();
        let g = gaps(&floor, &ledger);
        assert_eq!(g, [Gap { project: "blog".into(), aws_n: 2 * D, ledger_n: 0 }], "shop's 10% is within; tiny is under a dollar");
        assert!(ledger.whole());
        assert!(!Ledger { since: Some(month_start(202611) + 19 * 86_400), ..ledger.clone() }.whole(), "begun mid-month");
        assert!(!Ledger { since: None, ..ledger }.whole());
    }

}
