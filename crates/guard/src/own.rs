//! forkrail's own costs for the team, from AWS's numbers: what runs for the team without passing
//! through a router, so no router can count it.
//!
//! - **Storage:** what the install's bucket keeps in each region (S3's daily `BucketSizeBytes`):
//!   deployments' code, sites' files, sources, build caches, the request log archive, backups; and
//!   the builder's images in the registry.
//! - **The tables:** `forkrail-state` (projects, deployments, domains: read and written by
//!   control, the builder, the CLI and the dashboard) and the dashboard's own (people, sign-ins,
//!   tokens): their read and write units (CloudWatch), and what they keep with point-in-time
//!   recovery's backups.
//! - **forkrail's own functions** besides the builders (control counts each build for its project,
//!   `routes.json` `spend.builds`): control (deploys, promotes, rollbacks, every action in the
//!   dashboard), the notifier, the guard itself, and the one that keeps the cost tag active.
//!
//! Priced as the usage page prices them (web `src/server/storage.ts`, `costs.ts`), at list price,
//! month to date. The dashboard's table is its project's; the rest is the install's (`""`).
//! What has a monthly price and doesn't grow with use (machines, addresses, the accelerator's
//! hours, keys, alarms) isn't here, as it isn't in any budget.

use std::collections::BTreeMap;

use forkrail_common::install::DASHBOARD_PROJECT;
use forkrail_common::prices::{
    DYNAMODB_GB_MONTH, DYNAMODB_PITR_GB_MONTH, DYNAMODB_READ_UNIT, DYNAMODB_WRITE_UNIT, GB_BYTES, LAMBDA_ARM_GB_S, LAMBDA_REQUEST, LAMBDA_SCRATCH_GB_S, LAMBDA_X86_GB_S, Nanos,
    month_start,
};
use serde::{Deserialize, Serialize};

/// What `state/spend/own.json` holds: when it was counted, for which month, each part's month to
/// date, and each project's (the parts added up by whose they are).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Own {
    pub at: u64,
    pub month: u32,
    pub parts: BTreeMap<String, Nanos>,
    pub projects: BTreeMap<String, Nanos>,
}

/// The parts, by name: the dashboard's table is its project's, the rest the install's.
pub const STORAGE: &str = "storage";
pub const IMAGES: &str = "images";
pub const STATE_TABLE: &str = "state_table";
pub const DASHBOARD_TABLE: &str = "dashboard_table";
pub const FUNCTIONS: &str = "functions";

pub fn whose(part: &str) -> &'static str {
    if part == DASHBOARD_TABLE { DASHBOARD_PROJECT } else { "" }
}

impl Own {
    pub fn new(at: u64, month: u32, parts: BTreeMap<String, Nanos>) -> Self {
        let mut projects = BTreeMap::new();
        for (part, n) in &parts {
            *projects.entry(whose(part).to_owned()).or_insert(0) += n;
        }
        Own { at, month, parts, projects }
    }

    pub fn total(&self) -> Nanos {
        self.parts.values().sum()
    }
}

fn next(month: u32) -> u32 {
    if month % 100 == 12 { (month / 100 + 1) * 100 + 1 } else { month + 1 }
}

/// Seconds in `month` (YYYYMM): a month's price is for all of them.
pub fn month_seconds(month: u32) -> u64 {
    month_start(next(month)) - month_start(month)
}

fn nanos(usd: f64) -> Nanos {
    (usd * 1e9).round() as Nanos
}

/// Keeping `bytes` from `from` to `to` (unix seconds) at `per_gb_month`, in `month`.
pub fn kept_n(bytes: f64, from: u64, to: u64, per_gb_month: f64, month: u32) -> Nanos {
    if to <= from || bytes <= 0.0 {
        return 0;
    }
    nanos(bytes / GB_BYTES * per_gb_month * (to - from) as f64 / month_seconds(month) as f64)
}

/// A bucket's month to date from its daily sizes (each day's start, bytes): each size kept until
/// the next one, the last until `now`, and the month's first size from the month's start (the
/// figures trail by a day or two, so the last is usually yesterday's).
pub fn bucket_n(days: &[(u64, f64)], month: u32, now: u64, per_gb_month: f64) -> Nanos {
    let start = month_start(month);
    let mut days = days.to_vec();
    days.sort_by_key(|d| d.0);
    // The size the month began with: the last before its start, if there's one.
    let first = days.iter().rposition(|d| d.0 <= start).unwrap_or(0);
    let days = &days[first..];
    days.iter()
        .enumerate()
        .map(|(i, (t, bytes))| {
            let from = if i == 0 { start } else { (*t).max(start) };
            let to = days.get(i + 1).map_or(now, |d| d.0).min(now);
            kept_n(*bytes, from, to, per_gb_month, month)
        })
        .sum()
}

/// The builder's images, each kept since it was pushed (or the month began): (pushed, bytes).
pub fn images_n(images: &[(u64, f64)], month: u32, now: u64, per_gb_month: f64) -> Nanos {
    let start = month_start(month);
    images.iter().map(|(pushed, bytes)| kept_n(*bytes, (*pushed).max(start), now, per_gb_month, month)).sum()
}

/// A table's month to date: its read and write units, and keeping what it has now since it was
/// made (or the month began), with its backups when point-in-time recovery is on.
pub fn table_n(read_units: f64, write_units: f64, bytes: f64, created: u64, pitr: bool, month: u32, now: u64) -> Nanos {
    let per_gb_month = DYNAMODB_GB_MONTH + if pitr { DYNAMODB_PITR_GB_MONTH } else { 0.0 };
    nanos(read_units * DYNAMODB_READ_UNIT + write_units * DYNAMODB_WRITE_UNIT) + kept_n(bytes, created.max(month_start(month)), now, per_gb_month, month)
}

/// A function's month to date: its time at its memory, its scratch disk past the first 512 MB,
/// and each start.
pub fn function_n(ms: f64, invocations: f64, memory_mb: u32, scratch_mb: u32, x86: bool) -> Nanos {
    let gb_s = if x86 { LAMBDA_X86_GB_S } else { LAMBDA_ARM_GB_S };
    let per_s = f64::from(memory_mb) / 1024.0 * gb_s + f64::from(scratch_mb.saturating_sub(512)) / 1024.0 * LAMBDA_SCRATCH_GB_S;
    nanos(ms / 1000.0 * per_s + invocations * LAMBDA_REQUEST)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOV: u32 = 202611;
    const DAY: u64 = 86_400;
    const GIB: f64 = GB_BYTES;

    #[test]
    fn months_have_their_lengths() {
        assert_eq!(month_seconds(NOV), 30 * DAY);
        assert_eq!(month_seconds(202612), 31 * DAY);
        assert_eq!(month_seconds(202802), 29 * DAY);
    }

    #[test]
    fn a_gb_kept_the_whole_month_costs_its_price() {
        let start = month_start(NOV);
        assert_eq!(kept_n(GIB, start, start + 30 * DAY, 0.023, NOV), 23_000_000);
        assert_eq!(kept_n(GIB, start, start + 15 * DAY, 0.023, NOV), 11_500_000);
        assert_eq!(kept_n(GIB, start + DAY, start, 0.023, NOV), 0);
    }

    #[test]
    fn a_bucket_keeps_each_days_size_until_the_next() {
        let start = month_start(NOV);
        // 10 GB from before the month, 20 GB from the 11th; read on the 21st.
        let days = [(start - 2 * DAY, 10.0 * GIB), (start - DAY, 10.0 * GIB), (start + 10 * DAY, 20.0 * GIB)];
        let n = bucket_n(&days, NOV, start + 20 * DAY, 0.03);
        // 10 GB × 10 days + 20 GB × 10 days = 300 GB-days = 10 GB-months at $0.03.
        assert_eq!(n, 300_000_000);
        // The first figure after the month's start stands for the days before it.
        assert_eq!(bucket_n(&[(start + 5 * DAY, 30.0 * GIB)], NOV, start + 10 * DAY, 0.03), 300_000_000);
        assert_eq!(bucket_n(&[], NOV, start + 10 * DAY, 0.03), 0);
    }

    #[test]
    fn images_count_from_their_push_or_the_month() {
        let start = month_start(NOV);
        let images = [(start - 40 * DAY, 3.0 * GIB), (start + 15 * DAY, 3.0 * GIB)];
        // 3 GB for 30 days + 3 GB for 15 = 4.5 GB-months at $0.10.
        assert_eq!(images_n(&images, NOV, start + 30 * DAY, 0.10), 450_000_000);
    }

    #[test]
    fn a_table_is_its_units_and_what_it_keeps() {
        let start = month_start(NOV);
        // 2M reads, 1M writes, nothing kept.
        assert_eq!(table_n(2e6, 1e6, 0.0, start, true, NOV, start + DAY), 250_000_000 + 625_000_000);
        // 1 GB kept the whole month, made before it, with and without its backups.
        assert_eq!(table_n(0.0, 0.0, GIB, start - DAY, true, NOV, start + 30 * DAY), 450_000_000);
        assert_eq!(table_n(0.0, 0.0, GIB, start - DAY, false, NOV, start + 30 * DAY), 250_000_000);
    }

    #[test]
    fn a_function_is_its_time_its_disk_and_its_starts() {
        // 1,000 s at 1 GB arm64, 2 GB of scratch (1.5 past the free part), 1,000 starts.
        let n = function_n(1_000_000.0, 1000.0, 1024, 2048, false);
        let want = 1000.0 * (LAMBDA_ARM_GB_S + 1.5 * LAMBDA_SCRATCH_GB_S) + 1000.0 * LAMBDA_REQUEST;
        assert_eq!(n, (want * 1e9).round() as Nanos);
        assert!(function_n(1000.0, 0.0, 1024, 512, true) > function_n(1000.0, 0.0, 1024, 512, false));
    }

    #[test]
    fn the_dashboards_table_is_its_project_and_the_rest_the_installs() {
        let parts = BTreeMap::from([(STORAGE.to_owned(), 5), (DASHBOARD_TABLE.to_owned(), 2), (FUNCTIONS.to_owned(), 3)]);
        let own = Own::new(1, NOV, parts);
        assert_eq!(own.projects, BTreeMap::from([(String::new(), 8), (DASHBOARD_PROJECT.to_owned(), 2)]));
        assert_eq!(own.total(), 10);
    }
}
