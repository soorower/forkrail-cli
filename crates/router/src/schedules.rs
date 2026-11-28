//! Schedules: runs of deployments' `crons` and `schedules` (see `forkrail_common::schedules`).
//!
//! EventBridge Scheduler sends a message per run to an SQS queue (`FORKRAIL_SCHEDULE_QUEUE`); every
//! router long-polls it, so whichever takes a message runs it. A run:
//!
//! 1. is taken off the queue at once (no retries, as Vercel), and skipped if this router ran the
//!    same run id in the last 10 minutes, or if it's later than its message allows (`late_s`);
//! 2. is looked up in the routing table: the project's production deployment (`<project>.<base>`)
//!    and the schedule of that key in its entry. The path and method come from there, never from
//!    the message, so a message can only start a schedule the deployment has;
//! 3. is sent as an HTTP request to this process's own loopback listener (`Entry::Schedule`), with
//!    a single-use token naming the run. It then takes a visitor's path through the proxy (routing
//!    rules, static files, the function, maxDuration, the access log), minus protection, and the
//!    worker adds `Authorization: Bearer $CRON_SECRET`. The listener is this process's alone (not
//!    shared with the other router during a blue/green restart), and the token is what makes a
//!    request a run: anything else on that port is refused.
//!
//! A skipped run gets an access line too (`schedule_expired`, `schedule_gone`), so its schedule's
//! history shows it. For tests, the tester's port takes a message from loopback at
//! `POST /_forkrail/schedule-run`, as if from the queue.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use forkrail_common::schedules::{self as sched, Message, Schedule};
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper::header::{CONTENT_LENGTH, HOST, HeaderValue, USER_AGENT};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::registry::Registry;

/// On a run's request to the loopback listener: the token naming it.
pub const RUN_HEADER: &str = "x-forkrail-run";
/// A run id seen within this long is a repeat delivery.
const SEEN_FOR: Duration = Duration::from_secs(600);
/// Beyond the deployment's maxDuration, how long a run's request may take before it's dropped
/// (the proxy answers 504 at maxDuration itself).
const RUN_GRACE: Duration = Duration::from_secs(60);
/// Long polls: SQS answers as soon as a message arrives, or after this long with none.
const WAIT_S: i32 = 20;
/// After a failed poll, before the next.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// A poll failing over and over is warned about at most this often.
const WARN_EVERY: Duration = Duration::from_secs(300);

/// What the proxy needs to know about a run when its request arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    pub key: String,
    pub run: String,
    /// From when the expression matched to this router having the message, in ms; none for a
    /// manual run.
    pub delay_ms: Option<u64>,
    /// Of that, the time in the queue: from SQS having the message to this router having it, in
    /// ms. The rest is Scheduler's. None from the tester's port.
    pub queue_ms: Option<u64>,
    pub manual: bool,
}

/// Why a run didn't start.
#[derive(Debug, PartialEq, Eq)]
pub enum Skipped {
    /// The message isn't one this router reads.
    Unreadable(String),
    /// This router ran that run id moments ago.
    Repeat,
    /// Later than its message allows.
    Expired,
    /// The project's production deployment has no schedule of that key (or no production).
    Gone,
}

pub struct Runs {
    /// This process's own loopback listener for runs.
    addr: SocketAddr,
    registry: Arc<Registry>,
    /// Token → run, from sending a run's request until the proxy takes it.
    pending: Mutex<HashMap<String, RunInfo>>,
    /// Run ids lately run, against repeat deliveries.
    seen: Mutex<HashMap<String, Instant>>,
}

impl Runs {
    pub fn new(addr: SocketAddr, registry: Arc<Registry>) -> Arc<Self> {
        Arc::new(Self { addr, registry, pending: Mutex::default(), seen: Mutex::default() })
    }

    /// The run a request to the loopback listener names, once: the proxy's half of the handover.
    pub fn take(&self, token: &str) -> Option<RunInfo> {
        self.pending.lock().unwrap().remove(token)
    }

    /// Check a message and, if it should run, start its request in the background.
    pub fn accept(self: &Arc<Self>, msg: Message, sent_ms: Option<u64>) -> Result<RunInfo, Skipped> {
        if msg.v != 1 {
            return Err(Skipped::Unreadable(format!("version {}", msg.v)));
        }
        if msg.project.is_empty() || msg.key.is_empty() || msg.run.is_empty() {
            return Err(Skipped::Unreadable("project, key and run are required".to_owned()));
        }
        let now = unix_now();
        let delay_ms = (!msg.manual).then(|| sched::parse_time(&msg.scheduled)).flatten().map(|at| ((now - at).max(0.0) * 1000.0) as u64);
        {
            let mut seen = self.seen.lock().unwrap();
            seen.retain(|_, at| at.elapsed() < SEEN_FOR);
            if seen.insert(msg.run.clone(), Instant::now()).is_some() {
                return Err(Skipped::Repeat);
            }
        }
        let queue_ms = sent_ms.map(|sent| ((now * 1000.0) as u64).saturating_sub(sent));
        let info = RunInfo { key: msg.key.clone(), run: msg.run.clone(), delay_ms, queue_ms, manual: msg.manual };
        let found = self.registry.schedule(&msg.project, &msg.key);
        if !msg.manual && msg.late_s > 0 && delay_ms.is_some_and(|d| d > msg.late_s * 1000) {
            skipped_line(&msg, &info, found.as_ref(), "schedule_expired");
            return Err(Skipped::Expired);
        }
        let Some(found) = found else {
            skipped_line(&msg, &info, None, "schedule_gone");
            return Err(Skipped::Gone);
        };
        let token = forkrail_common::random_hex(16);
        self.pending.lock().unwrap().insert(token.clone(), info.clone());
        let runs = self.clone();
        let scheduled = if msg.scheduled.is_empty() || msg.manual { rfc3339_now() } else { msg.scheduled.clone() };
        tokio::spawn(async move {
            let started = Instant::now();
            let result = tokio::time::timeout(found.max_duration + RUN_GRACE, runs.send(&token, &found, &msg.run, &scheduled)).await;
            // Gone already if the proxy took it; otherwise the request never got that far.
            if runs.take(&token).is_some() {
                warn!(project = %msg.project, key = %msg.key, run = %msg.run, "a run's request never reached the proxy");
            }
            match result {
                Ok(Ok(status)) => debug!(project = %msg.project, key = %msg.key, run = %msg.run, status, ms = started.elapsed().as_millis() as u64, "schedule run done"),
                Ok(Err(e)) => warn!(project = %msg.project, key = %msg.key, run = %msg.run, error = %e, "schedule run failed"),
                Err(_) => warn!(project = %msg.project, key = %msg.key, run = %msg.run, "schedule run timed out"),
            }
        });
        Ok(info)
    }

    /// The run's request, to the loopback listener: its status once the whole body has come.
    async fn send(&self, token: &str, found: &Found, run: &str, scheduled: &str) -> Result<u16, crate::BoxError> {
        let s = &found.schedule;
        let mut req = Request::builder().method(s.method.as_str()).uri(s.path.as_str()).header(HOST, found.host.as_str()).header(USER_AGENT, sched::USER_AGENT).header(RUN_HEADER, token);
        for name in sched::EXPRESSION_HEADERS {
            req = req.header(name, s.expression.as_str());
        }
        // A `schedules` entry's run also describes itself as a CloudEvent, as apps moved over read it.
        if let Some(name) = &s.name {
            for (header, value) in [
                ("ce-specversion", "1.0"),
                ("ce-type", "com.vercel.schedule.v1beta"),
                ("ce-source", &format!("/schedule/default/{name}")),
                ("ce-id", run),
                ("ce-vssscheduleid", &s.key),
                ("ce-vssschedulename", name),
                ("ce-vssnamespace", "default"),
                ("ce-vssscheduledat", scheduled),
                ("ce-vssschedulesource", "static"),
                ("ce-vssexpression", &s.expression),
            ] {
                if let Ok(value) = HeaderValue::from_str(value) {
                    req = req.header(header, value);
                }
            }
        }
        let req = req.header(CONTENT_LENGTH, 0).body(Empty::<Bytes>::new())?;
        let tcp = TcpStream::connect(self.addr).await?;
        tcp.set_nodelay(true)?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let response = sender.send_request(req).await?;
        let status = response.status().as_u16();
        // Read to the end, so the access line says the whole response went out.
        let mut body = response.into_body();
        while let Some(frame) = body.frame().await {
            frame?;
        }
        Ok(status)
    }

    /// Take messages off the queue until `stop`: each deleted as soon as it's received, then run.
    pub async fn poll(self: Arc<Self>, client: aws_sdk_sqs::Client, url: String, mut stop: watch::Receiver<bool>) {
        info!(queue = %url, "schedules: polling");
        let mut warned: Option<Instant> = None;
        loop {
            let received = tokio::select! {
                r = client
                    .receive_message()
                    .queue_url(&url)
                    .max_number_of_messages(10)
                    .wait_time_seconds(WAIT_S)
                    .message_system_attribute_names(aws_sdk_sqs::types::MessageSystemAttributeName::SentTimestamp)
                    .send() => r,
                _ = stop.wait_for(|s| *s) => break,
            };
            let messages = match received {
                Ok(out) => {
                    if warned.take().is_some() {
                        info!(queue = %url, "schedules: polling works again");
                    }
                    out.messages.unwrap_or_default()
                }
                Err(e) => {
                    if warned.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
                        warn!(queue = %url, error = %aws_sdk_sqs::error::DisplayErrorContext(&e), "schedules: polling the queue failed");
                        warned = Some(Instant::now());
                    }
                    tokio::time::sleep(RETRY_AFTER).await;
                    continue;
                }
            };
            if messages.is_empty() {
                continue;
            }
            let entries: Vec<_> = messages
                .iter()
                .enumerate()
                .filter_map(|(i, m)| {
                    aws_sdk_sqs::types::DeleteMessageBatchRequestEntry::builder().id(i.to_string()).set_receipt_handle(m.receipt_handle.clone()).build().ok()
                })
                .collect();
            if let Err(e) = client.delete_message_batch().queue_url(&url).set_entries(Some(entries)).send().await {
                // They come back after the visibility timeout and run again, unless their run id is
                // still remembered here.
                warn!(error = %aws_sdk_sqs::error::DisplayErrorContext(&e), "schedules: deleting messages failed");
            }
            for m in messages {
                let sent = m
                    .attributes()
                    .and_then(|a| a.get(&aws_sdk_sqs::types::MessageSystemAttributeName::SentTimestamp))
                    .and_then(|t| t.parse::<u64>().ok());
                let _ = self.take_message(m.body.as_deref().unwrap_or_default(), sent);
            }
        }
        info!("schedules: stopped polling");
    }

    /// A message's body, from the queue or the tester's port.
    pub fn take_message(self: &Arc<Self>, body: &str, sent_ms: Option<u64>) -> Result<RunInfo, Skipped> {
        let result = serde_json::from_str::<Message>(body).map_err(|e| Skipped::Unreadable(e.to_string())).and_then(|msg| self.accept(msg, sent_ms));
        match &result {
            Ok(info) => debug!(key = %info.key, run = %info.run, delay_ms = info.delay_ms, manual = info.manual, "schedule run"),
            Err(Skipped::Unreadable(why)) => warn!(error = %why, body = %truncate(body, 300), "schedules: a message that isn't a run"),
            Err(Skipped::Repeat) => debug!(body = %truncate(body, 300), "schedules: a repeat delivery"),
            Err(_) => {}
        }
        result
    }
}

/// Where a run goes.
pub struct Found {
    /// `<project>.<base>`.
    pub host: String,
    pub deployment: String,
    pub schedule: Schedule,
    pub max_duration: Duration,
}

/// The access line of a run that didn't start, so its schedule's history shows it.
fn skipped_line(msg: &Message, info: &RunInfo, found: Option<&Found>, outcome: &str) {
    let (path, query) = match found.map(|f| f.schedule.path.as_str()) {
        Some(p) => match p.split_once('?') {
            Some((path, query)) => (Some(path), Some(query).filter(|q| !q.is_empty())),
            None => (Some(p), None),
        },
        None => (None, None),
    };
    info!(
        request = %forkrail_common::quick_id(),
        entry = "schedule",
        host = found.map(|f| f.host.as_str()),
        deployment = found.map(|f| f.deployment.as_str()),
        project = %msg.project,
        method = found.map(|f| f.schedule.method.as_str()),
        path,
        query = query.map(crate::proxy::mask_query),
        agent = sched::USER_AGENT,
        status = 0u16,
        outcome,
        attempts = 0u32,
        queue_ms = 0u64,
        total_ms = 0u64,
        bytes_out = 0u64,
        schedule = %info.key,
        schedule_run = %schedule_run(info),
        schedule_delay_ms = info.delay_ms,
        schedule_queue_ms = info.queue_ms,
        "request"
    );
}

/// The run id as logged: a manual run's says so.
pub fn schedule_run(info: &RunInfo) -> String {
    if info.manual && !info.run.starts_with("manual-") { format!("manual-{}", info.run) } else { info.run.clone() }
}

fn unix_now() -> f64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or_default()
}

fn rfc3339_now() -> String {
    let secs = unix_now() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
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

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The queue's client, in the queue's own region (from its URL, `https://sqs.<region>.amazonaws.com/…`).
pub async fn sqs_client(url: &str) -> aws_sdk_sqs::Client {
    use aws_config::{BehaviorVersion, Region};
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .timeout_config(aws_config::timeout::TimeoutConfig::builder().connect_timeout(Duration::from_secs(5)).operation_timeout(Duration::from_secs(WAIT_S as u64 + 10)).build());
    if let Some(region) = url.strip_prefix("https://sqs.").and_then(|rest| rest.split('.').next()) {
        loader = loader.region(Region::new(region.to_owned()));
    }
    aws_sdk_sqs::Client::new(&loader.load().await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let now = rfc3339_now();
        let back = sched::parse_time(&now).unwrap();
        assert!((back - unix_now()).abs() < 2.0, "{now}");
        assert_eq!(truncate("héllo", 2), "hé");
    }

    #[test]
    fn run_ids() {
        let info = RunInfo { key: "k".into(), run: "abc".into(), delay_ms: None, queue_ms: None, manual: true };
        assert_eq!(schedule_run(&info), "manual-abc");
        assert_eq!(schedule_run(&RunInfo { run: "manual-x".into(), ..info.clone() }), "manual-x");
        assert_eq!(schedule_run(&RunInfo { manual: false, ..info }), "abc");
    }
}
