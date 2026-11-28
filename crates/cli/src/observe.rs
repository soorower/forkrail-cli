//! `forkrail logs`, `stats` and `usage`: the analytics store, read from this machine.
//!
//! `logs` also works for someone signed in without AWS credentials (`forkrail login`): the
//! install's API runs the same queries, by name, as that person (`Source::Api`; the dashboard's
//! copy of them is web/src/server/log-queries.ts, and a query changed here must change there).
//!
//! Each query names a project and a time range, so ClickHouse reads only that project's part of
//! the sorting key (project, time). Lists and details read the raw tables; anything over hours or
//! days reads the rollups (requests_1m, requests_paths_1h, invocations_1h), never the raw rows.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::analytics::Analytics;
use crate::deploy::Ctx;

const FOLLOW_EVERY: Duration = Duration::from_secs(2);
/// Rows can arrive this late (Vector's 2 s batches, two routers): `--follow` looks back this far
/// and skips what it has shown.
const FOLLOW_OVERLAP_US: i64 = 20_000_000;

// AWS list prices, us-east-1, checked 2026-11-04 to 11-07. Estimates, not a bill.
const LAMBDA_GB_SECOND: f64 = 0.000_013_333_4; // arm64
const LAMBDA_REQUEST: f64 = 0.20 / 1e6;
const EGRESS_GB: f64 = 0.09;
const GA_PREMIUM_GB: f64 = 0.015; // Global Accelerator, US/EU
const LAMBDA_ROUTER_GB: f64 = 0.01; // each direction
const VERCEL_ACTIVE_CPU_HOUR: f64 = 0.128;
const VERCEL_MEMORY_GB_HOUR: f64 = 0.0106;
const VERCEL_FLUID_GB: f64 = 2.0; // fluid functions are billed at 2 GB whatever vercel.json says
const VERCEL_INVOCATION: f64 = 0.60 / 1e6;
const VERCEL_CDN_REQUEST: f64 = 2.0 / 1e6;
const VERCEL_FDT_GB: f64 = 0.15;
const VERCEL_FOT_GB: f64 = 0.06;
const GB: f64 = 1_073_741_824.0;

/// Where `logs` reads from.
pub enum Source<'a> {
    /// The analytics store itself, with the install's AWS credentials.
    Direct(Analytics),
    /// The install's API, for someone signed in: it runs the query of that name.
    Api(&'a crate::api::Client),
}

/// A query's parameter: one value, or a list (request ids).
enum Param {
    One(String),
    Many(Vec<String>),
}

fn one(value: impl ToString) -> Param {
    Param::One(value.to_string())
}

impl Source<'_> {
    /// The rows of the query `name` for `project`: `sql` run here, or the API's query of that name.
    /// `{project:String}` is the project either way.
    async fn rows<T: DeserializeOwned>(&self, project: &str, name: &str, sql: &str, params: Vec<(&str, Param)>) -> Result<Vec<T>> {
        match self {
            Source::Direct(db) => {
                let mut text: Vec<(&str, String)> = vec![("project", project.to_owned())];
                for (key, value) in &params {
                    text.push((key, match value {
                        Param::One(v) => v.clone(),
                        Param::Many(list) => array_param(&list.iter().map(String::as_str).collect::<Vec<_>>()),
                    }));
                }
                db.query(sql, &text).await
            }
            Source::Api(client) => {
                #[derive(Deserialize)]
                struct Answer {
                    rows: Vec<serde_json::Value>,
                }
                let params: serde_json::Map<String, serde_json::Value> = params
                    .into_iter()
                    .map(|(key, value)| (key.to_owned(), match value {
                        Param::One(v) => json!(v),
                        Param::Many(list) => json!(list),
                    }))
                    .collect();
                let answer: Answer = client.ask(&format!("projects/{project}/logs/query"), json!({ "query": name, "params": params })).await?;
                answer.rows.into_iter().map(|row| serde_json::from_value(row).context("reading a row")).collect()
            }
        }
    }
}

pub struct LogsArgs {
    pub project: String,
    pub since: String,
    pub until: Option<String>,
    pub status: Option<String>,
    pub path: Option<String>,
    pub host: Option<String>,
    pub limit: u32,
    pub follow: bool,
    pub request: Option<String>,
    pub app: bool,
    pub search: Option<String>,
    pub level: Option<String>,
}

pub async fn logs(db: &Source<'_>, args: LogsArgs) -> Result<()> {
    crate::deploy::check_project(&args.project)?;
    let now = now_us();
    if let Some(id) = &args.request {
        // A request id alone: look back far, which the bloom filter on `request` makes cheap.
        let since = if args.since == "1h" { now - 7 * 86_400_000_000 } else { parse_time(&args.since, now)? };
        return request_detail(db, &args.project, id, since).await;
    }
    let since = parse_time(&args.since, now)?;
    let until = args.until.as_deref().map(|u| parse_time(u, now)).transpose()?.unwrap_or(now + 3_600_000_000);
    if args.app {
        return app_lines(db, &args, since, until).await;
    }
    let (lo, hi) = status_range(args.status.as_deref())?;
    let params = |since: i64, limit: u32| {
        vec![
            ("since", one(since)),
            ("until", one(until)),
            ("lo", one(lo)),
            ("hi", one(hi)),
            ("path", one(args.path.clone().unwrap_or_default())),
            ("host", one(args.host.clone().unwrap_or_default())),
            ("limit", one(limit)),
        ]
    };
    const LIST: &str = "SELECT toUnixTimestamp64Micro(time) AS us, request, status, method, path, query, host,
            total_ms, queue_ms, attempts, outcome, router_region, region, worker
        FROM forkrail.requests
        WHERE project = {project:String}
          AND time > fromUnixTimestamp64Micro({since:Int64}, 'UTC') AND time < fromUnixTimestamp64Micro({until:Int64}, 'UTC')
          AND status BETWEEN {lo:UInt16} AND {hi:UInt16}
          AND startsWith(path, {path:String})
          AND ({host:String} = '' OR host = {host:String})";
    let mut rows: Vec<RequestRow> = db.rows(&args.project, "requests", &format!("{LIST} ORDER BY time DESC LIMIT {{limit:UInt32}}"), params(since, args.limit)).await?;
    rows.reverse();
    println!("{:<23}  {:>6}  {:>7}  {:<9}  {:<6} {:<58} {:>5}", "TIME (UTC)", "STATUS", "MS", "REGION", "METHOD", "PATH", "LINES");
    let lines = line_counts(db, &args.project, &rows).await?;
    for row in &rows {
        print_request(row, lines.get(&row.request));
    }
    if rows.is_empty() && !args.follow {
        println!("no requests for {} since {} UTC", args.project, format_us(since));
    }
    if !args.follow {
        return Ok(());
    }
    let mut seen = Seen::default();
    for row in &rows {
        seen.insert(row.us, row.request.clone());
    }
    let mut last = rows.last().map_or(now - FOLLOW_OVERLAP_US, |r| r.us);
    loop {
        tokio::time::sleep(FOLLOW_EVERY).await;
        let rows: Vec<RequestRow> =
            db.rows(&args.project, "requests_after", &format!("{LIST} ORDER BY time ASC LIMIT 2000"), params(last - FOLLOW_OVERLAP_US, 2000)).await?;
        let fresh: Vec<RequestRow> = rows.into_iter().filter(|r| seen.insert(r.us, r.request.clone())).collect();
        let lines = line_counts(db, &args.project, &fresh).await?;
        for row in &fresh {
            print_request(row, lines.get(&row.request));
            last = last.max(row.us);
        }
    }
}

/// App lines tagged with each request's id: how many, and how many look like errors.
async fn line_counts(db: &Source<'_>, project: &str, rows: &[RequestRow]) -> Result<HashMap<String, (u64, u64)>> {
    let (Some(first), Some(last)) = (rows.iter().map(|r| r.us).min(), rows.iter().map(|r| r.us).max()) else {
        return Ok(HashMap::new());
    };
    #[derive(Deserialize)]
    struct Count {
        request: String,
        lines: u64,
        errors: u64,
    }
    let ids = rows.iter().map(|r| r.request.clone()).collect::<Vec<_>>();
    let counts: Vec<Count> = db
        .rows(
            project,
            "line_counts",
            "SELECT request, count() AS lines, countIf(level = 'error') AS errors FROM forkrail.app_logs
             WHERE project = {project:String}
               AND time >= fromUnixTimestamp64Micro({lo:Int64}, 'UTC') AND time <= fromUnixTimestamp64Micro({hi:Int64}, 'UTC')
               AND request IN {ids:Array(String)}
             GROUP BY request",
            vec![
                // A line is written before its request ends, up to maxDuration (300 s) before.
                ("lo", one(first - 310_000_000)),
                ("hi", one(last + 5_000_000)),
                ("ids", Param::Many(ids)),
            ],
        )
        .await?;
    Ok(counts.into_iter().map(|c| (c.request, (c.lines, c.errors))).collect())
}

fn print_request(row: &RequestRow, lines: Option<&(u64, u64)>) {
    let target = if row.query.is_empty() { row.path.clone() } else { format!("{}?{}", row.path, row.query) };
    let mut note = String::new();
    if row.attempts > 1 {
        note.push_str(&format!(" ({} attempts)", row.attempts));
    }
    if row.outcome != "ok" && !row.outcome.is_empty() {
        note.push_str(&format!(" [{}]", row.outcome));
    }
    if row.queue_ms >= 300 {
        note.push_str(&format!(" (waited {} ms for a worker)", row.queue_ms));
    }
    let lines = match lines {
        Some((n, 0)) => n.to_string(),
        Some((n, e)) => format!("{n} ({e} err)"),
        None => "-".to_owned(),
    };
    println!(
        "{:<23}  {:>6}  {:>7}  {:<9}  {:<6} {:<58} {:>5}{note}  {}",
        format_us(row.us),
        row.status,
        row.total_ms,
        row.router_region,
        row.method,
        clip(&target, 58),
        lines,
        row.request
    );
}

async fn app_lines(db: &Source<'_>, args: &LogsArgs, since: i64, until: i64) -> Result<()> {
    const LINES: &str = "SELECT toUnixTimestamp64Micro(time) AS us, stream, level, request, worker, line
        FROM forkrail.app_logs
        WHERE project = {project:String}
          AND time > fromUnixTimestamp64Micro({since:Int64}, 'UTC') AND time < fromUnixTimestamp64Micro({until:Int64}, 'UTC')
          AND ({search:String} = '' OR positionCaseInsensitive(line, {search:String}) > 0)
          AND ({level:String} = '' OR level = {level:String})";
    let params = |since: i64, limit: u32| {
        vec![
            ("since", one(since)),
            ("until", one(until)),
            ("search", one(args.search.clone().unwrap_or_default())),
            ("level", one(args.level.clone().unwrap_or_default())),
            ("limit", one(limit)),
        ]
    };
    let mut rows: Vec<AppLine> = db.rows(&args.project, "lines", &format!("{LINES} ORDER BY time DESC LIMIT {{limit:UInt32}}"), params(since, args.limit)).await?;
    rows.reverse();
    for row in &rows {
        print_line(row);
    }
    if rows.is_empty() && !args.follow {
        println!("no app output for {} since {} UTC", args.project, format_us(since));
    }
    if !args.follow {
        return Ok(());
    }
    let key = |r: &AppLine| format!("{}{}{}", r.worker, r.us, r.line.len());
    let mut seen = Seen::default();
    for row in &rows {
        seen.insert(row.us, key(row));
    }
    let mut last = rows.last().map_or(now_us() - FOLLOW_OVERLAP_US, |r| r.us);
    loop {
        tokio::time::sleep(FOLLOW_EVERY).await;
        let rows: Vec<AppLine> = db.rows(&args.project, "lines_after", &format!("{LINES} ORDER BY time ASC LIMIT 5000"), params(last - FOLLOW_OVERLAP_US, 5000)).await?;
        for row in rows {
            if seen.insert(row.us, key(&row)) {
                print_line(&row);
                last = last.max(row.us);
            }
        }
    }
}

fn print_line(row: &AppLine) {
    let level = if row.level == "info" { "" } else { &row.level };
    println!("{}  {:<6} {:<7} {}", format_us(row.us), row.stream, level, row.line);
}

async fn request_detail(db: &Source<'_>, project: &str, id: &str, since: i64) -> Result<()> {
    let rows: Vec<RequestRow> = db
        .rows(
            project,
            "request",
            "SELECT toUnixTimestamp64Micro(time) AS us, * EXCEPT (time) FROM forkrail.requests
             WHERE project = {project:String} AND time > fromUnixTimestamp64Micro({since:Int64}, 'UTC') AND request = {id:String}
             LIMIT 1",
            vec![("since", one(since)), ("id", one(id))],
        )
        .await?;
    let Some(r) = rows.into_iter().next() else {
        bail!("no request {id} in {project} since {} UTC", format_us(since));
    };
    let start = r.us - i64::from(r.total_ms) * 1000;
    let field = |name: &str, value: String| {
        if !value.is_empty() {
            println!("  {name:<14} {value}");
        }
    };
    println!("Request");
    field("Request ID", r.request.clone());
    field("Time", format!("{} UTC", format_us(start)));
    field("Status", format!("{} ({})", r.status, r.outcome));
    field("Request", format!("{} {}", r.method, r.path));
    field("Search params", r.query.clone());
    field("Host", r.host.clone());
    field("Referer", r.referer.clone());
    field("User agent", r.agent.clone());
    field("Client", r.client.clone());
    let place: Vec<&str> = [r.city.as_str(), r.subdivision.as_str(), r.country.as_str()].into_iter().filter(|p| !p.is_empty()).collect();
    field("Location", place.join(", "));
    if r.asn > 0 {
        field("Network", format!("AS{} {}", r.asn, r.as_org));
    }
    field("Region", format!("{} (router)", dash(&r.router_region)));
    field("Protocol", format!("{} ({})", r.proto, r.entry));
    field("Encoding", format!("accepts {}, sent {}", dash(&r.accept_encoding), dash(&r.encoding)));
    field("Bytes", format!("{} in, {} out", r.bytes_in.map_or("-".to_owned(), |b| b.to_string()), r.bytes_out));
    if r.attempts > 1 {
        field("Attempts", r.attempts.to_string());
    }
    field("Cache", r.cache.clone());
    if !r.middleware.is_empty() {
        field("Middleware", format!("{}{}", r.middleware, r.middleware_ms.map_or(String::new(), |ms| format!(" ({ms} ms)"))));
    }
    field("Skew", r.skew.clone());

    let inv: Vec<Invocation> = if r.worker.is_empty() {
        Vec::new()
    } else {
        db.rows(
            project,
            "invocation",
            "SELECT toUnixTimestamp64Micro(time) AS us, memory_mb, duration_ms, result, hedge, connect_ms, env,
                    env_invocations, requests, app_start_ms, rss_peak_kb, retired
             FROM forkrail.invocations
             WHERE project = {project:String} AND time >= fromUnixTimestamp64Micro({at:Int64}, 'UTC')
               AND time < fromUnixTimestamp64Micro({until:Int64}, 'UTC') AND worker = {worker:String}
             LIMIT 1",
            vec![("at", one(start)), ("until", one(r.us + 1_000_000_000)), ("worker", one(&r.worker))],
        )
        .await?
    };
    println!();
    println!("Function");
    let environment = if r.environment.is_empty() { String::new() } else { format!(" ({})", r.environment) };
    field("Deployment", format!("{}{environment}", dash(&r.deployment)));
    field("Entrypoint", r.entrypoint.clone());
    field("Route", r.route.clone());
    field("Runtime", r.runtime.clone());
    field("Region", r.region.clone());
    let max = if r.max_duration_s > 0 { format!(" of {} s", r.max_duration_s) } else { String::new() };
    field("Duration", r.worker_ms.map_or(String::new(), |v| format!("{v} ms{max}")));
    let memory_max = match (r.memory_mb, inv.first()) {
        (m, _) if m > 0 => format!(" of {m} MB"),
        (_, Some(i)) => format!(" of {} MB", i.memory_mb),
        _ => String::new(),
    };
    let peak = inv.first().filter(|i| i.rss_peak_kb > 0).map_or(String::new(), |i| format!(", invocation peak {} MB", i.rss_peak_kb / 1024));
    field("Memory used", r.rss_kb.map_or(String::new(), |kb| format!("{} MB{memory_max}{peak}", kb / 1024)));
    let start_type = match r.start.as_str() {
        "cold" => format!("cold: waited {} ms for a new environment", r.queue_ms),
        "thaw" => format!("quick start: waited {} ms for a new worker in a ready environment", r.queue_ms),
        "warm" => "warm: an open worker".to_owned(),
        _ => String::new(),
    };
    field("Start", start_type);
    if let (Some(cpu), Some(mem)) = (r.cpu_us, r.mem_kb) {
        field("Ran alone", format!("{:.1} ms of CPU, {} MB of memory", f64::from(cpu) / 1000.0, mem / 1024));
    }
    field("Invocation", r.worker.clone());
    if let Some(i) = inv.first() {
        field(
            "",
            format!(
                "{} environment ({} invocations), open {} ms, served {} requests{}{}",
                if i.env_invocations == 1 { "new" } else { "reused" },
                i.env_invocations,
                i.duration_ms,
                i.requests,
                i.connect_ms.map_or(String::new(), |c| format!(", connected after {c} ms")),
                if i.app_start_ms > 0 { format!(", app started in {} ms", i.app_start_ms) } else { String::new() }
            ),
        );
    }

    println!();
    println!("Timeline");
    let mut events = vec![(0, "received".to_owned())];
    if r.queue_ms > 0 || !r.start.is_empty() {
        events.push((i64::from(r.queue_ms), format!("worker ({})", dash(&r.start))));
    }
    if let Some(t) = r.ttfb_ms {
        events.push((i64::from(t), "response started".to_owned()));
    }
    events.push((i64::from(r.total_ms), "response sent".to_owned()));
    for (at, what) in events {
        println!("  {:>8}  {what}", format!("+{at} ms"));
    }

    let calls: Vec<Outgoing> = db
        .rows(
            project,
            "outgoing",
            "SELECT toUnixTimestamp64Micro(time) AS us, method, host, path, query, status, duration_ms, bytes, error
             FROM forkrail.outgoing
             WHERE project = {project:String}
               AND time >= fromUnixTimestamp64Micro({lo:Int64}, 'UTC') AND time <= fromUnixTimestamp64Micro({hi:Int64}, 'UTC')
               AND request = {id:String}
             ORDER BY time LIMIT 200",
            vec![("lo", one(start - 1_000_000)), ("hi", one(r.us + 1_000_000)), ("id", one(id))],
        )
        .await?;
    println!();
    if calls.is_empty() {
        println!("Outgoing requests: none recorded");
    } else {
        println!("Outgoing requests ({})", calls.len());
        for c in &calls {
            let url = if c.query.is_empty() { format!("{}{}", c.host, c.path) } else { format!("{}{}?{}", c.host, c.path, c.query) };
            let answer = if c.status > 0 { c.status.to_string() } else { "ERR".to_owned() };
            println!(
                "  {:>8}  {:<6} {:>4}  {:>9}  {:>9}  {}{}",
                format!("+{} ms", (c.us - start) / 1000),
                c.method,
                answer,
                ms(f64::from(c.duration_ms)),
                c.bytes.map_or("-".to_owned(), bytes),
                clip(&url, 110),
                if c.error.is_empty() { String::new() } else { format!("  ({})", c.error) }
            );
        }
    }

    // Lines tagged with the request; for runtimes that don't tag them, the worker's lines while it
    // ran (the router's and Lambda's clocks agree to within a few milliseconds).
    let lines: Vec<AppLine> = db
        .rows(
            project,
            "request_lines",
            "SELECT toUnixTimestamp64Micro(time) AS us, stream, level, request, worker, line FROM forkrail.app_logs
             WHERE project = {project:String}
               AND time >= fromUnixTimestamp64Micro({lo:Int64}, 'UTC') AND time <= fromUnixTimestamp64Micro({hi:Int64}, 'UTC')
               AND (request = {id:String}
                    OR (request = '' AND worker = {worker:String}
                        AND time BETWEEN fromUnixTimestamp64Micro({start:Int64}, 'UTC') AND fromUnixTimestamp64Micro({end:Int64}, 'UTC')))
             ORDER BY time LIMIT 500",
            vec![
                ("lo", one(start - 1_000_000)),
                // Lines written after the response may come only when Lambda next thaws the
                // environment, minutes later; the request index finds them cheaply.
                ("hi", one(r.us + 900_000_000)),
                ("start", one(start - 50_000)),
                ("end", one(r.us + 200_000)),
                ("id", one(id)),
                ("worker", one(&r.worker)),
            ],
        )
        .await?;
    // A runtime that tags lines tags all of a request's lines; the untagged ones near it are the
    // app's own (its startup, say).
    let tagged = lines.iter().any(|l| l.request == id);
    let lines: Vec<&AppLine> = lines.iter().filter(|l| !tagged || l.request == id).collect();
    println!();
    if lines.is_empty() {
        println!("Log messages: none");
    } else {
        println!(
            "Log messages ({}){}",
            lines.len(),
            if tagged { "" } else { ", by worker and time: this deployment's runtime doesn't tag lines with requests" }
        );
        for line in lines {
            print_line(line);
        }
    }
    Ok(())
}

pub async fn stats(ctx: &Ctx, project: &str, since: &str) -> Result<()> {
    let db = Analytics::connect(ctx).await?;
    let now = now_us();
    let since_us = parse_time(since, now)?;
    let params = || vec![("project", project.to_owned()), ("since", since_us.to_string())];

    #[derive(Deserialize)]
    struct Totals {
        requests: u64,
        s2: u64,
        s3: u64,
        s4: u64,
        s5: u64,
        bytes_out: u64,
        bytes_in: u64,
        retried: u64,
        waited_cold: u64,
        client_gone: u64,
        q: Vec<f64>,
        first: i64,
    }
    let totals: Vec<Totals> = db
        .query(
            // Columns are qualified (r.) wherever an alias reuses a column's name.
            "SELECT sum(r.requests) AS requests,
                    sumIf(r.requests, r.status < 300) AS s2, sumIf(r.requests, r.status BETWEEN 300 AND 399) AS s3,
                    sumIf(r.requests, r.status BETWEEN 400 AND 499) AS s4, sumIf(r.requests, r.status >= 500) AS s5,
                    sum(r.bytes_out) AS bytes_out, sum(r.bytes_in) AS bytes_in, sum(r.retried) AS retried,
                    sum(r.waited_cold) AS waited_cold, sum(r.client_gone) AS client_gone,
                    arrayMap(x -> if(isNaN(x), 0, x), quantilesTDigestMerge(0.5, 0.75, 0.95, 0.99)(r.duration)) AS q,
                    toUnixTimestamp(min(r.minute)) * 1000000 AS first
             FROM forkrail.requests_1m AS r
             WHERE r.project = {project:String} AND r.minute >= toStartOfMinute(fromUnixTimestamp64Micro({since:Int64}, 'UTC'))
               AND r.entry = 'https'",
            &params(),
        )
        .await?;
    let t = totals.into_iter().next().context("no answer")?;
    println!("{project}, since {} UTC (public HTTPS requests)", format_us(since_us));
    if t.requests == 0 {
        println!("no requests");
        return Ok(());
    }
    let span_s = ((now - since_us.max(t.first)) as f64 / 1e6).max(1.0);
    let pct = |n: u64| 100.0 * n as f64 / t.requests as f64;
    println!(
        "Requests     {} ({:.2} a second): 2xx {:.2}%, 3xx {:.2}%, 4xx {:.2}%, 5xx {:.3}% ({})",
        count(t.requests),
        t.requests as f64 / span_s,
        pct(t.s2),
        pct(t.s3),
        pct(t.s4),
        pct(t.s5),
        count(t.s5)
    );
    if let [p50, p75, p95, p99] = t.q[..] {
        println!("Latency      p50 {}, p75 {}, p95 {}, p99 {} (at the router)", ms(p50), ms(p75), ms(p95), ms(p99));
    }
    println!(
        "             {} retried, {} waited 300 ms or more for a worker, {} clients left early",
        count(t.retried),
        count(t.waited_cold),
        count(t.client_gone)
    );
    println!("Transfer     {} out, {} in", bytes(t.bytes_out), bytes(t.bytes_in));

    #[derive(Deserialize)]
    struct Workers {
        invocations: u64,
        cold: u64,
        hedges: u64,
        served_nothing: u64,
        failed: u64,
        throttled: u64,
        gb_s: f64,
        open_s: f64,
        requests: u64,
        connect: Vec<f64>,
    }
    let workers: Vec<Workers> = db
        .query(
            "SELECT sum(i.invocations) AS invocations, sum(i.cold) AS cold, sum(i.hedges) AS hedges,
                    sum(i.served_nothing) AS served_nothing, sum(i.failed) AS failed, sum(i.throttled) AS throttled,
                    sum(i.duration_ms * i.memory_mb) / 1024 / 1000 AS gb_s, sum(i.duration_ms) / 1000 AS open_s,
                    sum(i.requests) AS requests, arrayMap(x -> if(isNaN(x), 0, x), quantilesTDigestMerge(0.5, 0.95)(i.connect_ms)) AS connect
             FROM forkrail.invocations_1h AS i
             WHERE i.project = {project:String} AND i.hour >= toStartOfHour(fromUnixTimestamp64Micro({since:Int64}, 'UTC'))",
            &params(),
        )
        .await?;
    if let Some(w) = workers.first().filter(|w| w.invocations > 0) {
        println!(
            "Workers      {} invocations ({:.1} requests each), {} new environments, {} second invokes, {} served nothing{}",
            count(w.invocations),
            w.requests as f64 / w.invocations as f64,
            count(w.cold),
            count(w.hedges),
            count(w.served_nothing),
            if w.failed + w.throttled > 0 { format!(", {} failed, {} throttled", w.failed, w.throttled) } else { String::new() }
        );
        if let [c50, c95] = w.connect[..] {
            println!("             connected after p50 {}, p95 {}", ms(c50), ms(c95));
        }
        println!(
            "Billed       {:.0} GB-seconds, {} of workers open: ${:.4} of Lambda (whole hours from {} UTC)",
            w.gb_s,
            secs(w.open_s),
            w.gb_s * LAMBDA_GB_SECOND + w.invocations as f64 * LAMBDA_REQUEST,
            format_us(since_us - since_us.rem_euclid(3_600_000_000))
        );
    }

    #[derive(Deserialize)]
    struct Region {
        router_region: String,
        requests: u64,
        q: Vec<f64>,
    }
    let regions: Vec<Region> = db
        .query(
            "SELECT r.router_region AS router_region, sum(r.requests) AS requests,
                    arrayMap(x -> if(isNaN(x), 0, x), quantilesTDigestMerge(0.5, 0.95)(r.duration)) AS q
             FROM forkrail.requests_1m AS r
             WHERE r.project = {project:String} AND r.minute >= toStartOfMinute(fromUnixTimestamp64Micro({since:Int64}, 'UTC'))
               AND r.entry = 'https'
             GROUP BY router_region ORDER BY requests DESC",
            &params(),
        )
        .await?;
    let by_region: Vec<String> = regions
        .iter()
        .map(|r| {
            format!("{} {:.1}% (p50 {}, p95 {})", dash(&r.router_region), pct(r.requests), ms(r.q[0]), ms(r.q[1]))
        })
        .collect();
    println!("Regions      {}", by_region.join(", "));

    // Over two days by hour, else by day.
    let bucket = if now - since_us <= 2 * 86_400_000_000 { "toStartOfHour(r.minute)" } else { "toStartOfDay(r.minute)" };
    #[derive(Deserialize)]
    struct Bucket {
        at: i64,
        requests: u64,
        errors: u64,
        bytes_out: u64,
        q: Vec<f64>,
    }
    let buckets: Vec<Bucket> = db
        .query(
            &format!(
                "SELECT toUnixTimestamp({bucket}) * 1000000 AS at, sum(r.requests) AS requests,
                        sumIf(r.requests, r.status >= 500) AS errors, sum(r.bytes_out) AS bytes_out,
                        arrayMap(x -> if(isNaN(x), 0, x), quantilesTDigestMerge(0.5, 0.95)(r.duration)) AS q
                 FROM forkrail.requests_1m AS r
                 WHERE r.project = {{project:String}} AND r.minute >= toStartOfMinute(fromUnixTimestamp64Micro({{since:Int64}}, 'UTC'))
                   AND r.entry = 'https'
                 GROUP BY at ORDER BY at"
            ),
            &params(),
        )
        .await?;
    println!();
    println!("{:<19}  {:>10}  {:>6}  {:>8}  {:>8}  {:>10}", "UTC", "REQUESTS", "5XX", "P50", "P95", "OUT");
    for b in &buckets {
        println!(
            "{:<19}  {:>10}  {:>6}  {:>8}  {:>8}  {:>10}",
            &format_us(b.at)[..16],
            count(b.requests),
            count(b.errors),
            ms(b.q[0]),
            ms(b.q[1]),
            bytes(b.bytes_out)
        );
    }

    #[derive(Deserialize)]
    struct PathRow {
        path: String,
        requests: u64,
        errors: u64,
        q: Vec<f64>,
        cpu_ms: f64,
    }
    let paths: Vec<PathRow> = db
        .query(
            "SELECT p.path AS path, sum(p.requests) AS requests, sumIf(p.requests, p.status >= 500) AS errors,
                    arrayMap(x -> if(isNaN(x), 0, x), quantilesTDigestMerge(0.5, 0.95)(p.duration)) AS q,
                    sum(p.cpu_us) / greatest(sum(p.cpu_measured), 1) / 1000 AS cpu_ms
             FROM forkrail.requests_paths_1h AS p
             WHERE p.project = {project:String} AND p.hour >= toStartOfHour(fromUnixTimestamp64Micro({since:Int64}, 'UTC'))
             GROUP BY path ORDER BY requests DESC LIMIT 12",
            &params(),
        )
        .await?;
    println!();
    println!("{:<44}  {:>10}  {:>6}  {:>8}  {:>8}  {:>8}", "BUSIEST PATHS", "REQUESTS", "5XX", "P50", "P95", "CPU");
    for p in &paths {
        println!(
            "{:<44}  {:>10}  {:>6}  {:>8}  {:>8}  {:>8}",
            clip(&p.path, 44),
            count(p.requests),
            count(p.errors),
            ms(p.q[0]),
            ms(p.q[1]),
            if p.cpu_ms > 0.0 { format!("{:.0} ms", p.cpu_ms) } else { "-".to_owned() }
        );
    }

    #[derive(Deserialize)]
    struct GeoRow {
        key: String,
        requests: u64,
        share: f64,
        errors: u64,
        q: Vec<f64>,
    }
    let geo = |by: &'static str| {
        format!(
            "SELECT {by} AS key, sum(g.requests) AS requests, sum(g.errors) AS errors,
                    sum(g.requests) * 100 / sum(sum(g.requests)) OVER () AS share,
                    arrayMap(x -> if(isNaN(x), 0, x), quantilesTDigestMerge(0.5, 0.95)(g.duration)) AS q
             FROM forkrail.requests_geo_1h AS g
             WHERE g.project = {{project:String}} AND g.hour >= toStartOfHour(fromUnixTimestamp64Micro({{since:Int64}}, 'UTC'))
             GROUP BY key ORDER BY requests DESC LIMIT 10"
        )
    };
    let countries: Vec<GeoRow> = db.query(&geo("if(g.country = '', '-', g.country)"), &params()).await?;
    let networks: Vec<GeoRow> =
        db.query(&geo("if(g.asn = 0, '-', concat('AS', toString(g.asn), ' ', g.as_org))"), &params()).await?;
    for (title, rows) in [("COUNTRIES", &countries), ("NETWORKS", &networks)] {
        if rows.is_empty() {
            continue;
        }
        println!();
        println!("{title:<44}  {:>10}  {:>6}  {:>6}  {:>8}  {:>8}", "REQUESTS", "SHARE", "5XX", "P50", "P95");
        for r in rows.iter() {
            println!(
                "{:<44}  {:>10}  {:>5.1}%  {:>6}  {:>8}  {:>8}",
                clip(&r.key, 44),
                count(r.requests),
                r.share,
                count(r.errors),
                ms(r.q[0]),
                ms(r.q[1])
            );
        }
    }
    if !countries.is_empty() {
        println!("(locations: IP Geolocation by DB-IP, https://db-ip.com)");
    }

    #[derive(Deserialize)]
    struct HostRow {
        host: String,
        calls: u64,
        failed: u64,
        http_errors: u64,
        q: Vec<f64>,
    }
    let hosts: Vec<HostRow> = db
        .query(
            "SELECT o.host AS host, sum(o.calls) AS calls, sum(o.failed) AS failed,
                    sumIf(o.calls, o.status >= 400) AS http_errors,
                    arrayMap(x -> if(isNaN(x), 0, x), quantilesTDigestMerge(0.5, 0.95)(o.duration)) AS q
             FROM forkrail.outgoing_1h AS o
             WHERE o.project = {project:String} AND o.hour >= toStartOfHour(fromUnixTimestamp64Micro({since:Int64}, 'UTC'))
             GROUP BY host ORDER BY calls DESC LIMIT 10",
            &params(),
        )
        .await?;
    if !hosts.is_empty() {
        println!();
        println!("{:<44}  {:>10}  {:>6}  {:>6}  {:>8}  {:>8}", "EXTERNAL APIS", "CALLS", "4XX+", "FAILED", "P50", "P95");
        for h in &hosts {
            println!(
                "{:<44}  {:>10}  {:>6}  {:>6}  {:>8}  {:>8}",
                clip(&h.host, 44),
                count(h.calls),
                count(h.http_errors),
                count(h.failed),
                ms(h.q[0]),
                ms(h.q[1])
            );
        }
    }
    Ok(())
}

/// Each router instance over a window: CPU, memory, disk, network, connections and requests, and
/// what the router and Vector themselves use, from `host_metrics` and `requests_1m`.
pub async fn routers(ctx: &Ctx, since: &str) -> Result<()> {
    let db = Analytics::connect(ctx).await?;
    let since_us = parse_time(since, now_us())?;
    let params = || vec![("since", since_us.to_string())];
    let window = "h.time >= fromUnixTimestamp64Micro({since:Int64}, 'UTC')";

    #[derive(Deserialize)]
    struct Host {
        router_region: String,
        router: String,
        seconds: f64,
        cpu_busy: f64,
        cpus: f64,
        load1: f64,
        mem_total: f64,
        mem_available: f64,
        disk_used: f64,
        disk_total: f64,
        rx: f64,
        tx: f64,
        tcp_established: f64,
        router_mem: f64,
        router_cpu: f64,
        vector_mem: f64,
        vector_cpu: f64,
        latest: i64,
    }
    // Counters are cumulative: a window's use is the last sample less the first.
    let hosts: Vec<Host> = db
        .query(
            &format!(
                "SELECT c.router_region AS router_region, c.router AS router,
                        max(c.seconds) AS seconds,
                        1 - sumIf(c.used, c.name = 'cpu_seconds_total' AND c.mode IN ('idle', 'iowait'))
                            / greatest(sumIf(c.used, c.name = 'cpu_seconds_total'), 1e-9) AS cpu_busy,
                        sumIf(c.used, c.name = 'cpu_seconds_total') / greatest(max(c.seconds), 1) AS cpus,
                        maxIf(c.last, c.name = 'load1') AS load1,
                        maxIf(c.last, c.name = 'memory_total_bytes') AS mem_total,
                        maxIf(c.last, c.name = 'memory_available_bytes') AS mem_available,
                        maxIf(c.last, c.name = 'filesystem_used_bytes') AS disk_used,
                        maxIf(c.last, c.name = 'filesystem_total_bytes') AS disk_total,
                        sumIf(c.used, c.name = 'network_receive_bytes_total') / greatest(max(c.seconds), 1) AS rx,
                        sumIf(c.used, c.name = 'network_transmit_bytes_total') / greatest(max(c.seconds), 1) AS tx,
                        maxIf(c.last, c.name = 'tcp_connections_total' AND c.state = 'established') AS tcp_established,
                        sumIf(c.last, c.name = 'cgroup_memory_current_bytes' AND c.cgroup LIKE '%forkrail-router@%') AS router_mem,
                        sumIf(c.used, c.name = 'cgroup_cpu_usage_seconds_total' AND c.cgroup LIKE '%forkrail-router@%') / greatest(max(c.seconds), 1) AS router_cpu,
                        sumIf(c.last, c.name = 'cgroup_memory_current_bytes' AND c.cgroup LIKE '%forkrail-vector%') AS vector_mem,
                        sumIf(c.used, c.name = 'cgroup_cpu_usage_seconds_total' AND c.cgroup LIKE '%forkrail-vector%') / greatest(max(c.seconds), 1) AS vector_cpu,
                        max(c.latest) AS latest
                 FROM (
                     SELECT h.router_region AS router_region, h.router AS router, h.name AS name,
                            h.tags['cpu'] AS cpu, h.tags['mode'] AS mode, h.tags['state'] AS state,
                            h.tags['cgroup'] AS cgroup, h.tags['device'] AS device,
                            greatest(argMax(h.value, h.time) - argMin(h.value, h.time), 0) AS used,
                            argMax(h.value, h.time) AS last,
                            dateDiff('millisecond', min(h.time), max(h.time)) / 1000 AS seconds,
                            toUnixTimestamp64Micro(max(h.time)) AS latest
                     FROM forkrail.host_metrics AS h
                     WHERE {window} AND (h.tags['mountpoint'] = '/' OR h.name NOT LIKE 'filesystem%')
                     GROUP BY router_region, router, name, cpu, mode, state, cgroup, device
                 ) AS c
                 GROUP BY router_region, router ORDER BY router_region"
            ),
            &params(),
        )
        .await?;
    #[derive(Deserialize)]
    struct Traffic {
        router_region: String,
        requests: u64,
        peak_minute: u64,
    }
    let traffic: Vec<Traffic> = db
        .query(
            "SELECT t.router_region AS router_region, sum(t.n) AS requests, max(t.n) AS peak_minute
             FROM (SELECT r.router_region AS router_region, r.minute AS minute, sum(r.requests) AS n
                   FROM forkrail.requests_1m AS r
                   WHERE r.minute >= toStartOfMinute(fromUnixTimestamp64Micro({since:Int64}, 'UTC'))
                   GROUP BY router_region, minute) AS t
             GROUP BY router_region",
            &params(),
        )
        .await?;
    if hosts.is_empty() {
        println!("No host metrics since {} UTC.", format_us(since_us));
        return Ok(());
    }
    for h in &hosts {
        let t = traffic.iter().find(|t| t.router_region == h.router_region);
        let per_sec = t.map_or(0.0, |t| t.requests as f64 / h.seconds.max(1.0));
        println!("{} {} (last sample {} UTC, over {})", h.router_region, h.router, format_us(h.latest), secs(h.seconds));
        println!("  CPU        {:.1}% of {:.0} vCPUs busy, load {:.2}", h.cpu_busy * 100.0, h.cpus, h.load1);
        println!(
            "  Memory     {} available of {} ({:.0}% used)",
            bytes(h.mem_available as u64),
            bytes(h.mem_total as u64),
            (1.0 - h.mem_available / h.mem_total.max(1.0)) * 100.0
        );
        println!("  Disk       {} used of {} ({:.0}%)", bytes(h.disk_used as u64), bytes(h.disk_total as u64), h.disk_used * 100.0 / h.disk_total.max(1.0));
        println!("  Network    {}/s in, {}/s out, {:.0} TCP connections open", bytes(h.rx as u64), bytes(h.tx as u64), h.tcp_established);
        println!(
            "  Requests   {:.1} a second, busiest minute {}",
            per_sec,
            t.map_or("-".to_owned(), |t| count(t.peak_minute))
        );
        println!(
            "  Router     {:.1}% of a vCPU, {} memory; Vector {:.1}%, {}",
            h.router_cpu * 100.0,
            bytes(h.router_mem as u64),
            h.vector_cpu * 100.0,
            bytes(h.vector_mem as u64)
        );
        let mut hints = Vec::new();
        // A t4g's baseline is 20% of each vCPU (t4g.small, medium) or 30-40% (large and up); past
        // it, surplus credits cost $0.04 a vCPU-hour.
        if h.cpu_busy > 0.2 {
            hints.push("CPU is above a t4g.small's 20% baseline: check CPUCreditBalance, or move to a larger or c7g instance");
        }
        if h.mem_available / h.mem_total.max(1.0) < 0.25 {
            hints.push("less than a quarter of memory is available");
        }
        if h.disk_used / h.disk_total.max(1.0) > 0.7 {
            hints.push("the disk is over 70% full");
        }
        for hint in hints {
            println!("  Note       {hint}");
        }
        println!();
    }
    Ok(())
}

pub async fn usage(ctx: &Ctx, project: Option<&str>, since: &str, daily: bool) -> Result<()> {
    let db = Analytics::connect(ctx).await?;
    let now = now_us();
    let since_us = parse_time(since, now)?;
    // By day, the key is the day; otherwise the project.
    let key = if daily { "toString(toDate(t))" } else { "project" };
    let sql = format!(
        "WITH
           req AS (SELECT {key} AS k, sum(requests) AS requests, sum(bytes_out) AS bytes_out, sum(bytes_in) AS bytes_in
                   FROM (SELECT minute AS t, * FROM forkrail.requests_1m)
                   WHERE minute >= toStartOfMinute(fromUnixTimestamp64Micro({{since:Int64}}, 'UTC')) AND entry = 'https'
                     AND ({{project:String}} = '' OR project = {{project:String}})
                   GROUP BY k),
           inv AS (SELECT {key} AS k, sum(invocations) AS invocations, sum(cold) AS cold,
                          sum(duration_ms * memory_mb) / 1024 / 1000 AS gb_s, sum(duration_ms) / 1000 AS open_s
                   FROM (SELECT hour AS t, * FROM forkrail.invocations_1h)
                   WHERE hour >= toStartOfHour(fromUnixTimestamp64Micro({{since:Int64}}, 'UTC'))
                     AND ({{project:String}} = '' OR project = {{project:String}})
                   GROUP BY k),
           cpu AS (SELECT k, sum(requests * cpu_ms) / 1000 AS cpu_s FROM (
                     SELECT {key} AS k, path, sum(requests) AS requests,
                            sum(cpu_us) / greatest(sum(cpu_measured), 1) / 1000 AS cpu_ms
                     FROM (SELECT hour AS t, * FROM forkrail.requests_paths_1h)
                     WHERE hour >= toStartOfHour(fromUnixTimestamp64Micro({{since:Int64}}, 'UTC'))
                       AND ({{project:String}} = '' OR project = {{project:String}})
                     GROUP BY k, path)
                   GROUP BY k)
         SELECT if(req.k != '', req.k, inv.k) AS key, req.requests AS requests, req.bytes_out AS bytes_out,
                req.bytes_in AS bytes_in, inv.invocations AS invocations, inv.cold AS cold, inv.gb_s AS gb_s,
                inv.open_s AS open_s, cpu.cpu_s AS cpu_s
         FROM req FULL OUTER JOIN inv ON req.k = inv.k LEFT JOIN cpu ON cpu.k = if(req.k != '', req.k, inv.k)
         WHERE key != ''
         ORDER BY {}",
        if daily { "key" } else { "bytes_out + requests * 1000 DESC" }
    );
    let rows: Vec<UsageRow> = db
        .query(&sql, &[("since", since_us.to_string()), ("project", project.unwrap_or_default().to_owned())])
        .await?;
    println!(
        "Usage since {} UTC{}: public requests, estimated at us-east-1 prices",
        format_us(since_us),
        project.map(|p| format!(", {p}")).unwrap_or_default()
    );
    println!(
        "{:<16}  {:>9}  {:>9}  {:>9}  {:>9}  {:>8}  {:>8}  {:>8}  {:>9}  {:>9}",
        if daily { "DAY" } else { "PROJECT" },
        "REQUESTS",
        "INVOKES",
        "GB-S",
        "OUT",
        "LAMBDA",
        "EGRESS",
        "HOP",
        "FORKRAIL",
        "VERCEL"
    );
    let mut sum = UsageRow::default();
    for row in &rows {
        print_usage(&row.key, row);
        sum.add(row);
    }
    if rows.len() > 1 {
        print_usage("total", &sum);
    }
    println!();
    println!("LAMBDA: GB-seconds at ${LAMBDA_GB_SECOND}/GB-s (arm64) and invocations at $0.20/M. EGRESS: ${EGRESS_GB}/GB plus");
    println!("Global Accelerator's ${GA_PREMIUM_GB}/GB. HOP: Lambda to router, ${LAMBDA_ROUTER_GB}/GB each way. Not included: the");
    println!("routers, Global Accelerator's hourly fee and the analytics box, shared by every project.");
    println!("VERCEL: fluid compute (active CPU from each path's measured CPU, 2 GB while workers were open),");
    println!("$0.60/M invocations, $2/M CDN requests, $0.15/GB Fast Data Transfer (no included TB) and $0.06/GB");
    println!("Fast Origin Transfer, at iad1 prices. A rough estimate.");
    Ok(())
}

#[derive(Deserialize, Default)]
struct UsageRow {
    #[serde(default)]
    key: String,
    #[serde(default)]
    requests: u64,
    #[serde(default)]
    bytes_out: u64,
    #[serde(default)]
    bytes_in: u64,
    #[serde(default)]
    invocations: u64,
    #[serde(default)]
    cold: u64,
    #[serde(default)]
    gb_s: f64,
    #[serde(default)]
    open_s: f64,
    #[serde(default)]
    cpu_s: f64,
}

impl UsageRow {
    fn add(&mut self, other: &UsageRow) {
        self.requests += other.requests;
        self.bytes_out += other.bytes_out;
        self.bytes_in += other.bytes_in;
        self.invocations += other.invocations;
        self.cold += other.cold;
        self.gb_s += other.gb_s;
        self.open_s += other.open_s;
        self.cpu_s += other.cpu_s;
    }

    fn lambda(&self) -> f64 {
        self.gb_s * LAMBDA_GB_SECOND + self.invocations as f64 * LAMBDA_REQUEST
    }

    fn egress(&self) -> f64 {
        self.bytes_out as f64 / GB * (EGRESS_GB + GA_PREMIUM_GB)
    }

    fn hop(&self) -> f64 {
        (self.bytes_out + self.bytes_in) as f64 / GB * LAMBDA_ROUTER_GB
    }

    fn vercel(&self) -> f64 {
        let out_gb = self.bytes_out as f64 / GB;
        self.cpu_s / 3600.0 * VERCEL_ACTIVE_CPU_HOUR
            + self.open_s / 3600.0 * VERCEL_FLUID_GB * VERCEL_MEMORY_GB_HOUR
            + self.requests as f64 * (VERCEL_INVOCATION + VERCEL_CDN_REQUEST)
            + out_gb * (VERCEL_FDT_GB + VERCEL_FOT_GB)
    }
}

fn print_usage(key: &str, row: &UsageRow) {
    let forkrail = row.lambda() + row.egress() + row.hop();
    println!(
        "{:<16}  {:>9}  {:>9}  {:>9.0}  {:>9}  {:>8}  {:>8}  {:>8}  {:>9}  {:>9}",
        clip(key, 16),
        count(row.requests),
        count(row.invocations),
        row.gb_s,
        bytes(row.bytes_out),
        dollars(row.lambda()),
        dollars(row.egress()),
        dollars(row.hop()),
        dollars(forkrail),
        dollars(row.vercel())
    );
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RequestRow {
    us: i64,
    request: String,
    status: u16,
    method: String,
    path: String,
    query: String,
    host: String,
    total_ms: u32,
    queue_ms: u32,
    attempts: u8,
    outcome: String,
    router_region: String,
    region: String,
    worker: String,
    deployment: String,
    entry: String,
    client: String,
    proto: String,
    agent: String,
    accept_encoding: String,
    encoding: String,
    ttfb_ms: Option<u32>,
    worker_ms: Option<u32>,
    bytes_in: Option<u64>,
    bytes_out: u64,
    mem_kb: Option<u32>,
    cpu_us: Option<u32>,
    referer: String,
    route: String,
    start: String,
    rss_kb: Option<u32>,
    environment: String,
    runtime: String,
    entrypoint: String,
    memory_mb: u32,
    max_duration_s: u32,
    country: String,
    subdivision: String,
    city: String,
    asn: u32,
    as_org: String,
    /// C16's columns, empty on a store without them: what the response cache did (`x-vercel-cache`'s
    /// value), what Routing Middleware said and how long it took, what skew protection made of it.
    cache: String,
    middleware: String,
    middleware_ms: Option<u32>,
    skew: String,
}

#[derive(Deserialize)]
struct Outgoing {
    us: i64,
    method: String,
    host: String,
    path: String,
    query: String,
    status: u16,
    duration_ms: f32,
    bytes: Option<u64>,
    error: String,
}

#[derive(Deserialize)]
struct AppLine {
    us: i64,
    stream: String,
    level: String,
    request: String,
    worker: String,
    line: String,
}

#[derive(Deserialize)]
struct Invocation {
    #[allow(dead_code)]
    us: i64,
    memory_mb: u32,
    duration_ms: u32,
    #[allow(dead_code)]
    result: String,
    #[allow(dead_code)]
    hedge: bool,
    connect_ms: Option<u32>,
    #[allow(dead_code)]
    env: String,
    env_invocations: u32,
    requests: u32,
    app_start_ms: u32,
    rss_peak_kb: u32,
    #[allow(dead_code)]
    retired: String,
}

/// Keys shown lately, so `--follow`'s overlapping reads print each row once.
#[derive(Default)]
struct Seen {
    keys: HashSet<String>,
    order: VecDeque<(i64, String)>,
}

impl Seen {
    /// Whether `key` is new; forgets keys well behind `at`.
    fn insert(&mut self, at: i64, key: String) -> bool {
        while self.order.front().is_some_and(|(t, _)| *t < at - 4 * FOLLOW_OVERLAP_US) {
            let (_, old) = self.order.pop_front().expect("checked");
            self.keys.remove(&old);
        }
        if !self.keys.insert(key.clone()) {
            return false;
        }
        self.order.push_back((at, key));
        true
    }
}

/// `5xx`, `4xx`, `404`, or nothing (all), as an inclusive range.
fn status_range(status: Option<&str>) -> Result<(u16, u16)> {
    let Some(s) = status else { return Ok((0, 999)) };
    let s = s.to_ascii_lowercase();
    if let Some(class) = s.strip_suffix("xx")
        && let Ok(c @ 1..=5) = class.parse::<u16>()
    {
        return Ok((c * 100, c * 100 + 99));
    }
    match s.parse::<u16>() {
        Ok(code @ 100..=599) => Ok((code, code)),
        _ => bail!("--status takes a class (2xx, 4xx, 5xx) or a code (404), not {s}"),
    }
}

fn array_param(values: &[&str]) -> String {
    let quoted: Vec<String> = values.iter().map(|v| format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))).collect();
    format!("[{}]", quoted.join(","))
}

fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as i64)
}

/// A time as microseconds since the epoch: an age (`30s`, `15m`, `2h`, `7d`) back from now, or a
/// UTC date and time (`2026-11-08`, `2026-11-08 14:00`, `2026-11-08T14:00:30Z`).
fn parse_time(text: &str, now: i64) -> Result<i64> {
    let text = text.trim();
    if let Some((number, unit)) = text.char_indices().find(|(_, c)| !c.is_ascii_digit()).map(|(i, _)| text.split_at(i))
        && !number.is_empty()
        && let Ok(n) = number.parse::<i64>()
    {
        let seconds = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            "w" => 604_800,
            _ => 0,
        };
        if seconds > 0 {
            return Ok(now - n * seconds * 1_000_000);
        }
    }
    let cleaned = text.trim_end_matches('Z').replace('T', " ");
    let (date, time) = cleaned.split_once(' ').unwrap_or((&cleaned, "00:00:00"));
    let d: Vec<i64> = date.split('-').map(str::parse).collect::<Result<_, _>>().with_context(|| format!("not a time: {text}"))?;
    let t: Vec<i64> = time.split(':').map(str::parse).collect::<Result<_, _>>().with_context(|| format!("not a time: {text}"))?;
    let [y, mo, day] = d[..] else { bail!("not a date: {text}") };
    let (h, mi, s) = match t[..] {
        [h, m] => (h, m, 0),
        [h, m, s] => (h, m, s),
        _ => bail!("not a time: {text}"),
    };
    Ok(((days_from_civil(y, mo, day) * 86_400) + h * 3600 + mi * 60 + s) * 1_000_000)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `2026-11-08 20:14:50.249`, UTC.
fn format_us(us: i64) -> String {
    let secs = us.div_euclid(1_000_000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}", s / 3600, s / 60 % 60, s % 60, us.rem_euclid(1_000_000) / 1000)
}

fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn ms(value: f64) -> String {
    if value.is_nan() {
        "-".to_owned()
    } else if value >= 10_000.0 {
        format!("{:.0} s", value / 1000.0)
    } else if value >= 1000.0 {
        format!("{:.2} s", value / 1000.0)
    } else {
        format!("{value:.0} ms")
    }
}

fn secs(value: f64) -> String {
    if value >= 3600.0 { format!("{:.1} h", value / 3600.0) } else { format!("{value:.0} s") }
}

fn bytes(n: u64) -> String {
    let n = n as f64;
    if n >= GB {
        format!("{:.2} GB", n / GB)
    } else if n >= 1_048_576.0 {
        format!("{:.1} MB", n / 1_048_576.0)
    } else if n >= 1024.0 {
        format!("{:.0} KB", n / 1024.0)
    } else {
        format!("{n:.0} B")
    }
}

fn dollars(value: f64) -> String {
    if value >= 100.0 { format!("${value:.0}") } else { format!("${value:.2}") }
}

fn dash(text: &str) -> &str {
    if text.is_empty() { "-" } else { text }
}

fn clip(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        text.to_owned()
    } else {
        let mut out: String = text.chars().take(n - 1).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse() {
        let now = 1_794_168_000_000_000; // 2026-11-08 20:00:00 UTC
        assert_eq!(format_us(now), "2026-11-08 20:00:00.000");
        assert_eq!(parse_time("15m", now).unwrap(), now - 900_000_000);
        assert_eq!(parse_time("2d", now).unwrap(), now - 2 * 86_400_000_000);
        assert_eq!(parse_time("2026-11-08 20:00", now).unwrap(), now);
        assert_eq!(parse_time("2026-11-08T20:00:00Z", now).unwrap(), now);
        assert_eq!(parse_time("2026-11-08", now).unwrap(), now - 20 * 3_600_000_000);
        assert!(parse_time("soon", now).is_err());
        assert_eq!(format_us(parse_time("2024-02-29 23:59:59", now).unwrap()), "2024-02-29 23:59:59.000");
    }

    #[test]
    fn status_classes() {
        assert_eq!(status_range(Some("5xx")).unwrap(), (500, 599));
        assert_eq!(status_range(Some("404")).unwrap(), (404, 404));
        assert_eq!(status_range(None).unwrap(), (0, 999));
        assert!(status_range(Some("6xx")).is_err());
        assert!(status_range(Some("abc")).is_err());
    }

    #[test]
    fn follow_prints_each_row_once() {
        let mut seen = Seen::default();
        assert!(seen.insert(1, "a".into()));
        assert!(!seen.insert(2, "a".into()));
        assert!(seen.insert(3, "b".into()));
        // Long after, `a` is forgotten.
        assert!(seen.insert(1 + 5 * FOLLOW_OVERLAP_US, "a".into()));
    }

    #[test]
    fn numbers_read_well() {
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(count(12), "12");
        assert_eq!(ms(349.6), "350 ms");
        assert_eq!(ms(1234.0), "1.23 s");
        assert_eq!(array_param(&["a", "b'c"]), r"['a','b\'c']");
    }
}
