//! The app's output, from each worker into the router's log.
//!
//! When a worker that announced `logs` connects, the router opens one long-lived request to it,
//! `GET /_forkrail/logs`, and writes every line that comes back as an `app log` event with the
//! deployment and worker. The stream isn't a lease: it doesn't count as in flight, so packing,
//! the idle clock and retiring ignore it. It is tied to the `Worker` instead: dropping the
//! worker's last handle ends the stream (`Follow`), so a retired worker's tunnel still closes once
//! its last request finishes.
//!
//! What each request may print is what Vercel allows it: `REQUEST_LINES` lines and
//! `REQUEST_BYTES` of text in all (a line is at most 256 KiB, which the runtime sees to). Within
//! that, every line is kept, however much the deployment is writing: there is no limit per second.
//! What belongs to no request (start-up, background work), and what a request prints past its own
//! limits, shares a bucket per deployment, in bytes (`REST_BYTES_PER_SEC`, `REST_BURST_BYTES`), so
//! a loop that prints can't fill the disk. (Until 2026-11-10 every line shared one bucket of 200
//! lines a second per deployment, which dropped 18-50% of one API's at 12-29 requests a second.)
//!
//! What isn't kept is counted and reported: for the deployment once a second, and for each
//! request on its access log line (`lines_dropped`, `calls_dropped`), so its log says when it
//! isn't whole. Lines can reach the router after their request's access log line (a burst printed
//! just before the app answers does); those dropped then are reported `LATE` afterwards in a
//! `late drops` event that names the request.
//!
//! The same stream carries the app's outgoing HTTP calls, which the shims report (stream
//! `fetch`). Each becomes an `outgoing` event with the request it was made for, its URL without a
//! user or password and with secret-looking query parameters masked, its status or error and its
//! duration. A request's calls are always kept: they are how its time is accounted for. Only a
//! request past `REQUEST_CALLS` of them is taken for a loop.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::{Buf, BytesMut};
use forkrail_common::{LOGS_PATH, LOGS_TOKEN_HEADER, LogLine};
use serde::Deserialize;
use http_body_util::{BodyExt, Empty};
use hyper::client::conn::http2::SendRequest;
use hyper::{Request, Version};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::proxy::{ProxyBody, mask_url};
use crate::sink::{self, Call, RowPrefix, Sink};

/// What each request may print, as on Vercel: this many lines, and this much text in all.
const REQUEST_LINES: u32 = 256;
const REQUEST_BYTES: usize = 1 << 20;
/// A request's outgoing calls are always kept. One that reports more than this is taken for a
/// loop: about a hundred times the most any request has made (112).
const REQUEST_CALLS: u32 = 10_000;
/// For what belongs to no request, and what a request writes past its own limits: this much a
/// second per deployment on average, and this much at once after a quiet spell, counting each
/// line as its text plus `ENVELOPE`. For lines of 120 bytes that is about 310 a second and bursts
/// of 5,000.
const REST_BYTES_PER_SEC: f64 = 128.0 * 1024.0;
const REST_BURST_BYTES: f64 = 2.0 * 1024.0 * 1024.0;
/// About what the router's log adds to each line it writes: the time, deployment, worker, request.
const ENVELOPE: usize = 300;
/// For this long after a request's access log line, lines that arrive for it still count as its
/// own; then what was dropped of them is reported, and the request forgotten.
const LATE: Duration = Duration::from_secs(10);
/// Dropped lines are reported at most this often.
const REPORT_EVERY: Duration = Duration::from_secs(1);
/// A worker's stream holding more than this without a newline is broken; it's dropped.
const MAX_PENDING: usize = 4 << 20;

/// Ends the worker's log stream when dropped. The `Worker` holds it.
pub struct Follow {
    _stop: oneshot::Sender<()>,
}

/// Open the log stream on a worker's tunnel and log what comes back, until the `Follow` drops or
/// the worker ends the stream.
pub fn follow(
    sender: &SendRequest<ProxyBody>,
    token: &str,
    project: &str,
    deployment: &str,
    worker: &str,
    limiter: std::sync::Arc<LogLimiter>,
    background: std::sync::Arc<Background>,
    net: Option<(crate::spend::Owner, std::sync::Arc<std::sync::atomic::AtomicU64>)>,
) -> Follow {
    let (stop_tx, mut stop) = oneshot::channel::<()>();
    let request = Request::get(format!("http://forkrail-worker{LOGS_PATH}"))
        .version(Version::HTTP_2)
        .header(LOGS_TOKEN_HEADER, token)
        .body(Empty::new().map_err(|never| match never {}).boxed())
        .expect("a valid request");
    let mut sender = sender.clone();
    let (project, deployment, worker) = (project.to_owned(), deployment.to_owned(), worker.to_owned());
    // With a sink, rows go straight to ClickHouse; without one, into the router's log.
    let direct: Option<(&'static Sink, RowPrefix)> = sink::get().map(|sink| (&**sink, sink.prefix(&project, &deployment, &worker)));
    tokio::spawn(async move {
        // The response doesn't need the handle; holding it would keep the tunnel open.
        let response = sender.send_request(request);
        drop(sender);
        let response = tokio::select! {
            response = response => response,
            _ = &mut stop => return,
        };
        let mut body = match response {
            Ok(response) if response.status().is_success() => response.into_body(),
            Ok(response) => {
                warn!(%worker, status = response.status().as_u16(), "log stream refused");
                return;
            }
            Err(e) => {
                debug!(%worker, error = %e, "log stream failed");
                return;
            }
        };
        let mut pending = BytesMut::new();
        loop {
            let frame = tokio::select! {
                frame = body.frame() => frame,
                // Dropping the body resets the stream.
                _ = &mut stop => break,
            };
            let Some(Ok(frame)) = frame else { break };
            let Ok(data) = frame.into_data() else { continue };
            pending.extend_from_slice(&data);
            while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                let line = pending.split_to(end + 1);
                write_line(&line[..end], &project, &deployment, &worker, &limiter, direct.as_ref(), &background, net.as_ref());
            }
            if pending.len() > MAX_PENDING {
                warn!(%worker, bytes = pending.len(), "log stream line too long; dropping it");
                pending.advance(pending.len());
            }
        }
        limiter.flush();
        // No stream, no word of the work: the worker isn't kept for what can't be followed.
        background.set(0);
    });
    Follow { _stop: stop_tx }
}

/// How much work a worker's app is doing behind its answers (`waitUntil`, Next's `after()`), as
/// the worker says on its log stream (`LogLine::Background`): the pool keeps a worker while there
/// is some (`pool.rs`).
pub struct Background {
    running: std::sync::atomic::AtomicU64,
    /// When it last went from none to some, and from some to none.
    began: std::sync::Mutex<Option<Instant>>,
    ended: std::sync::Mutex<Option<Instant>>,
    /// Wakes the pool's reaper, which may have a worker to let go.
    changed: std::sync::Arc<tokio::sync::Notify>,
}

impl Background {
    pub fn new(changed: std::sync::Arc<tokio::sync::Notify>) -> Self {
        Self { running: 0.into(), began: Default::default(), ended: Default::default(), changed }
    }

    pub fn running(&self) -> u64 {
        self.running.load(std::sync::atomic::Ordering::Acquire)
    }

    /// How long the latest stretch of such work has lasted (so far, or in all once it ended).
    pub fn lasted(&self, now: Instant) -> Option<Duration> {
        let began = (*self.began.lock().unwrap())?;
        let until = if self.running() > 0 { now } else { (*self.ended.lock().unwrap())? };
        Some(until.saturating_duration_since(began))
    }

    pub fn set(&self, running: u64) {
        let before = self.running.swap(running, std::sync::atomic::Ordering::AcqRel);
        if (before > 0) == (running > 0) {
            return;
        }
        let stamp = if running > 0 { &self.began } else { &self.ended };
        *stamp.lock().unwrap() = Some(Instant::now());
        self.changed.notify_waiters();
    }
}

#[allow(clippy::too_many_arguments)]
fn write_line(
    raw: &[u8],
    project: &str,
    deployment: &str,
    worker: &str,
    limiter: &LogLimiter,
    direct: Option<&(&Sink, RowPrefix)>,
    background: &Background,
    net: Option<&(crate::spend::Owner, std::sync::Arc<std::sync::atomic::AtomicU64>)>,
) {
    match serde_json::from_slice::<LogLine>(raw) {
        Ok(LogLine::Background { background: running }) => background.set(running),
        // What the app sent the internet itself: on the spend ledger now (`spend.rs`), and noted so
        // the invocation's report adds only what the stream didn't carry.
        Ok(LogLine::Net { net_out }) => {
            if let Some((owner, streamed)) = net {
                crate::spend::ledger().add(owner, forkrail_common::prices::Meter::FnEgressBytes, None, net_out);
                streamed.fetch_add(net_out, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Ok(LogLine::Line { stream, line, request, .. }) if stream == FETCH_STREAM => {
            if limiter.admit(Kind::Call, request.as_deref(), line.len(), Instant::now()) {
                write_fetch(&line, project, deployment, worker, request.as_deref(), direct);
            }
        }
        Ok(LogLine::Line { ts, stream, line, request }) => {
            if limiter.admit(Kind::Line, request.as_deref(), line.len(), Instant::now()) {
                match direct {
                    Some((sink, prefix)) => sink.app_line(prefix, ts, request.as_deref(), &stream, &line),
                    None => info!(project, deployment, worker, request = request.as_deref(), stream, line_ts = ts, line = line.as_str(), "app log"),
                }
            }
        }
        Ok(LogLine::Dropped { dropped }) => {
            warn!(deployment, worker, dropped, "the worker's log buffer overflowed; lines were lost");
        }
        Err(e) => warn!(worker, error = %e, "bad log line from worker"),
    }
}

/// What the runtime calls the shims' reports of outgoing calls on the log stream.
const FETCH_STREAM: &str = "fetch";
const MAX_LOGGED_URL: usize = 1024;
const MAX_LOGGED_ERROR: usize = 300;

/// An outgoing call, as the shims report it.
#[derive(Deserialize)]
struct Fetch {
    method: Option<String>,
    url: Option<String>,
    /// Seconds since the epoch, when the call began, and how long it took in ms.
    start: Option<f64>,
    ms: Option<f64>,
    status: Option<u16>,
    error: Option<String>,
    bytes: Option<u64>,
}

fn write_fetch(raw: &str, project: &str, deployment: &str, worker: &str, request: Option<&str>, direct: Option<&(&Sink, RowPrefix)>) {
    let Ok(f) = serde_json::from_str::<Fetch>(raw) else {
        warn!(worker, "bad outgoing call report from worker");
        return;
    };
    let url = mask_url(f.url.as_deref().unwrap_or(""));
    let (host, path, query) = split_url(&url);
    let error = f.error.map(|e| clip(&redact_userinfo(&e), MAX_LOGGED_ERROR));
    let method = f.method.as_deref().unwrap_or("GET");
    let path = clip(path, MAX_LOGGED_URL);
    let query = query.map(|q| clip(q, MAX_LOGGED_URL));
    if let Some((sink, prefix)) = direct {
        let call = Call { request, start: f.start, method, host, path: &path, query: query.as_deref(), status: f.status, ms: f.ms, bytes: f.bytes, error: error.as_deref() };
        return sink.outgoing(prefix, &call);
    }
    info!(
        project,
        deployment,
        worker,
        request,
        method,
        host,
        path = path.as_str(),
        query,
        status = f.status,
        ms = f.ms,
        bytes = f.bytes,
        error,
        start_ts = f.start,
        "outgoing"
    );
}

/// `scheme://host[:port]/path?query` as (host with port, path, query).
fn split_url(url: &str) -> (&str, &str, Option<&str>) {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (rest, None),
    };
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..], query),
        None => (rest, "/", query),
    }
}

/// Text with any `scheme://user:password@` cut to `scheme://`: errors may quote a URL or a proxy.
fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        out.push_str(&rest[..i + 3]);
        rest = &rest[i + 3..];
        let end = rest.find(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '\'' | '"' | ')')).unwrap_or(rest.len());
        if let Some(at) = rest[..end].rfind('@') {
            rest = &rest[at + 1..];
        }
    }
    out.push_str(rest);
    out
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// What a request's log is missing: lines and outgoing calls that came while it was in flight
/// and weren't kept.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    pub lines: u32,
    pub calls: u32,
}

#[derive(Clone, Copy)]
enum Kind {
    Line = 0,
    Call = 1,
}

/// A deployment's log budgets, shared by its workers: what each request may write of its own,
/// and a bucket of `REST_BURST_BYTES`, refilled at `REST_BYTES_PER_SEC`, for the rest, one for the
/// app's lines and one for its outgoing calls.
pub struct LogLimiter {
    /// For its reports.
    deployment: String,
    state: Mutex<State>,
}

struct State {
    budgets: [Budget; 2],
    /// The requests the deployment's workers have, from `begin`, and those that ended within
    /// `LATE`, oldest first in `ended`.
    requests: HashMap<String, Counts>,
    ended: VecDeque<(Instant, String)>,
}

/// One request's lines and calls so far.
#[derive(Default)]
struct Counts {
    /// What it has written of its own allowance.
    lines: u32,
    bytes: usize,
    calls: u32,
    /// Not kept, by `Kind`.
    dropped: [u32; 2],
    /// Its access log line is written, with `dropped`: what's dropped from here on is `late`.
    ended: bool,
    late: [u32; 2],
}

impl Counts {
    /// Whether one more, of `bytes` of text, is within what the request may write; counts it if so.
    fn take(&mut self, kind: Kind, bytes: usize) -> bool {
        match kind {
            Kind::Line if self.lines < REQUEST_LINES && self.bytes + bytes <= REQUEST_BYTES => {
                self.lines += 1;
                self.bytes += bytes;
                true
            }
            Kind::Call if self.calls < REQUEST_CALLS => {
                self.calls += 1;
                true
            }
            _ => false,
        }
    }
}

struct Budget {
    what: &'static str,
    /// For what no request's own allowance covers, in bytes.
    rest: Tokens,
    /// Dropped since the last report.
    dropped: u64,
    reported: Instant,
}

struct Tokens {
    tokens: f64,
    per_sec: f64,
    burst: f64,
    refilled: Instant,
}

impl Tokens {
    fn new(per_sec: f64, burst: f64, now: Instant) -> Self {
        Self { tokens: burst, per_sec, burst, refilled: now }
    }

    fn take(&mut self, units: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.burst);
        self.refilled = now;
        let enough = self.tokens >= units;
        if enough {
            self.tokens -= units;
        }
        enough
    }
}

impl LogLimiter {
    pub fn new(deployment: &str) -> Self {
        let now = Instant::now();
        let budget = |what| Budget { what, rest: Tokens::new(REST_BYTES_PER_SEC, REST_BURST_BYTES, now), dropped: 0, reported: now };
        Self {
            deployment: deployment.to_owned(),
            state: Mutex::new(State {
                budgets: [budget("app log lines"), budget("outgoing call reports")],
                requests: HashMap::new(),
                ended: VecDeque::new(),
            }),
        }
    }

    /// A request is about to go to a worker: from now its lines and calls are counted for it.
    pub fn begin(&self, request: &str) {
        self.state.lock().unwrap().requests.insert(request.to_owned(), Counts::default());
    }

    /// The request's access log line is being written: what its log is missing. Its late lines
    /// keep its allowance for `LATE`.
    pub fn end(&self, request: &str) -> Dropped {
        self.end_at(request, Instant::now())
    }

    fn end_at(&self, request: &str, now: Instant) -> Dropped {
        let mut state = self.state.lock().unwrap();
        let Some(counts) = state.requests.get_mut(request) else { return Dropped::default() };
        counts.ended = true;
        let dropped = Dropped { lines: counts.dropped[Kind::Line as usize], calls: counts.dropped[Kind::Call as usize] };
        state.ended.push_back((now, request.to_owned()));
        dropped
    }

    /// Whether to write one now, of `bytes` of text, for `request` if it names one: always while
    /// the request is within what it may write, and otherwise if the deployment's bucket for the
    /// rest has room. What's dropped is counted for the deployment and for the request.
    fn admit(&self, kind: Kind, request: Option<&str>, bytes: usize, now: Instant) -> bool {
        let k = kind as usize;
        let mut state = self.state.lock().unwrap();
        let State { budgets, requests, .. } = &mut *state;
        let budget = &mut budgets[k];
        let mut counts = request.and_then(|id| requests.get_mut(id));
        if counts.as_deref_mut().is_some_and(|c| c.take(kind, bytes)) || budget.rest.take((bytes + ENVELOPE) as f64, now) {
            return true;
        }
        if budget.dropped == 0 {
            // The first drop of a spell: report it a second from now, with what follows.
            budget.reported = now;
        }
        budget.dropped += 1;
        if let Some(counts) = counts {
            let count = if counts.ended { &mut counts.late[k] } else { &mut counts.dropped[k] };
            *count = count.saturating_add(1);
        }
        false
    }

    /// Now and then (the pool's reaper calls it every tick): report what the deployment dropped,
    /// once a second at most, and forget requests that ended `LATE` ago, reporting what each
    /// lost after its access log line.
    pub fn tick(&self) {
        self.tick_at(Instant::now());
    }

    fn tick_at(&self, now: Instant) {
        // Written once the lock is released: requests take it too, and a log write can wait.
        let (dropped, late) = {
            let mut state = self.state.lock().unwrap();
            let due = |b: &Budget| b.dropped > 0 && now.saturating_duration_since(b.reported) >= REPORT_EVERY;
            let dropped = state.budgets.each_mut().map(|b| (b.what, if due(b) { std::mem::take(&mut b.dropped) } else { 0 }));
            (dropped, state.forget_ended(now))
        };
        for (what, dropped) in dropped {
            report(&self.deployment, what, dropped);
        }
        for (request, late) in late {
            info!(
                deployment = self.deployment.as_str(),
                request,
                lines_dropped = late[Kind::Line as usize],
                calls_dropped = late[Kind::Call as usize],
                "late drops"
            );
        }
    }

    /// Report what was dropped so far, when a stream ends.
    fn flush(&self) {
        let dropped = self.state.lock().unwrap().budgets.each_mut().map(|b| (b.what, std::mem::take(&mut b.dropped)));
        for (what, dropped) in dropped {
            report(&self.deployment, what, dropped);
        }
    }
}

impl State {
    /// Forget requests that ended `LATE` ago. Returns those that lost lines or calls since.
    fn forget_ended(&mut self, now: Instant) -> Vec<(String, [u32; 2])> {
        let mut late = Vec::new();
        while self.ended.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) >= LATE) {
            let (_, request) = self.ended.pop_front().expect("checked");
            if let Some(counts) = self.requests.remove(&request)
                && counts.late != [0, 0]
            {
                late.push((request, counts.late));
            }
        }
        late
    }
}

fn report(deployment: &str, what: &str, dropped: u64) {
    if dropped > 0 {
        warn!(deployment, dropped, "dropped {dropped} {what}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dropped_so_far(limiter: &LogLimiter, kind: Kind) -> u64 {
        limiter.state.lock().unwrap().budgets[kind as usize].dropped
    }

    /// A line that takes 512 bytes of the rest's bucket, so the bucket empties exactly: how many
    /// it takes at once, and in a second.
    const LINE: usize = 512 - ENVELOPE;
    const BURST: usize = REST_BURST_BYTES as usize / 512;
    const PER_SEC: usize = REST_BYTES_PER_SEC as usize / 512;

    /// Lines no request is named for share the rest's bucket, by their size.
    #[test]
    fn lines_outside_requests_take_a_burst_then_the_rate() {
        let limiter = LogLimiter::new("d");
        let start = Instant::now();
        let line = |at| limiter.admit(Kind::Line, None, LINE, at);
        assert_eq!((0..BURST + 1000).filter(|_| line(start)).count(), BURST);
        assert_eq!(dropped_so_far(&limiter, Kind::Line), 1000);
        // Half a second refills half a second's worth; the drops aren't a second old yet, so not reported.
        let later = start + Duration::from_millis(500);
        assert_eq!((0..PER_SEC).filter(|_| line(later)).count(), PER_SEC / 2);
        limiter.tick_at(start + Duration::from_millis(999));
        assert_eq!(dropped_so_far(&limiter, Kind::Line), 1000 + (PER_SEC - PER_SEC / 2) as u64);
        // A second after the first drop, the next tick reports them and the count starts again.
        limiter.tick_at(start + Duration::from_millis(1001));
        assert_eq!(dropped_so_far(&limiter, Kind::Line), 0);
        // An hour refills to the burst only; long lines take more of it; calls have a bucket of their own.
        let hour = start + Duration::from_secs(3600);
        let long = REST_BURST_BYTES as usize / (16 * 1024 + ENVELOPE);
        assert_eq!((0..10_000).filter(|_| limiter.admit(Kind::Line, None, 16 * 1024, hour)).count(), long);
        assert_eq!((0..10_000).filter(|_| limiter.admit(Kind::Call, None, LINE, hour)).count(), BURST);
    }

    #[test]
    fn a_request_keeps_what_vercel_would_however_busy_the_deployment() {
        let limiter = LogLimiter::new("d");
        let start = Instant::now();
        // A request that floods: its own 256 lines, then the rest's bucket, then nothing.
        limiter.begin("flood");
        let kept = (0..10_000).filter(|_| limiter.admit(Kind::Line, Some("flood"), LINE, start)).count();
        assert_eq!(kept, REQUEST_LINES as usize + BURST);
        // With that bucket empty, a thousand requests at the same instant still keep every line
        // and every call of their own: nothing is limited per second.
        for i in 0..1000 {
            let id = format!("r{i}");
            limiter.begin(&id);
            assert!((0..REQUEST_LINES).all(|_| limiter.admit(Kind::Line, Some(&id), LINE, start)));
            assert!((0..50).all(|_| limiter.admit(Kind::Call, Some(&id), 300, start)));
            assert_eq!(limiter.end_at(&id, start), Dropped::default());
        }
        // Past its own lines a request has only the rest's bucket, and its log line says what it lost.
        limiter.begin("chatty");
        assert_eq!((0..300).filter(|_| limiter.admit(Kind::Line, Some("chatty"), LINE, start)).count(), REQUEST_LINES as usize);
        assert_eq!(limiter.end_at("chatty", start), Dropped { lines: 44, calls: 0 });
        assert_eq!(limiter.end_at("flood", start), Dropped { lines: 10_000 - kept as u32, calls: 0 });
        // A request the router never began (an older line, the default pool's start-up) has none.
        assert!(!limiter.admit(Kind::Line, Some("unknown"), LINE, start));
        assert_eq!(limiter.end_at("unknown", start), Dropped::default());
    }

    #[test]
    fn a_request_has_a_megabyte_and_its_calls() {
        let limiter = LogLimiter::new("d");
        let start = Instant::now();
        // Empty both of the rest's buckets, so only requests' own allowances are left.
        while limiter.admit(Kind::Line, None, LINE, start) {}
        while limiter.admit(Kind::Call, None, LINE, start) {}
        // 16 KiB lines: 64 make the megabyte, well short of 256 lines.
        limiter.begin("big");
        assert_eq!((0..100).filter(|_| limiter.admit(Kind::Line, Some("big"), 16 * 1024, start)).count(), 64);
        assert!(!limiter.admit(Kind::Line, Some("big"), LINE, start));
        // A 256 KiB line fits four times.
        limiter.begin("huge");
        assert_eq!((0..10).filter(|_| limiter.admit(Kind::Line, Some("huge"), 256 * 1024, start)).count(), 4);
        // Calls: all of them, until the request looks like a loop.
        limiter.begin("calls");
        assert_eq!((0..REQUEST_CALLS + 5).filter(|_| limiter.admit(Kind::Call, Some("calls"), 300, start)).count(), REQUEST_CALLS as usize);
        assert_eq!(limiter.end_at("calls", start), Dropped { lines: 0, calls: 5 });
        assert_eq!(limiter.end_at("big", start), Dropped { lines: 37, calls: 0 });
    }

    #[test]
    fn late_lines_keep_the_request_for_a_while() {
        let limiter = LogLimiter::new("d");
        let start = Instant::now();
        while limiter.admit(Kind::Line, None, LINE, start) {}
        limiter.begin("r");
        assert!(limiter.admit(Kind::Line, Some("r"), LINE, start));
        assert_eq!(limiter.end_at("r", start), Dropped::default());
        // After its access log line, its lines still come out of its allowance, and what's
        // dropped of them is counted apart.
        let soon = start + Duration::from_millis(1);
        assert_eq!((0..300).filter(|_| limiter.admit(Kind::Line, Some("r"), LINE, soon)).count(), REQUEST_LINES as usize - 1);
        assert!(limiter.admit(Kind::Call, Some("r"), LINE, soon));
        // Not forgotten before `LATE` has passed; then it is, with what it lost since.
        assert!(limiter.state.lock().unwrap().forget_ended(start + LATE - Duration::from_millis(1)).is_empty());
        assert_eq!(limiter.state.lock().unwrap().forget_ended(start + LATE), [("r".to_owned(), [45, 0])]);
        let state = limiter.state.lock().unwrap();
        assert!(state.requests.is_empty() && state.ended.is_empty());
    }

    #[test]
    fn outgoing_urls_lose_secrets() {
        let url = mask_url("https://user:pw@api.example.com:8443/v1/x?q=1&api_key=abc#frag");
        assert_eq!(url, "https://api.example.com:8443/v1/x?q=1&api_key=REDACTED");
        assert_eq!(split_url(&url), ("api.example.com:8443", "/v1/x", Some("q=1&api_key=REDACTED")));
        assert_eq!(split_url("http://h"), ("h", "/", None));
        assert_eq!(
            redact_userinfo("ProxyError('http://u:p@gw.proxy.io:823/' failed), see https://curl.se/x"),
            "ProxyError('http://gw.proxy.io:823/' failed), see https://curl.se/x"
        );
        assert_eq!(clip("añb", 2), "a");
    }
}
