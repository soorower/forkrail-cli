//! forkrail-guard (deploy/install/guard.yaml): spend protection's second line (C15 P4,
//! `docs/spend-protection.md`). Every few minutes it reads AWS's own numbers, apart from the
//! routers' ledger, and pauses a project whose budget they show reached, in case a router is down
//! or its meter wrong. It shares nothing with the routers but the price table and the routing
//! table it reads the budgets from.
//!
//! Each run:
//! - **The cheap look** (two metrics): Lambda's Duration over the whole account in each region
//!   since the last full read, at the dearest memory any forkrail deployment has: the most
//!   forkrail's functions can have added since.
//! - **A full read** when that could reach a budget, or an hour after the last: each forkrail
//!   function's Duration this month in each region (one metric each), priced as a floor
//!   (`check.rs`), and the egress Global Accelerator counted (`FORKRAIL_ACCELERATOR`).
//! - **A budget the floor reaches** and the table doesn't show paused: the function's reserved
//!   concurrency goes to 0 in every region (Lambda refuses every invocation, whoever asks), control
//!   is told on its queue (it records the pause, so the routers refuse the project's hosts too),
//!   and the team hears of it.
//! - **A floor over the ledger** the routers publish (`state/spend/<region>.json`) by more than
//!   10% and $1, or a ledger that's gone stale while AWS shows functions running: the team hears
//!   that the meter is missing something (`spend.blind`).
//! - **Once a day** (from noon UTC): AWS's bill for yesterday (Cost Explorer, what's tagged
//!   project=forkrail; needs the cost allocation tag active) against what the ledger counted that
//!   day, the lines it meters only. More on the bill is said the same way.
//! - **forkrail's own costs** with each full read (`own.rs`: storage, the tables, control and the
//!   other functions of its own), written to `state/spend/own.json`: no router sees them, so the
//!   routers and the dashboard add them to the team's spend from there, and this run's checks
//!   count them too.
//!
//! Its own state (the last full read) is `state/guard.json`. Invoked by hand with `{"dry_run":
//! true}` it reads and says what it would do, and does nothing. Run outside Lambda (`cargo run -p
//! forkrail-guard --features local`, with the install's AWS credentials) it does the same once:
//! a full read, nothing acted on or written.
//!
//! Settings (environment): `FORKRAIL_STATE_BUCKET`, `FORKRAIL_HOME_REGION`, `FORKRAIL_REGIONS`
//! (comma-separated, the home region's included), `FORKRAIL_SPEND_QUEUE` (control's),
//! `FORKRAIL_EVENTS_QUEUE` (the notifier's), `FORKRAIL_ACCELERATOR` (its id; optional),
//! `FORKRAIL_FULL_EVERY_S` (default 3600).

mod check;
mod own;

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use aws_sdk_cloudwatch::types::{Dimension, Statistic};
use aws_sdk_cloudwatch::primitives::DateTime;
use forkrail_common::install::{BUILDER_FUNCTION, BUILDER_REPOSITORY, OWN_FUNCTIONS, OWN_SPEND_KEY, ROUTES_KEY, STATE_TABLE, WEB_TABLE, function_name};
use forkrail_common::prices::Nanos;
use forkrail_common::routes::Routes;
use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const STATE_KEY: &str = "state/guard.json";
const LEDGER_PREFIX: &str = "state/spend/";
/// A ledger older than this, while AWS shows forkrail's functions running, is a router that's
/// stopped publishing (they write every 5 minutes).
const STALE_AFTER_S: u64 = 20 * 60;

struct Config {
    bucket: String,
    home: String,
    regions: Vec<String>,
    spend_queue: Option<String>,
    events_queue: Option<String>,
    accelerator: Option<String>,
    full_every_s: u64,
}

impl Config {
    fn from_env() -> Result<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let home = var("FORKRAIL_HOME_REGION").unwrap_or_else(|| "us-east-1".into());
        let mut regions: Vec<String> = var("FORKRAIL_REGIONS").map(|r| r.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        if !regions.contains(&home) {
            regions.insert(0, home.clone());
        }
        Ok(Config {
            bucket: var("FORKRAIL_STATE_BUCKET").context("FORKRAIL_STATE_BUCKET")?,
            home,
            regions,
            spend_queue: var("FORKRAIL_SPEND_QUEUE"),
            events_queue: var("FORKRAIL_EVENTS_QUEUE"),
            accelerator: var("FORKRAIL_ACCELERATOR"),
            full_every_s: var("FORKRAIL_FULL_EVERY_S").and_then(|v| v.parse().ok()).unwrap_or(3600).max(300),
        })
    }
}

/// What the guard keeps between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    month: u32,
    /// When the last full read was (unix seconds), and what it found.
    full_at: u64,
    floor: BTreeMap<String, Nanos>,
    egress_n: Nanos,
    /// Events said, so a run doesn't say again what the last did (the notifier would drop it
    /// anyway: the ids are made from what they say).
    #[serde(default)]
    said: Vec<String>,
    /// The ledger a day at a time, for the daily look at AWS's bill.
    #[serde(default)]
    days: check::Days,
    /// forkrail's own costs at the last full read, by part (`own.rs`): a part that can't be read
    /// keeps what it was.
    #[serde(default)]
    own: BTreeMap<String, Nanos>,
}

struct Aws {
    s3: aws_sdk_s3::Client,
    sqs: aws_sdk_sqs::Client,
    base: aws_config::SdkConfig,
}

impl Aws {
    fn cloudwatch(&self, region: &str) -> aws_sdk_cloudwatch::Client {
        let config = aws_sdk_cloudwatch::config::Builder::from(&self.base).region(aws_config::Region::new(region.to_owned())).build();
        aws_sdk_cloudwatch::Client::from_conf(config)
    }

    fn dynamodb(&self) -> aws_sdk_dynamodb::Client {
        aws_sdk_dynamodb::Client::new(&self.base)
    }

    fn ecr(&self) -> aws_sdk_ecr::Client {
        aws_sdk_ecr::Client::new(&self.base)
    }

    fn lambda(&self, region: &str) -> aws_sdk_lambda::Client {
        let config = aws_sdk_lambda::config::Builder::from(&self.base).region(aws_config::Region::new(region.to_owned())).build();
        aws_sdk_lambda::Client::from_conf(config)
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, bucket: &str, key: &str) -> Result<Option<T>> {
        match self.s3.get_object().bucket(bucket).key(key).send().await {
            Ok(out) => {
                let bytes = out.body.collect().await?.into_bytes();
                Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("s3://{bucket}/{key}"))?))
            }
            Err(e) => {
                let missing = e.as_service_error().is_some_and(|s| s.is_no_such_key()) || e.raw_response().is_some_and(|r| r.status().as_u16() == 404 || r.status().as_u16() == 403);
                if missing { Ok(None) } else { Err(anyhow::Error::new(e.into_service_error()).context(format!("s3://{bucket}/{key}"))) }
            }
        }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn at(secs: u64) -> DateTime {
    DateTime::from_secs(secs as i64)
}

/// One metric to read: its id among the answers, and what GetMetricStatistics takes. That call,
/// not GetMetricData, as it's in CloudWatch's free million requests a month (GetMetricData never
/// is: $0.01 per 1,000 metrics). For 50 projects in two regions the guard makes about 90,000 a
/// month, and about 850,000 in a month spent near a budget (everything read every 5 minutes);
/// past the free million, a request costs what a metric of GetMetricData does.
#[derive(Clone)]
struct Query {
    id: String,
    namespace: &'static str,
    metric: &'static str,
    dimensions: Vec<Dimension>,
    period: i32,
    stat: Statistic,
}

impl Query {
    /// The metric's datapoints over [start, end): (the period's start, unix seconds; value), in
    /// order. A call answers at most 1,440 datapoints, so a longer span is read in parts.
    async fn read(&self, cw: &aws_sdk_cloudwatch::Client, start: u64, end: u64) -> Result<Vec<(u64, f64)>> {
        let span = self.period.max(60) as u64 * 1440;
        let mut points = Vec::new();
        let mut from = start;
        while from < end {
            let to = (from + span).min(end);
            let out = cw
                .get_metric_statistics()
                .namespace(self.namespace)
                .metric_name(self.metric)
                .set_dimensions(Some(self.dimensions.clone()))
                .start_time(at(from))
                .end_time(at(to))
                .period(self.period)
                .statistics(self.stat.clone())
                .send()
                .await
                .map_err(|e| anyhow::Error::new(e.into_service_error()).context(format!("CloudWatch {} {}", self.namespace, self.metric)))?;
            for d in out.datapoints() {
                let value = if self.stat == Statistic::Average { d.average() } else { d.sum() };
                if let (Some(t), Some(v)) = (d.timestamp(), value) {
                    points.push((t.secs().max(0) as u64, v));
                }
            }
            from = to;
        }
        points.sort_by_key(|p| p.0);
        Ok(points)
    }
}

/// Each query id's datapoints over [start, end), read a few at a time.
async fn series(cw: &aws_sdk_cloudwatch::Client, queries: Vec<Query>, start: u64, end: u64) -> Result<BTreeMap<String, Vec<(u64, f64)>>> {
    let mut out = BTreeMap::new();
    for batch in queries.chunks(16) {
        let mut reads = tokio::task::JoinSet::new();
        for q in batch {
            let (cw, q) = (cw.clone(), q.clone());
            reads.spawn(async move {
                let points = q.read(&cw, start, end).await;
                (q.id, points)
            });
        }
        while let Some(done) = reads.join_next().await {
            let (id, points) = done.context("a CloudWatch read")?;
            out.insert(id, points?);
        }
    }
    Ok(out)
}

/// Sum of a metric's datapoints for each query id, over [start, end).
async fn sums(cw: &aws_sdk_cloudwatch::Client, queries: Vec<Query>, start: u64, end: u64) -> Result<BTreeMap<String, f64>> {
    Ok(series(cw, queries, start, end).await?.into_iter().map(|(id, points)| (id, points.iter().map(|p| p.1).sum())).collect())
}

fn query(id: &str, namespace: &'static str, metric: &'static str, dimensions: &[(&str, &str)], period: i32) -> Query {
    query_stat(id, namespace, metric, dimensions, period, Statistic::Sum)
}

fn query_stat(id: &str, namespace: &'static str, metric: &'static str, dimensions: &[(&str, &str)], period: i32, stat: Statistic) -> Query {
    Query {
        id: id.to_owned(),
        namespace,
        metric,
        dimensions: dimensions.iter().map(|(n, v)| Dimension::builder().name(*n).value(*v).build()).collect(),
        period,
        stat,
    }
}

/// The full read: each function's Duration this month in each region, and GA's egress.
async fn full_read(aws: &Aws, config: &Config, functions: &BTreeMap<String, check::Function>, month: u32, end: u64) -> Result<(BTreeMap<String, Nanos>, Nanos)> {
    let start = check::month_start(month);
    let mut durations = Vec::new();
    for region in &config.regions {
        let cw = aws.cloudwatch(region);
        let names: Vec<&String> = functions.keys().collect();
        let queries = names.iter().enumerate().map(|(i, f)| query(&format!("f{i}"), "AWS/Lambda", "Duration", &[("FunctionName", f)], 3600)).collect();
        let found = sums(&cw, queries, start, end).await.with_context(|| format!("durations in {region}"))?;
        for (i, f) in names.iter().enumerate() {
            if let Some(ms) = found.get(&format!("f{i}")) {
                durations.push(check::Duration { function: (*f).clone(), region: region.clone(), ms: *ms });
            }
        }
    }
    let floor = check::floor(functions, &durations);
    let egress_n = match &config.accelerator {
        // Global Accelerator's metrics are in us-west-2, whatever the accelerator's regions.
        Some(id) => {
            let found = sums(&aws.cloudwatch("us-west-2"), vec![query("ga", "AWS/GlobalAccelerator", "ProcessedBytesOut", &[("Accelerator", id)], 86_400)], start, end).await?;
            check::egress_floor(found.get("ga").copied().unwrap_or(0.0), &config.home)
        }
        None => 0,
    };
    Ok((floor, egress_n))
}

/// The cheap look: the most forkrail's functions can have cost since `since`, from the account's
/// Duration in each region at the dearest memory size forkrail uses (and x86's price).
async fn grown_at_most(aws: &Aws, config: &Config, functions: &BTreeMap<String, check::Function>, routes: &Routes, since: u64, end: u64) -> Result<(Nanos, f64)> {
    // The builders' too: the large one has 8 GB.
    let max_mb = routes.deployments.values().map(|d| d.memory_mb).max().unwrap_or(0).max(functions.values().map(|f| f.memory_mb).max().unwrap_or(0)).max(8192);
    let mut n = 0;
    let mut account_ms = 0.0;
    for region in &config.regions {
        let found = sums(&aws.cloudwatch(region), vec![query("all", "AWS/Lambda", "Duration", &[], 300)], since.saturating_sub(300), end).await?;
        let ms = found.get("all").copied().unwrap_or(0.0);
        account_ms += ms;
        n += forkrail_common::prices::cost(forkrail_common::prices::Meter::FnX86MbMs, region, None, (ms * f64::from(max_mb)) as u64);
    }
    Ok((n, account_ms))
}

/// forkrail's own costs this month, by part (`own.rs`). A part that can't be read is an error in
/// its place, for the caller to keep the last figure of.
async fn own_read(aws: &Aws, config: &Config, month: u32, end: u64) -> BTreeMap<&'static str, Result<Nanos>> {
    let start = check::month_start(month);
    let mut out = BTreeMap::new();
    out.insert(own::STORAGE, own_storage(aws, config, month, start, end).await);
    out.insert(own::IMAGES, own_images(aws, month, end).await);
    out.insert(own::STATE_TABLE, own_table(aws, config, STATE_TABLE, month, start, end).await);
    out.insert(own::DASHBOARD_TABLE, own_table(aws, config, WEB_TABLE, month, start, end).await);
    out.insert(own::FUNCTIONS, own_functions(aws, config, start, end).await);
    out
}

/// The install's bucket in each region, from S3's daily sizes (in the bucket's region; a few days
/// before the month too, for the size it began with).
async fn own_storage(aws: &Aws, config: &Config, month: u32, start: u64, end: u64) -> Result<Nanos> {
    let mut n = 0;
    for region in &config.regions {
        let bucket = if *region == config.home { config.bucket.clone() } else { format!("{}-{region}", config.bucket) };
        let q = query_stat("size", "AWS/S3", "BucketSizeBytes", &[("BucketName", &bucket), ("StorageType", "StandardStorage")], 86_400, Statistic::Average);
        let found = series(&aws.cloudwatch(region), vec![q], start.saturating_sub(4 * 86_400), end).await.with_context(|| format!("{bucket}'s size"))?;
        n += own::bucket_n(found.get("size").map_or(&[][..], Vec::as_slice), month, end, forkrail_common::prices::s3_gb_month(region));
    }
    Ok(n)
}

/// The builder's images in the registry (the home region's).
async fn own_images(aws: &Aws, month: u32, end: u64) -> Result<Nanos> {
    let mut images = Vec::new();
    let mut token = None;
    loop {
        let page = match aws.ecr().describe_images().repository_name(BUILDER_REPOSITORY).set_next_token(token).send().await {
            Ok(page) => page,
            Err(e) => {
                let e = e.into_service_error();
                // An install without remote builds has no registry.
                if e.is_repository_not_found_exception() {
                    return Ok(0);
                }
                return Err(anyhow::Error::new(e).context("the builder's images"));
            }
        };
        for i in page.image_details() {
            images.push((i.image_pushed_at().map_or(0, |t| t.secs().max(0) as u64), i.image_size_in_bytes().unwrap_or(0) as f64));
        }
        token = page.next_token().map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    Ok(own::images_n(&images, month, end, forkrail_common::prices::ECR_GB_MONTH))
}

/// One table (the home region's): its units this month, the table's and its indexes', and what
/// it keeps now.
async fn own_table(aws: &Aws, config: &Config, table: &str, month: u32, start: u64, end: u64) -> Result<Nanos> {
    let ddb = aws.dynamodb();
    let described = match ddb.describe_table().table_name(table).send().await {
        Ok(out) => out,
        Err(e) => {
            let e = e.into_service_error();
            if e.is_resource_not_found_exception() {
                return Ok(0);
            }
            return Err(anyhow::Error::new(e).context(format!("describing {table}")));
        }
    };
    let Some(t) = described.table() else { return Ok(0) };
    let indexes: Vec<&str> = t.global_secondary_indexes().iter().filter_map(|i| i.index_name()).collect();
    let bytes = t.table_size_bytes().unwrap_or(0) + t.global_secondary_indexes().iter().filter_map(|i| i.index_size_bytes()).sum::<i64>();
    let created = t.creation_date_time().map_or(start, |d| d.secs().max(0) as u64);
    // Point-in-time recovery's status unknown: counted as on, as the install's tables have it.
    let pitr = ddb
        .describe_continuous_backups()
        .table_name(table)
        .send()
        .await
        .ok()
        .and_then(|b| b.continuous_backups_description().and_then(|d| d.point_in_time_recovery_description()).and_then(|p| p.point_in_time_recovery_status().cloned()))
        .is_none_or(|s| s == aws_sdk_dynamodb::types::PointInTimeRecoveryStatus::Enabled);
    let mut queries = Vec::new();
    for (k, index) in std::iter::once(None).chain(indexes.iter().map(Some)).enumerate() {
        let mut dims = vec![("TableName", table)];
        if let Some(i) = index {
            dims.push(("GlobalSecondaryIndexName", i));
        }
        queries.push(query(&format!("r{k}"), "AWS/DynamoDB", "ConsumedReadCapacityUnits", &dims, 3600));
        queries.push(query(&format!("w{k}"), "AWS/DynamoDB", "ConsumedWriteCapacityUnits", &dims, 3600));
    }
    let found = sums(&aws.cloudwatch(&config.home), queries, start, end).await.with_context(|| format!("{table}'s units"))?;
    let of = |prefix: char| found.iter().filter(|(id, _)| id.starts_with(prefix)).map(|(_, v)| v).sum::<f64>();
    Ok(own::table_n(of('r'), of('w'), bytes as f64, created, pitr, month, end))
}

/// forkrail's own functions besides the builders (the home region's).
async fn own_functions(aws: &Aws, config: &Config, start: u64, end: u64) -> Result<Nanos> {
    let lambda = aws.lambda(&config.home);
    let mut found_fns = Vec::new();
    for f in OWN_FUNCTIONS {
        match lambda.get_function_configuration().function_name(f).send().await {
            Ok(c) => {
                let x86 = c.architectures().iter().any(|a| *a == aws_sdk_lambda::types::Architecture::X8664);
                found_fns.push((f, c.memory_size().unwrap_or(128) as u32, c.ephemeral_storage().map_or(512, |e| e.size()) as u32, x86));
            }
            Err(e) => {
                let e = e.into_service_error();
                if !e.is_resource_not_found_exception() {
                    return Err(anyhow::Error::new(e).context(format!("{f}'s configuration")));
                }
            }
        }
    }
    let queries = found_fns
        .iter()
        .enumerate()
        .flat_map(|(i, (f, ..))| [query(&format!("d{i}"), "AWS/Lambda", "Duration", &[("FunctionName", f)], 3600), query(&format!("n{i}"), "AWS/Lambda", "Invocations", &[("FunctionName", f)], 3600)])
        .collect();
    let found = sums(&aws.cloudwatch(&config.home), queries, start, end).await.context("forkrail's own functions")?;
    Ok(found_fns
        .iter()
        .enumerate()
        .map(|(i, (_, memory_mb, scratch_mb, x86))| {
            let get = |k: &str| found.get(&format!("{k}{i}")).copied().unwrap_or(0.0);
            own::function_n(get("d"), get("n"), *memory_mb, *scratch_mb, *x86)
        })
        .sum())
}

/// Whether what could have grown reaches any budget, from the last full read.
fn could_reach(routes: &Routes, state: &State, grown: Nanos) -> bool {
    let install = state.floor.values().sum::<Nanos>() + state.egress_n + state.own.values().sum::<Nanos>() + grown;
    if routes.spend.as_ref().is_some_and(|s| install >= s.budget_n) {
        return true;
    }
    routes.projects.iter().any(|(p, r)| r.spend.as_ref().and_then(|s| s.budget_n).is_some_and(|b| state.floor.get(p).copied().unwrap_or(0) + grown >= b))
}

fn envelope(kind: &str, id: Option<String>, region: &str, payload: Value) -> Value {
    json!({
        "id": id.unwrap_or_else(|| format!("evt_{}", forkrail_common::random_hex(12))),
        "type": kind,
        "createdAt": now() * 1000,
        "payload": payload,
        "region": region,
    })
}

/// An id made from what an event says (the routers make theirs alike: router events.rs `id_of`).
fn id_of(what: &str) -> String {
    use sha2::Digest;
    format!("evt_{}", &forkrail_common::hex(&sha2::Sha256::digest(what.as_bytes()))[..24])
}

async fn send(aws: &Aws, queue: &Option<String>, body: Value) {
    let Some(url) = queue else { return };
    if let Err(e) = aws.sqs.send_message().queue_url(url).message_body(body.to_string()).send().await {
        println!("{}", json!({"warning": "sending failed", "queue": url, "error": aws_sdk_sqs::error::DisplayErrorContext(&e).to_string()}));
    }
}

/// Pause `project` at the AWS level: its function's reserved concurrency to 0 in every region
/// (what it was before goes to control, which puts it back on resume), and control told.
async fn pause(aws: &Aws, config: &Config, project: &str, why: &check::Why, floor_n: Nanos, budget_n: Option<Nanos>, month: u32) -> Vec<String> {
    let function = function_name(project);
    let mut done = Vec::new();
    let mut reserved = None;
    for region in &config.regions {
        let lambda = aws.lambda(region);
        if region == &config.home
            && let Ok(out) = lambda.get_function_concurrency().function_name(&function).send().await
        {
            reserved = out.reserved_concurrent_executions().filter(|n| *n > 0);
        }
        match lambda.put_function_concurrency().function_name(&function).reserved_concurrent_executions(0).send().await {
            Ok(_) => done.push(region.clone()),
            Err(e) => {
                let e = e.into_service_error();
                if !e.is_resource_not_found_exception() {
                    println!("{}", json!({"warning": "concurrency 0 failed", "function": function, "region": region, "error": e.to_string()}));
                }
            }
        }
    }
    let reason = match why {
        check::Why::ProjectBudget => "project_budget",
        check::Why::InstallBudget => "install_budget",
    };
    send(aws, &config.spend_queue, envelope("spend.paused", None, &config.home, json!({"project": project, "reason": "guard", "reserved": reserved, "guardReason": reason, "floorN": floor_n, "budgetN": budget_n, "month": month}))).await;
    done
}

/// One run. `dry_run`: decide and say, act on nothing, write nothing. `full`: read everything
/// whatever the cheap look says.
async fn run(aws: &Aws, config: &Config, dry_run: bool, full: bool) -> Result<Value> {
    let end = now();
    let month = check::month_of(end);
    let routes: Routes = aws.get(&config.bucket, ROUTES_KEY).await?.context("no routing table")?;
    let mut state: State = aws.get(&config.bucket, STATE_KEY).await.ok().flatten().unwrap_or_default();
    if state.month != month {
        state = State { month, days: std::mem::take(&mut state.days), ..State::default() };
    }
    let own_before = std::mem::take(&mut state.own);
    // The routers' ledgers, every run (each UTC day's total needs the one nearest midnight).
    let mut ledgers = Vec::new();
    for region in &config.regions {
        if let Ok(Some(l)) = aws.get::<check::Ledger>(&config.bucket, &format!("{LEDGER_PREFIX}{region}.json")).await {
            ledgers.push(l);
        }
    }
    let freshest = check::freshest(ledgers, month);
    if let Some(l) = &freshest {
        state.days.saw(end / 86_400, l.projects.values().map(|p| p.total_n).sum(), l.month);
    }
    let mut functions = check::functions(&routes);
    // The builders' time is the install's (their memory as the stack sets it is unknown here:
    // the smallest, 4 GB, so it stays a floor).
    for builder in [BUILDER_FUNCTION.to_owned(), format!("{BUILDER_FUNCTION}-large")] {
        functions.insert(builder, check::Function { project: String::new(), memory_mb: 4096 });
    }
    let has_budget = routes.spend.is_some() || routes.projects.values().any(|p| p.spend.as_ref().is_some_and(|s| s.budget_n.is_some()));
    let (grown, account_ms) = if state.full_at > 0 { grown_at_most(aws, config, &functions, &routes, state.full_at, end).await? } else { (0, 0.0) };
    let due = full || state.full_at == 0 || end.saturating_sub(state.full_at) >= config.full_every_s || (has_budget && could_reach(&routes, &state, grown));
    let mut report = json!({"month": month, "full_read": due, "grown_at_most_n": grown, "account_duration_ms": account_ms, "dry_run": dry_run});
    bill_check(aws, config, &mut state, &mut report, end, dry_run).await;
    if !due {
        if !dry_run {
            state.own = own_before;
            put_state(aws, config, &state).await?;
        }
        return Ok(report);
    }
    let (floor, egress_n) = full_read(aws, config, &functions, month, end).await?;
    // forkrail's own costs: a part that can't be read keeps its last figure.
    let mut own_parts = BTreeMap::new();
    let mut own_errors = BTreeMap::new();
    for (part, read) in own_read(aws, config, month, end).await {
        match read {
            Ok(n) => {
                own_parts.insert(part.to_owned(), n);
            }
            Err(e) => {
                own_errors.insert(part, format!("{e:#}"));
                if let Some(n) = own_before.get(part) {
                    own_parts.insert(part.to_owned(), *n);
                }
            }
        }
    }
    let own = own::Own::new(end, month, own_parts);
    let install_n = floor.values().sum::<Nanos>() + egress_n + own.total();
    report["floor_n"] = json!(floor);
    report["egress_floor_n"] = json!(egress_n);
    report["own_n"] = json!(own.parts);
    if !own_errors.is_empty() {
        report["own_errors"] = json!(own_errors);
    }
    report["install_floor_n"] = json!(install_n);
    if !dry_run {
        put_json(aws, config, OWN_SPEND_KEY, &own).await?;
    }

    // Pauses: a budget the floor reaches.
    let to_pause = check::decide(&routes, &floor, egress_n + own.total());
    report["pause"] = json!(to_pause.iter().map(|(p, w)| json!({"project": p, "why": format!("{w:?}")})).collect::<Vec<_>>());
    if !dry_run && !to_pause.is_empty() {
        let mut by_why: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (project, why) in &to_pause {
            let budget_n = match why {
                check::Why::ProjectBudget => routes.projects.get(project).and_then(|r| r.spend.as_ref()).and_then(|s| s.budget_n),
                check::Why::InstallBudget => routes.spend.as_ref().map(|s| s.budget_n),
            };
            let regions = pause(aws, config, project, why, floor.get(project).copied().unwrap_or(0), budget_n, month).await;
            println!("{}", json!({"paused": project, "why": format!("{why:?}"), "regions": regions}));
            by_why.entry(format!("{why:?}")).or_default().push(project.clone());
        }
        for (why, projects) in by_why {
            let install = why == "InstallBudget";
            let budget_n = if install { routes.spend.as_ref().map(|s| s.budget_n) } else { routes.projects.get(&projects[0]).and_then(|r| r.spend.as_ref()).and_then(|s| s.budget_n) };
            let spent_n = if install { install_n } else { floor.get(&projects[0]).copied().unwrap_or(0) };
            let reason = if install { "install_budget" } else { "project_budget" };
            let id = id_of(&format!("spend.paused|guard|{reason}|{month}|{budget_n:?}|{}", projects.join(",")));
            send(aws, &config.events_queue, envelope("spend.paused", Some(id), &config.home, json!({"reason": reason, "by": "guard", "project": (projects.len() == 1 && !install).then(|| projects[0].clone()), "projects": projects, "spentN": spent_n, "budgetN": budget_n, "month": month}))).await;
        }
    }

    // Gaps against the routers' ledgers, and ledgers gone stale.
    let day = end / 86_400;
    let mut said = Vec::new();
    let running = floor.values().sum::<Nanos>() > state.floor.values().sum::<Nanos>() || account_ms > 0.0;
    match &freshest {
        None => report["ledger"] = json!("none published this month"),
        Some(ledger) => {
            if ledger.whole() {
                let gaps = check::gaps(&floor, ledger);
                report["gaps"] = json!(gaps.iter().map(|g| json!({"project": g.project, "aws_n": g.aws_n, "ledger_n": g.ledger_n})).collect::<Vec<_>>());
                for g in gaps {
                    let what = format!("spend.gap|{}|{month}|{day}", g.project);
                    said.push(what.clone());
                    if !dry_run && !state.said.contains(&what) {
                        send(aws, &config.events_queue, envelope("spend.blind", Some(id_of(&what)), &config.home, json!({"source": "guard", "project": g.project, "awsN": g.aws_n, "ledgerN": g.ledger_n, "month": month}))).await;
                    }
                }
            } else {
                report["ledger"] = json!("begun mid-month: AWS's month isn't held against it until the next");
            }
            let stale_s = end.saturating_sub(ledger.at);
            if stale_s > STALE_AFTER_S && running && has_budget {
                let what = format!("spend.stale|{month}|{day}");
                said.push(what.clone());
                report["ledger_stale_s"] = json!(stale_s);
                if !dry_run && !state.said.contains(&what) {
                    send(aws, &config.events_queue, envelope("spend.blind", Some(id_of(&what)), &config.home, json!({"source": "guard", "staleS": stale_s, "month": month}))).await;
                }
            }
        }
    }
    if !dry_run {
        state = State { month, full_at: end, floor, egress_n, said, days: std::mem::take(&mut state.days), own: own.parts.clone() };
        put_state(aws, config, &state).await?;
    }
    Ok(report)
}

/// AWS's bill for `day` (unix days), from Cost Explorer: what's tagged project=forkrail, in all
/// and of the lines the ledger meters (`check::metered`), in nano-dollars, and the metered lines.
async fn bill_of(aws: &Aws, day: u64) -> Result<(Nanos, Nanos, BTreeMap<String, Nanos>)> {
    use aws_sdk_costexplorer::types::{DateInterval, Dimension as CeDimension, Expression, Granularity, GroupDefinition, GroupDefinitionType, TagValues};
    let config = aws_sdk_costexplorer::config::Builder::from(&aws.base).region(aws_config::Region::new("us-east-1")).build();
    let ce = aws_sdk_costexplorer::Client::from_conf(config);
    let period = DateInterval::builder().start(check::date_of(day)).end(check::date_of(day + 1)).build()?;
    let tagged = Expression::builder().tags(TagValues::builder().key("project").values("forkrail").build()).build();
    let by_type = GroupDefinition::builder().r#type(GroupDefinitionType::Dimension).key(CeDimension::UsageType.as_str()).build();
    let (mut total, mut metered, mut lines) = (0, 0, BTreeMap::new());
    let mut token = None;
    loop {
        let page = ce
            .get_cost_and_usage()
            .time_period(period.clone())
            .granularity(Granularity::Daily)
            .metrics("UnblendedCost")
            .filter(tagged.clone())
            .group_by(by_type.clone())
            .set_next_page_token(token)
            .send()
            .await
            .map_err(|e| anyhow::Error::new(e.into_service_error()).context("Cost Explorer GetCostAndUsage"))?;
        for result in page.results_by_time() {
            for group in result.groups() {
                let usage = group.keys().first().cloned().unwrap_or_default();
                let n = group.metrics().and_then(|m| m.get("UnblendedCost")).and_then(|v| v.amount()).and_then(|a| a.parse::<f64>().ok()).map_or(0, |d| (d * 1e9).round() as Nanos);
                total += n;
                if check::metered(&usage) {
                    metered += n;
                    *lines.entry(usage).or_insert(0) += n;
                }
            }
        }
        token = page.next_page_token().map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    Ok((total, metered, lines))
}

/// Once a day, from noon UTC (Cost Explorer has the day before by then, near enough): AWS's bill
/// for yesterday against the ledger's. More on the bill than the ledger counted (by over 10% and
/// $1) is something the meter misses, said to the team. Each look costs $0.01.
async fn bill_check(aws: &Aws, config: &Config, state: &mut State, report: &mut Value, end: u64, dry_run: bool) {
    let today = end / 86_400;
    let yesterday = today - 1;
    if end % 86_400 < 12 * 3600 || state.days.checked >= yesterday {
        return;
    }
    let Some(&ledger_n) = state.days.done.get(&yesterday) else { return };
    let date = check::date_of(yesterday);
    match bill_of(aws, yesterday).await {
        Ok((0, _, _)) => {
            report["bill"] = json!({"date": date, "note": "nothing tagged project=forkrail on the bill: the cost allocation tag isn't active (Billing, Cost allocation tags)"});
            state.days.checked = yesterday;
        }
        Ok((total_n, bill_n, lines)) => {
            let gap = check::bill_gap(bill_n, ledger_n);
            report["bill"] = json!({"date": date, "total_n": total_n, "metered_n": bill_n, "ledger_n": ledger_n, "gap": gap, "lines": lines});
            if gap && !dry_run {
                let what = format!("spend.bill|{date}");
                send(aws, &config.events_queue, envelope("spend.blind", Some(id_of(&what)), &config.home, json!({"source": "bill", "date": date, "billN": bill_n, "ledgerN": ledger_n}))).await;
            }
            state.days.checked = yesterday;
        }
        Err(e) => report["bill"] = json!({"date": date, "error": format!("{e:#}")}),
    }
}

async fn put_state(aws: &Aws, config: &Config, state: &State) -> Result<()> {
    put_json(aws, config, STATE_KEY, state).await
}

async fn put_json(aws: &Aws, config: &Config, key: &str, value: &impl Serialize) -> Result<()> {
    aws.s3
        .put_object()
        .bucket(&config.bucket)
        .key(key)
        .content_type("application/json")
        .body(serde_json::to_vec(value)?.into())
        .send()
        .await
        .map_err(|e| anyhow::Error::new(e.into_service_error()).context(format!("writing s3://{}/{key}", config.bucket)))?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let config = Config::from_env()?;
    let base = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(config.home.clone()))
        .timeout_config(aws_config::timeout::TimeoutConfig::builder().operation_timeout(Duration::from_secs(20)).build())
        .load()
        .await;
    let aws = Aws { s3: aws_sdk_s3::Client::new(&base), sqs: aws_sdk_sqs::Client::new(&base), base };
    let (aws, config) = (&aws, &config);
    if std::env::var("AWS_LAMBDA_RUNTIME_API").is_err() {
        // Outside Lambda: one look, acting on nothing.
        let mut report = run(aws, config, true, true).await?;
        // And yesterday's bill, whatever the ledger has (one Cost Explorer call, $0.01).
        let yesterday = now() / 86_400 - 1;
        report["bill_yesterday"] = match bill_of(aws, yesterday).await {
            Ok((total_n, metered_n, lines)) => json!({"date": check::date_of(yesterday), "total_n": total_n, "metered_n": metered_n, "lines": lines}),
            Err(e) => json!({"error": format!("{e:#}")}),
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    lambda_runtime::run(service_fn(|event: LambdaEvent<Value>| async move {
        let dry_run = event.payload.get("dry_run").and_then(Value::as_bool) == Some(true);
        let started = std::time::Instant::now();
        let out = match run(aws, config, dry_run, false).await {
            Ok(mut report) => {
                report["ms"] = json!(started.elapsed().as_millis() as u64);
                report
            }
            Err(e) => json!({"error": format!("{e:#}"), "ms": started.elapsed().as_millis() as u64}),
        };
        println!("{out}");
        Ok::<_, Error>(out)
    }))
    .await
}
