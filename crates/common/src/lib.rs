//! Types and helpers shared by the router, the Lambda worker runtime and the CLI.

pub mod domains;
#[cfg(feature = "envfile")]
pub mod envfile;
#[cfg(feature = "rules")]
pub mod firewall;
#[cfg(feature = "images")]
pub mod images;
pub mod install;
pub mod loops;
pub mod prices;
#[cfg(feature = "rules")]
pub mod next;
pub mod nextcache;
pub mod protection;
pub mod purge;
pub mod regions;
pub mod routes;
#[cfg(feature = "rules")]
pub mod rules;
pub mod schedules;
pub mod statics;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Buf;
use http::HeaderMap;
use http::header::{CONNECTION, HeaderName, TE, TRANSFER_ENCODING, UPGRADE};
use http_body::{Body, Frame, SizeHint};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;

/// Payload the router sends in `lambda:Invoke` to start a worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokePayload {
    pub worker_id: String,
    /// `host:port` of the router's tunnel listener.
    pub router_addr: String,
    /// Single-use secret the worker presents in its preface.
    pub token: String,
    /// Hex SHA-256 of the router's tunnel certificate (DER). The worker pins it.
    pub cert_sha256: String,
    pub max_inflight: u32,
    /// Safety net: the worker ends its invocation after this long with nothing in flight.
    /// The router normally retires idle workers sooner by closing the tunnel.
    pub idle_ms: u64,
    /// The deployment's maxDuration, in seconds: how long work an app hands `waitUntil` (Next's
    /// `after()` too) may go on once its request is answered. A router that sends it also wants to
    /// be told of such work on the log stream ([`LogLine::Background`]), and keeps the worker
    /// meanwhile. Routers from before it send none, and runtimes from before it ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_s: Option<u64>,
    /// The install's own hosts (`loops::own_host`'s entries), which the app's outgoing calls carry
    /// the loop guard's mark to. None from older routers: the runtime then leaves the list it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_hosts: Option<Vec<String>>,
    /// Report what the environment sends to anywhere but the router (`LogLine::Net`). Asked only
    /// on Lambda: a local worker shares its machine's network.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub report_net: bool,
}

/// Payload that asks an environment to be ready and nothing more: Lambda starts one if none is
/// free, the runtime starts the app in it, and the invocation returns. No request reaches the
/// app, and no router is dialed. The environment then waits frozen, so the first real worker on
/// that version thaws in milliseconds instead of starting cold. Sent before production is pointed
/// at a deployment (the CLI's warm.rs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarmRequest {
    /// Always true: what tells this payload from a worker's.
    pub warm: bool,
    /// How long to stay in the invocation once ready, so that warm invocations sent together
    /// overlap and each gets an environment of its own.
    #[serde(default)]
    pub hold_ms: u64,
}

/// The most a warm invocation is held.
pub const WARM_HOLD_MAX_MS: u64 = 2_000;

/// What a warm invocation answers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarmReport {
    pub warm: bool,
    /// The app is listening (or the deployment has none of its own).
    pub ok: bool,
    pub env_id: String,
    /// How many invocations this environment has served, this one included: 1 for a new one.
    pub env_invocations: u64,
    /// How long the app took to start, if it started for this invocation.
    #[serde(default)]
    pub app_start_ms: u64,
    /// Why the app didn't start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// TLS server name of the router's self-signed tunnel certificate.
pub const TUNNEL_SERVER_NAME: &str = "forkrail-router";

/// When set, the runtime runs one invocation from this JSON payload instead of
/// polling the Lambda Runtime API. Used for local testing.
pub const LOCAL_PAYLOAD_ENV: &str = "FORKRAIL_LOCAL_PAYLOAD";

/// What the worker returns when its invocation ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerReport {
    pub worker_id: String,
    /// Random id of the execution environment; stays the same across warm invocations.
    pub env_id: String,
    /// How many invocations this environment has served, including this one.
    pub env_invocations: u64,
    pub requests: u64,
    pub connect_ms: u64,
    pub active_ms: u64,
    pub end_reason: EndReason,
    /// How long the user's app took to start, if it started for this invocation; zero otherwise.
    #[serde(default)]
    pub app_start_ms: u64,
    /// Peak resident memory of the runtime and the app during this invocation, in KiB.
    pub rss_peak_kb: u64,
    /// The environment's total memory, and what was still available when the invocation ended, in KiB.
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    /// Bytes the environment sent anywhere but the router in this invocation, every one (the log
    /// stream's `LogLine::Net` carried all but the last moments'). None when not asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_out: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Idle,
    Deadline,
    RouterClosed,
    /// The user's app exited, so the worker dropped the tunnel.
    AppExited,
}

impl EndReason {
    /// As it's serialized.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Deadline => "deadline",
            Self::RouterClosed => "router_closed",
            Self::AppExited => "app_exited",
        }
    }
}

const PREFACE_PREFIX: &str = "FORKRAIL/1";
const PREFACE_ACK: &[u8] = b"OK\n";
const MAX_LINE: usize = 512;

/// What a worker announces when it dials the router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preface {
    pub worker_id: String,
    pub token: String,
    /// The function's configured memory and the worker's resident memory, in KiB. Zero where unknown.
    pub limit_kb: u64,
    pub rss_kb: u64,
    /// The worker serves the app's output at [`LOGS_PATH`]. Older runtimes don't send the flag, so
    /// the router never asks them, and their apps never see the path.
    pub logs: bool,
    /// How many invocations this Lambda environment has served, this one included: 1 is a cold
    /// start, more a ready environment taking another invocation (a quick start). Older runtimes
    /// don't send it.
    pub env_invocations: Option<u64>,
    /// The worker's app is a Next.js server whose caches are the router's (`nextcache`): the router
    /// opens the cache stream on its tunnel. Older runtimes don't send the flag.
    pub cache: bool,
}

/// Preface feature flag: the worker streams the app's output.
const FEATURE_LOGS: &str = "logs";
/// Preface feature `env=N`: the environment's invocation count. Routers that don't know it ignore it.
const FEATURE_ENV: &str = "env=";
/// Preface feature flag: the worker's app uses the router as Next.js's shared cache.
const FEATURE_CACHE: &str = "cache";

/// The router opens one long-lived `GET` on this path to each worker that announced `logs`, with
/// [`LOGS_TOKEN_HEADER`] set to the worker's single-use token, so a public request for the same path
/// still goes to the app. The response is a stream of JSON lines (see [`LogLine`]). It isn't a
/// request: neither side counts it as in flight.
pub const LOGS_PATH: &str = "/_forkrail/logs";
pub const LOGS_TOKEN_HEADER: &str = "x-forkrail-logs-token";

/// One line of the app's output, or a note that lines were dropped, as the worker sends them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LogLine {
    Line {
        /// Seconds since the epoch, when the runtime read the line.
        ts: f64,
        /// `stdout` or `stderr`.
        stream: String,
        line: String,
        /// The router's id of the request the app was serving when it wrote the line, where the
        /// shim can tell (`x-forkrail-request-id`). Older runtimes don't send it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<String>,
    },
    /// The worker's buffer overflowed, or lines were cut, while no one was reading.
    Dropped { dropped: u64 },
    /// How much work the app's functions handed `waitUntil` is running now (zero: none any more).
    /// Sent when it changes, and only to a router that asked ([`InvokePayload::max_duration_s`]):
    /// while there is some the router keeps the worker, so the work isn't cut off from its logs
    /// and the next request doesn't find the environment busy and start another.
    Background { background: u64 },
    /// Bytes the environment sent on the wire to anywhere but the router since the last of these
    /// (the app's own calls and uploads), for the spend ledger. Only to a router that asked
    /// ([`InvokePayload::report_net`]).
    Net { net_out: u64 },
}

/// Response headers in which the worker reports its resident memory in KiB, when the
/// request arrived and when the response started. The router strips them.
pub const RSS_START_HEADER: &str = "x-forkrail-rss-start-kb";
pub const RSS_HEADER: &str = "x-forkrail-rss-kb";
/// For a request that arrived with nothing else in flight: the peak since it arrived. Memory it
/// used and freed before answering counts too, which matters for Node, whose garbage collector
/// often frees a request's memory before the response starts.
pub const RSS_PEAK_HEADER: &str = "x-forkrail-rss-peak-kb";

/// The app's route template for the request (`/items/{item_id}`), where the shim can tell. The
/// router logs it and strips it.
pub const ROUTE_HEADER: &str = "x-forkrail-route";
/// The largest header block (HTTP/2's `SETTINGS_MAX_HEADER_LIST_SIZE`) either end of a tunnel
/// takes, the router's answers' and the worker's requests'. Far above what the router lets in from
/// a client (`proxy.rs`, about 128 KB, as Vercel), so a request never meets it; and big enough that
/// h2 counts an oversized block as one stream's error before it counts it as a CONTINUATION flood,
/// which ends the whole connection (every request in flight on the worker).
pub const TUNNEL_MAX_HEADER_LIST: u32 = 1 << 20;

/// Sent with a request to a deployment of several functions in one (Vercel's `api/*.go` and
/// `api/*.rs` files): the function path its routes reached (`/api/users`, `/api/[id]`), which the
/// app's dispatcher picks the function by. The router takes any a client sends off first.
pub const FUNCTION_HEADER: &str = "x-forkrail-function";

/// An AWS region as Vercel names its compute regions (`VERCEL_REGION`, the first part of
/// `x-vercel-id`), or None where Vercel has none.
pub fn vercel_region(aws: &str) -> Option<&'static str> {
    regions::FUNCTION_REGIONS.iter().find(|r| r.aws == aws).map(|r| r.vercel)
}

/// For a request that had the worker to itself from arrival until its response started: the CPU
/// time the environment used meanwhile (the runtime and the app, since Lambda divides one CPU
/// share among them), and the wall time it took, in microseconds. The router learns from them
/// what each route costs in CPU.
pub const CPU_HEADER: &str = "x-forkrail-cpu-us";
pub const BUSY_HEADER: &str = "x-forkrail-busy-us";
/// On a request: sample its CPU use over time if it runs alone. The runtime strips it.
pub const PROFILE_REQUEST_HEADER: &str = "x-forkrail-profile";
/// On the response to such a request: comma-separated `elapsed_us:cpu_us` samples since it
/// arrived, a few milliseconds apart, so a tuner can see when in a request its CPU work happens.
pub const PROFILE_HEADER: &str = "x-forkrail-cpu-profile";

/// On a worker's response: the request didn't finish on this worker, through no fault of its own,
/// so the router may retry it as it would after a failure before any response. The value says why:
/// - `app-exited`: the app died of another request, and the worker goes with it.
/// - `app-connection`: the connection to the app failed under it, with the app still running (it
///   closed a kept-alive connection the request had just been sent on). The worker is fine.
pub const RETRY_HEADER: &str = "x-forkrail-retry";
pub const RETRY_APP_EXITED: &str = "app-exited";
pub const RETRY_APP_CONNECTION: &str = "app-connection";
/// With `app-connection`: what failed, for the router's log.
pub const APP_ERROR_HEADER: &str = "x-forkrail-app-error";

/// Worker side: announce ourselves on a fresh tunnel connection and wait for the ack.
pub async fn send_preface<S>(io: &mut S, preface: &Preface) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Preface { worker_id, token, limit_kb, rss_kb, logs, env_invocations, cache } = preface;
    let mut features: Vec<String> = Vec::new();
    if *logs {
        features.push(FEATURE_LOGS.into());
    }
    if let Some(n) = env_invocations {
        features.push(format!("{FEATURE_ENV}{n}"));
    }
    if *cache {
        features.push(FEATURE_CACHE.into());
    }
    let features = if features.is_empty() { String::new() } else { format!(" {}", features.join(",")) };
    let line = format!("{PREFACE_PREFIX} {worker_id} {token} {limit_kb} {rss_kb}{features}\n");
    io.write_all(line.as_bytes()).await?;
    io.flush().await?;
    let mut ack = [0u8; 3];
    io.read_exact(&mut ack).await?;
    if ack != PREFACE_ACK {
        return Err(std::io::Error::other("router rejected preface"));
    }
    Ok(())
}

/// Router side: read the preface line.
pub async fn read_preface<S>(io: &mut S) -> std::io::Result<Preface>
where
    S: AsyncRead + Unpin,
{
    parse_preface(&read_line(io).await?)
}

/// What a connection to the tunnel listener opens with: a worker's preface, or an egress request
/// from a worker's app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hello {
    Worker(Preface),
    Egress(EgressHello),
    /// A function's purge by tag (`purge::RuntimePurge`), from a live worker.
    Purge(PurgeHello),
    /// Next.js's shared cache (`nextcache`), from a worker whose tunnel is gone: frames both ways
    /// after the ack. `PurgeHello` has the two fields it needs.
    Cache(PurgeHello),
}

/// A purge from a worker's app (`@vercel/functions`' `invalidateByTag` and the rest): the runtime
/// dials the router as the live worker `worker_id` with its token, so the purge is of that
/// worker's project and environment only. After the ack, one line of JSON each way
/// (`purge::RuntimePurge`, then `purge::PurgeReply`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeHello {
    pub worker_id: String,
    pub token: String,
}

const PURGE_PREFIX: &str = "FORKRAIL-PURGE/1";
const CACHE_PREFIX: &str = "FORKRAIL-CACHE/1";

/// A connection the app opened to a private address through its router: the runtime dials the
/// router as the live worker `worker_id`, with its token, and names the target. After the ack the
/// connection carries the app's bytes both ways, untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressHello {
    pub worker_id: String,
    pub token: String,
    /// A name the router maps to an address (`FORKRAIL_EGRESS_TARGETS`), which the deployment must allow.
    pub target: String,
}

const EGRESS_PREFIX: &str = "FORKRAIL-EGRESS/1";

/// Router side: read the first line, whichever kind it is.
pub async fn read_hello<S>(io: &mut S) -> std::io::Result<Hello>
where
    S: AsyncRead + Unpin,
{
    let line = read_line(io).await?;
    match line.split_once(' ') {
        Some((EGRESS_PREFIX, rest)) => match rest.split(' ').collect::<Vec<_>>().as_slice() {
            [id, token, target] if !id.is_empty() && !token.is_empty() && !target.is_empty() => Ok(Hello::Egress(EgressHello {
                worker_id: (*id).to_owned(),
                token: (*token).to_owned(),
                target: (*target).to_owned(),
            })),
            _ => Err(std::io::Error::other("malformed egress request")),
        },
        Some((PURGE_PREFIX, rest)) => match rest.split(' ').collect::<Vec<_>>().as_slice() {
            [id, token] if !id.is_empty() && !token.is_empty() => Ok(Hello::Purge(PurgeHello { worker_id: (*id).to_owned(), token: (*token).to_owned() })),
            _ => Err(std::io::Error::other("malformed purge request")),
        },
        Some((CACHE_PREFIX, rest)) => match rest.split(' ').collect::<Vec<_>>().as_slice() {
            [id, token] if !id.is_empty() && !token.is_empty() => Ok(Hello::Cache(PurgeHello { worker_id: (*id).to_owned(), token: (*token).to_owned() })),
            _ => Err(std::io::Error::other("malformed cache request")),
        },
        _ => parse_preface(&line).map(Hello::Worker),
    }
}

/// Worker side: ask for a connection to `target`, and wait for the router to open it.
pub async fn send_egress<S>(io: &mut S, hello: &EgressHello) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let EgressHello { worker_id, token, target } = hello;
    io.write_all(format!("{EGRESS_PREFIX} {worker_id} {token} {target}\n").as_bytes()).await?;
    io.flush().await?;
    let mut ack = [0u8; 3];
    io.read_exact(&mut ack).await.map_err(|_| std::io::Error::other("router refused the connection"))?;
    if ack != PREFACE_ACK {
        return Err(std::io::Error::other("router refused the connection"));
    }
    Ok(())
}

/// Worker side: open a purge as `hello`'s worker, and wait for the router to take it.
pub async fn send_purge<S>(io: &mut S, hello: &PurgeHello) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let PurgeHello { worker_id, token } = hello;
    io.write_all(format!("{PURGE_PREFIX} {worker_id} {token}\n").as_bytes()).await?;
    io.flush().await?;
    let mut ack = [0u8; 3];
    io.read_exact(&mut ack).await.map_err(|_| std::io::Error::other("router refused the purge"))?;
    if ack != PREFACE_ACK {
        return Err(std::io::Error::other("router refused the purge"));
    }
    Ok(())
}

/// Worker side: open a cache connection as `hello`'s worker, and wait for the router to take it.
pub async fn send_cache<S>(io: &mut S, hello: &PurgeHello) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let PurgeHello { worker_id, token } = hello;
    io.write_all(format!("{CACHE_PREFIX} {worker_id} {token}\n").as_bytes()).await?;
    io.flush().await?;
    let mut ack = [0u8; 3];
    io.read_exact(&mut ack).await.map_err(|_| std::io::Error::other("router refused the cache connection"))?;
    if ack != PREFACE_ACK {
        return Err(std::io::Error::other("router refused the cache connection"));
    }
    Ok(())
}

/// One line of up to `max` bytes (without its newline), read in chunks through a buffer: for a
/// purge's JSON, after which nothing else comes on the connection.
pub async fn read_json_line<S>(io: &mut S, max: usize) -> std::io::Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut line = Vec::with_capacity(256);
    let mut reader = tokio::io::BufReader::new(io);
    loop {
        let buf = tokio::io::AsyncBufReadExt::fill_buf(&mut reader).await?;
        if buf.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the line ended early"));
        }
        if let Some(at) = buf.iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&buf[..at]);
            if line.len() > max {
                return Err(std::io::Error::other("line too long"));
            }
            return Ok(line);
        }
        line.extend_from_slice(buf);
        let n = buf.len();
        tokio::io::AsyncBufReadExt::consume(&mut reader, n);
        if line.len() > max {
            return Err(std::io::Error::other("line too long"));
        }
    }
}

/// One line, read byte by byte so nothing past the newline is consumed: HTTP/2 or the app's
/// bytes start right after.
async fn read_line<S>(io: &mut S) -> std::io::Result<String>
where
    S: AsyncRead + Unpin,
{
    let mut line = Vec::with_capacity(128);
    loop {
        let byte = io.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        if line.len() == MAX_LINE {
            return Err(std::io::Error::other("preface too long"));
        }
        line.push(byte);
    }
    String::from_utf8(line).map_err(std::io::Error::other)
}

fn parse_preface(line: &str) -> std::io::Result<Preface> {
    let parts: Vec<&str> = line.split(' ').collect();
    // Older runtimes send no features.
    let (fields, features) = match parts.len() {
        5 => (&parts[..], ""),
        6 => (&parts[..5], parts[5]),
        _ => return Err(std::io::Error::other("malformed preface")),
    };
    match fields {
        [PREFACE_PREFIX, id, token, limit_kb, rss_kb] => Ok(Preface {
            worker_id: (*id).to_owned(),
            token: (*token).to_owned(),
            limit_kb: limit_kb.parse().map_err(std::io::Error::other)?,
            rss_kb: rss_kb.parse().map_err(std::io::Error::other)?,
            logs: features.split(',').any(|f| f == FEATURE_LOGS),
            env_invocations: features.split(',').find_map(|f| f.strip_prefix(FEATURE_ENV)?.parse().ok()),
            cache: features.split(',').any(|f| f == FEATURE_CACHE),
        }),
        _ => Err(std::io::Error::other("malformed preface")),
    }
}

/// Router side: accept the worker after its preface checked out.
pub async fn ack_preface<S>(io: &mut S) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    io.write_all(PREFACE_ACK).await?;
    io.flush().await
}

/// Remove connection-specific headers. HTTP/2 rejects them, and they don't belong past a proxy anyway.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [CONNECTION, TRANSFER_ENCODING, UPGRADE, TE] {
        headers.remove(name);
    }
    headers.remove("keep-alive");
    headers.remove("proxy-connection");
}

/// Random lowercase hex string from `bytes` random bytes.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS random source unavailable");
    hex(&buf)
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[usize::from(b >> 4)] as char);
        out.push(DIGITS[usize::from(b & 0xf)] as char);
    }
    out
}

/// A unique id for something that needn't be unguessable, such as a request: 16 hex digits from a
/// per-thread generator, seeded once from the OS. `random_hex` asks the OS every time, which on a
/// busy router was a system call per request.
pub fn quick_id() -> String {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new({
            let mut seed = [0u8; 8];
            getrandom::fill(&mut seed).expect("OS random source unavailable");
            u64::from_le_bytes(seed)
        });
    }
    // SplitMix64.
    let mut z = STATE.with(|s| {
        let next = s.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        s.set(next);
        next
    });
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    hex(&(z ^ (z >> 31)).to_be_bytes())
}

/// Counts requests in flight and wakes waiters whenever the count drops.
#[derive(Debug)]
pub struct Inflight {
    count: AtomicUsize,
    changed: Arc<Notify>,
}

impl Inflight {
    /// `changed` may be shared, so one waiter can watch many counters.
    pub fn new(changed: Arc<Notify>) -> Self {
        Self { count: AtomicUsize::new(0), changed }
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    pub fn enter(self: &Arc<Self>) -> InflightGuard {
        self.count.fetch_add(1, Ordering::AcqRel);
        InflightGuard(self.clone())
    }

    /// Resolve once nothing has been in flight for `idle`.
    pub async fn wait_idle(&self, idle: Duration) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count() == 0 {
                tokio::select! {
                    _ = tokio::time::sleep(idle) => {
                        if self.count() == 0 {
                            return;
                        }
                    }
                    _ = notified => {}
                }
            } else {
                notified.await;
            }
        }
    }
}

/// Decrements its counter on drop.
#[derive(Debug)]
pub struct InflightGuard(Arc<Inflight>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}

pin_project_lite::pin_project! {
    /// Body wrapper that holds a guard until the body finishes or is dropped,
    /// so a request counts as in flight for as long as its response streams.
    pub struct TrackedBody<B, G> {
        #[pin]
        inner: B,
        guard: Option<G>,
    }
}

impl<B, G> TrackedBody<B, G> {
    pub fn new(inner: B, guard: G) -> Self {
        Self { inner, guard: Some(guard) }
    }
}

impl<B, G> Body for TrackedBody<B, G>
where
    B: Body,
    B::Data: Buf,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        let polled = this.inner.poll_frame(cx);
        if matches!(polled, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            this.guard.take();
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        let ids: std::collections::HashSet<String> = (0..10_000).map(|_| quick_id()).collect();
        assert_eq!(ids.len(), 10_000);
        assert!(ids.iter().all(|id| id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit())));
        assert_eq!(random_hex(4).len(), 8);
    }

    use super::*;

    #[tokio::test]
    async fn preface_round_trip() {
        let (mut worker, mut router) = tokio::io::duplex(1024);
        let router_side = tokio::spawn(async move {
            let got = read_preface(&mut router).await.unwrap();
            ack_preface(&mut router).await.unwrap();
            got
        });
        let sent = Preface { worker_id: "w1".into(), token: "tok".into(), limit_kb: 1_811_456, rss_kb: 4_096, logs: true, env_invocations: Some(3), cache: false };
        send_preface(&mut worker, &sent).await.unwrap();
        assert_eq!(router_side.await.unwrap(), sent);
    }

    #[tokio::test]
    async fn egress_hello_round_trip() {
        let (mut worker, mut router) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            let got = read_hello(&mut router).await.unwrap();
            ack_preface(&mut router).await.unwrap();
            got
        });
        let hello = EgressHello { worker_id: "d1.ab".into(), token: "tok".into(), target: "analytics".into() };
        send_egress(&mut worker, &hello).await.unwrap();
        assert_eq!(task.await.unwrap(), Hello::Egress(hello));
        // A worker's preface reads as one.
        let (mut worker, mut router) = tokio::io::duplex(1024);
        worker.write_all(b"FORKRAIL/1 w1 tok 1024 512 logs\n").await.unwrap();
        assert!(matches!(read_hello(&mut router).await.unwrap(), Hello::Worker(p) if p.logs && p.worker_id == "w1"));
        let (mut worker, mut router) = tokio::io::duplex(1024);
        worker.write_all(b"FORKRAIL-EGRESS/1 w1 tok\n").await.unwrap();
        assert!(read_hello(&mut router).await.is_err());
    }

    #[tokio::test]
    async fn older_prefaces_have_no_logs() {
        let (mut worker, mut router) = tokio::io::duplex(1024);
        worker.write_all(b"FORKRAIL/1 w1 tok 1024 512\n").await.unwrap();
        let got = read_preface(&mut router).await.unwrap();
        assert_eq!((got.limit_kb, got.rss_kb, got.logs, got.env_invocations), (1024, 512, false, None));
    }

    #[tokio::test]
    async fn prefaces_carry_the_invocation_count() {
        let (mut worker, mut router) = tokio::io::duplex(1024);
        worker.write_all(b"FORKRAIL/1 w1 tok 1024 512 logs,env=1\n").await.unwrap();
        let got = read_preface(&mut router).await.unwrap();
        assert_eq!((got.logs, got.env_invocations), (true, Some(1)));
        // A router before `env=` still finds `logs` among the features.
        assert!("logs,env=1".split(',').any(|f| f == FEATURE_LOGS));
    }

    #[test]
    fn log_lines_round_trip() {
        let line = LogLine::Line { ts: 1.5, stream: "stderr".into(), line: "boom \"x\"".into(), request: None };
        let json = serde_json::to_string(&line).unwrap();
        assert_eq!(serde_json::from_str::<LogLine>(&json).unwrap(), line);
        assert_eq!(serde_json::from_str::<LogLine>(r#"{"dropped":3}"#).unwrap(), LogLine::Dropped { dropped: 3 });
        assert_eq!(serde_json::to_string(&LogLine::Background { background: 2 }).unwrap(), r#"{"background":2}"#);
        assert_eq!(serde_json::from_str::<LogLine>(r#"{"background":0}"#).unwrap(), LogLine::Background { background: 0 });
    }

    #[tokio::test]
    async fn malformed_preface_is_rejected() {
        let (mut worker, mut router) = tokio::io::duplex(1024);
        worker.write_all(b"HELLO there\n").await.unwrap();
        assert!(read_preface(&mut router).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_idle_waits_for_last_guard() {
        let inflight = Arc::new(Inflight::new(Arc::new(Notify::new())));
        let guard = inflight.enter();
        let waiter = {
            let inflight = inflight.clone();
            tokio::spawn(async move { inflight.wait_idle(Duration::from_millis(100)).await })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!waiter.is_finished());
        drop(guard);
        waiter.await.unwrap();
        assert_eq!(inflight.count(), 0);
    }
}
