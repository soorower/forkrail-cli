//! App log lines and outgoing calls, from the router straight into ClickHouse.
//!
//! Until 2026-11-10 every line went the way the router's own log goes: a JSON event in the hour's
//! file, which Vector reads, parses, reshapes and sends on. Measured on a bench router under load,
//! Vector spends about 47 µs a line there and can't use more than one core, so at about 20,000
//! lines a second it falls behind however idle the router is; the router's own part is about 5 µs
//! a line. A request prints 18 lines on average and makes as many calls, so that path, not
//! proxying, set how many requests a box could serve.
//!
//! Here each row is written once, as a `JSONEachRow` line, into a buffer per table. A task sends
//! each buffer as one `INSERT` every `FLUSH_EVERY` (sooner when one fills), over one kept-alive
//! connection. Nothing reaches the disk unless ClickHouse can't be reached: then batches wait in
//! memory up to `MEMORY_LIMIT`, after that in files under the spill directory up to its limit,
//! and are sent oldest first once it answers again. Past both limits batches are dropped and
//! counted. What a stopping router hasn't sent it leaves in the spill directory, and the next one
//! (or the one already running beside it) takes it from there. A batch is resent byte for byte, so
//! ClickHouse's deduplication drops a repeat whose first answer was lost.
//!
//! Without `FORKRAIL_CLICKHOUSE_URL` there is no sink and lines go to the router's log as before
//! (local runs, tests).

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::BoxError;

/// How often the buffers are sent.
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// A buffer this full is sent without waiting for the tick.
const FLUSH_BYTES: usize = 4 << 20;
/// Unsent batches kept in memory before they go to the spill directory.
const MEMORY_LIMIT: usize = 64 << 20;
/// How long ClickHouse gets to take a batch.
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// And to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// After a failure, how long before the next try.
const RETRY_AFTER: Duration = Duration::from_secs(2);
/// What a stopping router gives ClickHouse to take what's buffered, before leaving it on disk.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// A batch ClickHouse refuses (a 4xx: bad rows, a missing table) this many times is set aside as
/// `bad.*` in the spill directory rather than block everything behind it.
const REFUSALS: u32 = 3;
/// Failures are reported at most this often.
const WARN_EVERY: Duration = Duration::from_secs(10);
/// How often the spill directory is checked for files a stopped router left.
const ADOPT_EVERY: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Table {
    AppLogs = 0,
    Outgoing = 1,
    /// The spend ledger's counts (`spend.rs`).
    Usage = 2,
}

const TABLES: [(Table, &str); 3] = [(Table::AppLogs, "app_logs"), (Table::Outgoing, "outgoing"), (Table::Usage, "usage")];

impl Table {
    fn name(self) -> &'static str {
        TABLES[self as usize].1
    }

    fn named(name: &str) -> Option<Table> {
        TABLES.iter().find(|(_, n)| *n == name).map(|(t, _)| *t)
    }
}

/// Where the rows go, from the environment (see `main.rs`).
pub struct Config {
    /// `http://host:8123` or `https://host:8443`.
    pub url: String,
    pub database: String,
    pub user: String,
    pub password: String,
    /// For HTTPS: the certificates the server may present (pinned), when it has no public one.
    pub ca_pem: Option<Vec<u8>>,
    /// Compress batches (worth it across regions, where the bytes are paid for).
    pub gzip: bool,
    pub spill_dir: Option<PathBuf>,
    pub spill_limit: u64,
    /// This router, on every row.
    pub router: String,
    pub router_region: String,
}

type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

pub struct Sink {
    buffers: [Mutex<Vec<u8>>; 3],
    wake: Notify,
    /// `"router_region":"…","router":"…",` for every row's start.
    router_fields: String,
    pub stats: Stats,
    /// Bytes of spend rows handed over (`usage_backlog`) and not sent yet: at or below zero, every
    /// one is in ClickHouse (`budgets.rs` then stops counting them itself).
    usage_unsent: std::sync::atomic::AtomicI64,
}

#[derive(Default)]
pub struct Stats {
    pub rows: AtomicU64,
    pub sent_batches: AtomicU64,
    pub sent_bytes: AtomicU64,
    pub failed_sends: AtomicU64,
    pub spilled_batches: AtomicU64,
    pub dropped_batches: AtomicU64,
    pub dropped_bytes: AtomicU64,
    /// Rows that found the buffer full: the sender had stopped collecting.
    pub dropped_rows: AtomicU64,
}

static SINK: OnceLock<Arc<Sink>> = OnceLock::new();
/// The same ClickHouse, for reads (the spend ledger's month to date: `budgets.rs`).
static TARGET: OnceLock<Arc<Target>> = OnceLock::new();

/// Insert `rows` (JSONEachRow) into `table` now, waiting for ClickHouse's answer. None without a
/// sink.
pub async fn insert(table: Table, rows: Bytes) -> Option<Result<(), String>> {
    let target = TARGET.get()?;
    Some(target.clone().send(table, rows).await.map_err(|e| match e {
        SendError::Refused(why) | SendError::Unreachable(why) => why,
    }))
}

/// Run `sql` (a SELECT, with its FORMAT) on the sink's ClickHouse: its answer's text. None without
/// a sink.
pub async fn query(sql: &str) -> Option<Result<String, String>> {
    let target = TARGET.get()?;
    Some(target.query(sql).await)
}

/// The sink, if this router has one.
pub fn get() -> Option<&'static Arc<Sink>> {
    SINK.get()
}

/// Start the sink and its sender. The returned handle flushes on shutdown.
pub fn start(cfg: Config) -> Result<Flusher, BoxError> {
    let sink = Arc::new(Sink {
        buffers: [Mutex::new(Vec::with_capacity(1 << 20)), Mutex::new(Vec::with_capacity(1 << 20)), Mutex::new(Vec::with_capacity(64 << 10))],
        wake: Notify::new(),
        router_fields: format!("\"router_region\":{},\"router\":{},", json_string(&cfg.router_region), json_string(&cfg.router)),
        usage_unsent: Default::default(),
        stats: Stats::default(),
    });
    let sender = Sender::new(cfg)?;
    let _ = TARGET.set(sender.target.clone());
    info!(url = %sender.target.url, database = %sender.target.database, gzip = sender.target.gzip, spill = ?sender.spill_dir, "app logs go straight to ClickHouse");
    SINK.set(sink.clone()).map_err(|_| "the log sink was started twice")?;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(sender.run(sink, stop_rx));
    Ok(Flusher { stop: Some(stop_tx), task: Some(task) })
}

/// Held by `main`: `finish` sends what's buffered, or leaves it in the spill directory.
pub struct Flusher {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Flusher {
    pub async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

fn json_string(text: &str) -> String {
    serde_json::to_string(text).expect("strings serialize")
}

fn push_json(out: &mut Vec<u8>, text: &str) {
    serde_json::to_writer(&mut *out, text).expect("strings serialize");
}

/// Whether a time an app reports (a line's, a call's start) is within a day of when it arrived.
/// Rows are stored by month: times an app makes up across many months would fail the whole insert
/// they share with every other project's rows.
fn near(time: f64, received: f64) -> bool {
    time > 0.0 && (time - received).abs() < 86_400.0
}

fn now_seconds() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// The start of every row one worker's stream writes: who it is. Built once per stream.
pub struct RowPrefix(String);

/// An outgoing call, as it is stored.
pub struct Call<'a> {
    pub request: Option<&'a str>,
    /// Seconds since the epoch, when the call began.
    pub start: Option<f64>,
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub status: Option<u16>,
    pub ms: Option<f64>,
    pub bytes: Option<u64>,
    pub error: Option<&'a str>,
}

impl Sink {
    pub fn prefix(&self, project: &str, deployment: &str, worker: &str) -> RowPrefix {
        RowPrefix(format!(
            "{{\"project\":{},\"deployment\":{},\"worker\":{},{}",
            json_string(project),
            json_string(deployment),
            json_string(worker),
            self.router_fields
        ))
    }

    /// One line of an app's output.
    pub fn app_line(&self, prefix: &RowPrefix, ts: f64, request: Option<&str>, stream: &str, line: &str) {
        let received = now_seconds();
        self.row(Table::AppLogs, line.len() + 256, |row| {
            row.extend_from_slice(prefix.0.as_bytes());
            let _ = write!(row, "\"time\":{:.6},\"received\":{received:.6},", if near(ts, received) { ts } else { received });
            if let Some(request) = request {
                row.extend_from_slice(b"\"request\":");
                push_json(row, request);
                row.push(b',');
            }
            row.extend_from_slice(b"\"stream\":");
            push_json(row, stream);
            row.extend_from_slice(b",\"line\":");
            push_json(row, line);
            row.extend_from_slice(b"}\n");
        });
    }

    /// One outgoing call an app made.
    pub fn outgoing(&self, prefix: &RowPrefix, call: &Call<'_>) {
        let received = now_seconds();
        self.row(Table::Outgoing, 512, |row| {
            row.extend_from_slice(prefix.0.as_bytes());
            let time = call.start.filter(|t| near(*t, received)).unwrap_or(received);
            let _ = write!(row, "\"time\":{time:.6},\"received\":{received:.6},");
            let mut text = |name: &str, value: Option<&str>| {
                if let Some(value) = value {
                    let _ = write!(row, "\"{name}\":");
                    push_json(row, value);
                    row.push(b',');
                }
            };
            text("request", call.request);
            text("method", Some(call.method));
            text("host", Some(call.host));
            text("path", Some(call.path));
            text("query", call.query);
            text("error", call.error);
            if let Some(status) = call.status {
                let _ = write!(row, "\"status\":{status},");
            }
            if let Some(bytes) = call.bytes {
                let _ = write!(row, "\"bytes\":{bytes},");
            }
            let _ = write!(row, "\"duration_ms\":{}}}\n", call.ms.filter(|ms| ms.is_finite()).unwrap_or(0.0));
        });
    }

    /// Spend rows (`usage_line`s) ClickHouse didn't take when the ledger sent them itself: sent
    /// with the rest, kept on disk while it's away.
    pub fn usage_backlog(&self, rows: &[u8]) {
        self.usage_unsent.fetch_add(rows.len() as i64, Ordering::Relaxed);
        self.row(Table::Usage, rows.len(), |r| r.extend_from_slice(rows));
    }

    /// Whether every spend row handed over is in ClickHouse.
    pub fn usage_drained(&self) -> bool {
        self.usage_unsent.load(Ordering::Relaxed) <= 0
    }

    /// One of the spend ledger's counts as a row, as of `time` (Unix seconds).
    pub fn usage_line(&self, time: u64, row: &crate::spend::Row) -> Vec<u8> {
        let mut r = Vec::with_capacity(256);
        self.usage_into(&mut r, time, row);
        r
    }

    /// One of the spend ledger's counts, as of `time` (Unix seconds), into the buffer.
    pub fn usage(&self, time: u64, row: &crate::spend::Row) {
        let line = self.usage_line(time, row);
        self.usage_backlog(&line);
    }

    fn usage_into(&self, r: &mut Vec<u8>, time: u64, row: &crate::spend::Row) {
        r.extend_from_slice(b"{\"project\":");
        push_json(r, &row.project);
        r.extend_from_slice(b",\"deployment\":");
        push_json(r, &row.deployment);
        r.extend_from_slice(b",\"region\":");
        push_json(r, &row.region);
        r.push(b',');
        r.extend_from_slice(self.router_fields.as_bytes());
        let zone = row.zone.map_or("", |z| z.name());
        let _ = write!(r, "\"time\":{time},\"meter\":\"{}\",\"zone\":\"{zone}\",\"quantity\":{},\"cost_n\":{}}}\n", row.meter.name(), row.quantity, row.cost);
    }

    fn row(&self, table: Table, room: usize, write: impl FnOnce(&mut Vec<u8>)) {
        let full = {
            let mut buffer = self.buffers[table as usize].lock().unwrap();
            // The sender empties this every tick; only a stalled sender lets it grow so far.
            if buffer.len() + room > MEMORY_LIMIT {
                drop(buffer);
                self.stats.dropped_rows.fetch_add(1, Ordering::Relaxed);
                return;
            }
            buffer.reserve(room);
            write(&mut buffer);
            buffer.len() >= FLUSH_BYTES
        };
        self.stats.rows.fetch_add(1, Ordering::Relaxed);
        if full {
            self.wake.notify_one();
        }
    }

    fn take(&self, table: Table) -> Vec<u8> {
        let mut buffer = self.buffers[table as usize].lock().unwrap();
        let capacity = buffer.capacity().min(FLUSH_BYTES);
        std::mem::replace(&mut *buffer, Vec::with_capacity(capacity))
    }
}

/// A batch waiting to be sent: in memory, or in a file of the spill directory.
enum Batch {
    Memory { body: Bytes },
    File { path: PathBuf, bytes: u64 },
}

/// Where batches go. Shared with each send, so the sender can keep collecting while one is out.
struct Target {
    client: HttpClient,
    url: String,
    database: String,
    user: String,
    password: String,
    gzip: bool,
}

struct Sender {
    target: Arc<Target>,
    spill_dir: Option<PathBuf>,
    spill_limit: u64,
    /// What waits to be sent, oldest first, for each table.
    queues: [VecDeque<Batch>; 3],
    /// The table whose turn it is.
    turn: usize,
    in_memory: usize,
    on_disk: u64,
    seq: u64,
    /// How often ClickHouse has refused the batch at the front.
    refusals: u32,
    failing_since: Option<Instant>,
    warned: Option<Instant>,
    /// After a failure, nothing is sent before this.
    retry_at: Option<Instant>,
}

/// Accepts exactly the certificates of `FORKRAIL_CLICKHOUSE_CA`: the analytics box's is self-signed
/// (`openssl req -x509`), which a chain check refuses as a server's own certificate, and it is
/// reached by address, so there is no name to check either. Pinning is stricter than both.
#[derive(Debug)]
struct Pinned {
    pinned: Vec<CertificateDer<'static>>,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self.pinned.iter().any(|pinned| pinned.as_ref() == end_entity.as_ref()) {
            true => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            false => Err(rustls::Error::General("the server's certificate isn't the pinned one".into())),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// An error with what caused it: hyper's own text is only "client error (Connect)".
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Why a batch wasn't taken.
enum SendError {
    /// ClickHouse answered and said no: sending it again won't help.
    Refused(String),
    /// No answer, or a server error: try again.
    Unreachable(String),
}

impl Target {
    async fn query(&self, sql: &str) -> Result<String, String> {
        let uri = format!("{}/?database={}", self.url, self.database);
        let request = Request::post(uri)
            .header("x-clickhouse-user", &self.user)
            .header("x-clickhouse-key", &self.password)
            .body(Full::new(Bytes::from(sql.to_owned())))
            .map_err(|e| e.to_string())?;
        let answer = async {
            let response = self.client.request(request).await.map_err(|e| chain(&e))?;
            let status = response.status();
            let body = response.into_body().collect().await.map(|b| b.to_bytes()).map_err(|e| e.to_string())?;
            Ok::<_, String>((status, body))
        };
        let (status, body) = tokio::time::timeout(SEND_TIMEOUT, answer).await.map_err(|_| "timed out".to_owned())??;
        let text = String::from_utf8_lossy(&body).into_owned();
        match status.is_success() {
            true => Ok(text),
            false => Err(format!("{status}: {}", text.chars().take(300).collect::<String>().trim())),
        }
    }

    async fn send(self: Arc<Self>, table: Table, rows: Bytes) -> Result<(), SendError> {
        let uri = format!(
            "{}/?database={}&query=INSERT%20INTO%20{}%20FORMAT%20JSONEachRow&input_format_skip_unknown_fields=1&date_time_input_format=best_effort",
            self.url,
            self.database,
            table.name()
        );
        let mut request = Request::post(uri).header("x-clickhouse-user", &self.user).header("x-clickhouse-key", &self.password);
        let body = if self.gzip {
            request = request.header("content-encoding", "gzip");
            let mut encoder = flate2::write::GzEncoder::new(Vec::with_capacity(rows.len() / 6), flate2::Compression::fast());
            encoder.write_all(&rows).and_then(|()| encoder.finish()).map(Bytes::from).map_err(|e| SendError::Unreachable(e.to_string()))?
        } else {
            rows
        };
        let request = request.body(Full::new(body)).map_err(|e| SendError::Refused(e.to_string()))?;
        let answer = async {
            let response = self.client.request(request).await.map_err(|e| SendError::Unreachable(chain(&e)))?;
            let status = response.status();
            let said = response.into_body().collect().await.map(|body| body.to_bytes()).unwrap_or_default();
            Ok::<_, SendError>((status, said))
        };
        let (status, said) = match tokio::time::timeout(SEND_TIMEOUT, answer).await {
            Ok(answer) => answer?,
            Err(_) => return Err(SendError::Unreachable("timed out".into())),
        };
        if status.is_success() {
            return Ok(());
        }
        let said: String = String::from_utf8_lossy(&said).chars().take(300).collect();
        let why = format!("{status}: {}", said.trim());
        // 401 and 403 are a setup fault that someone will fix: keep the rows.
        match status.is_client_error() && !matches!(status.as_u16(), 401 | 403 | 408 | 429) {
            true => Err(SendError::Refused(why)),
            false => Err(SendError::Unreachable(why)),
        }
    }
}

impl Sender {
    fn new(cfg: Config) -> Result<Self, BoxError> {
        let builder = hyper_rustls::HttpsConnectorBuilder::new();
        let builder = match &cfg.ca_pem {
            Some(pem) => {
                let pinned: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem).collect::<Result<_, _>>()?;
                if pinned.is_empty() {
                    return Err("FORKRAIL_CLICKHOUSE_CA has no certificate".into());
                }
                let verifier = Arc::new(Pinned { pinned, algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms });
                builder.with_tls_config(rustls::ClientConfig::builder().dangerous().with_custom_certificate_verifier(verifier).with_no_client_auth())
            }
            None => builder.with_provider_and_platform_verifier(rustls::crypto::ring::default_provider())?,
        };
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(CONNECT_TIMEOUT));
        http.set_nodelay(true);
        let connector = builder.https_or_http().enable_http1().wrap_connector(http);
        let client = Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(50)).build(connector);
        if let Some(dir) = &cfg.spill_dir {
            std::fs::create_dir_all(dir)?;
        }
        Ok(Self {
            target: Arc::new(Target {
                client,
                url: cfg.url.trim_end_matches('/').to_owned(),
                database: cfg.database,
                user: cfg.user,
                password: cfg.password,
                gzip: cfg.gzip,
            }),
            spill_dir: cfg.spill_dir,
            spill_limit: cfg.spill_limit,
            queues: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
            turn: 0,
            in_memory: 0,
            on_disk: 0,
            seq: 0,
            refusals: 0,
            failing_since: None,
            warned: None,
            retry_at: None,
        })
    }

    /// Collect the buffers once a tick (or when one is full), whatever a send is doing, and send
    /// what waits, a table at a time: everything queued for it in one INSERT, up to `FLUSH_BYTES`.
    /// So a table gets one INSERT a second however its rows trickle in, and a backlog drains in
    /// large batches. (Collecting after every send, as this first did, made a batch per send for
    /// each table and sent one: with rows always arriving for both it fell behind without end.)
    /// On `stop`: one last try within `STOP_TIMEOUT`, then the rest goes to the spill directory.
    async fn run(mut self, sink: Arc<Sink>, mut stop: tokio::sync::oneshot::Receiver<()>) {
        self.adopt(&sink);
        let mut tick = tokio::time::interval(FLUSH_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut adopted = Instant::now();
        'running: loop {
            if adopted.elapsed() >= ADOPT_EVERY {
                adopted = Instant::now();
                self.adopt(&sink);
            }
            let ready = self.retry_at.is_none_or(|at| Instant::now() >= at);
            let Some((table, body, batches)) = (if ready { self.front().await } else { None }) else {
                tokio::select! {
                    biased;
                    _ = &mut stop => break 'running,
                    _ = tick.tick() => self.collect(&sink),
                    _ = sink.wake.notified() => self.collect(&sink),
                }
                continue;
            };
            let send = self.target.clone().send(table, body.clone());
            tokio::pin!(send);
            let result = loop {
                tokio::select! {
                    biased;
                    _ = &mut stop => break 'running,
                    result = &mut send => break result,
                    _ = tick.tick() => self.collect(&sink),
                    _ = sink.wake.notified() => self.collect(&sink),
                }
            };
            self.sent(&sink, table, body, batches, result);
        }

        self.collect(&sink);
        let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
        while let Some((table, body, batches)) = self.front().await {
            match tokio::time::timeout_at(deadline, self.target.clone().send(table, body.clone())).await {
                Ok(Ok(())) => self.sent(&sink, table, body, batches, Ok(())),
                _ => break,
            }
        }
        // Whatever is left waits on disk for the next router, in as few files as it fits.
        let mut left = 0;
        for (table, _) in TABLES {
            let mut rows: Vec<u8> = Vec::new();
            for batch in std::mem::take(&mut self.queues[table as usize]) {
                left += 1;
                let Batch::Memory { body } = batch else { continue };
                self.in_memory -= body.len();
                if !rows.is_empty() && rows.len() + body.len() > FLUSH_BYTES {
                    self.spill(&sink, table, Bytes::from(std::mem::take(&mut rows)));
                }
                rows.extend_from_slice(&body);
            }
            if !rows.is_empty() {
                self.spill(&sink, table, Bytes::from(rows));
            }
        }
        if left > 0 {
            warn!(batches = left, "the log sink stopped with batches unsent; they wait in the spill directory");
        }
    }

    fn waiting(&self) -> usize {
        self.queues.iter().map(VecDeque::len).sum()
    }

    /// The buffers into the queue: in memory up to `MEMORY_LIMIT`, then in files.
    fn collect(&mut self, sink: &Sink) {
        for (table, _) in TABLES {
            let rows = sink.take(table);
            if rows.is_empty() {
                continue;
            }
            let body = Bytes::from(rows);
            if self.in_memory + body.len() > MEMORY_LIMIT {
                self.spill(sink, table, body);
            } else {
                self.in_memory += body.len();
                self.queues[table as usize].push_back(Batch::Memory { body });
            }
        }
    }

    /// What to send next: the table whose turn it is (or the other, if it has nothing), and its
    /// oldest batches together, as many as fit in `FLUSH_BYTES`; how many that is comes back too.
    /// Rows end in a newline, so batches join as they are. After a refusal only the oldest goes,
    /// so one bad batch is set aside alone.
    async fn front(&mut self) -> Option<(Table, Bytes, usize)> {
        loop {
            let index = (0..TABLES.len()).map(|i| (self.turn + i) % TABLES.len()).find(|i| !self.queues[*i].is_empty())?;
            let table = TABLES[index].0;
            let one = self.refusals > 0;
            if let Some(Batch::Memory { body }) = self.queues[index].front()
                && (one || self.queues[index].len() == 1)
            {
                return Some((table, body.clone(), 1));
            }
            let (mut rows, mut batches, mut unreadable) = (Vec::new(), 0, None);
            for (at, batch) in self.queues[index].iter().enumerate() {
                let size = match batch {
                    Batch::Memory { body } => body.len(),
                    Batch::File { bytes, .. } => *bytes as usize,
                };
                if batches > 0 && (one || rows.len() + size > FLUSH_BYTES) {
                    break;
                }
                match batch {
                    Batch::Memory { body } => rows.extend_from_slice(body),
                    Batch::File { path, .. } => match tokio::fs::read(path).await {
                        Ok(read) => rows.extend_from_slice(&read),
                        Err(e) => {
                            warn!(path = %path.display(), error = %e, "a spilled batch can't be read; skipping it");
                            unreadable = Some(at);
                            break;
                        }
                    },
                }
                batches += 1;
            }
            if let Some(at) = unreadable {
                if let Some(Batch::File { bytes, .. }) = self.queues[index].remove(at) {
                    self.on_disk = self.on_disk.saturating_sub(bytes);
                }
                continue;
            }
            return Some((table, Bytes::from(rows), batches));
        }
    }

    /// What became of the table's `batches` oldest batches, sent together as `body`.
    fn sent(&mut self, sink: &Sink, table: Table, body: Bytes, batches: usize, result: Result<(), SendError>) {
        let error = match result {
            Ok(()) => {
                sink.stats.sent_batches.fetch_add(1, Ordering::Relaxed);
                sink.stats.sent_bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
                if table == Table::Usage {
                    sink.usage_unsent.fetch_sub(body.len() as i64, Ordering::Relaxed);
                }
                if let Some(since) = self.failing_since.take() {
                    info!(after_s = since.elapsed().as_secs(), waiting = self.waiting() - batches, "ClickHouse takes app logs again");
                }
                self.refusals = 0;
                self.retry_at = None;
                self.warned = None;
                // The other table goes next, so neither waits behind a backlog of the other's.
                self.turn = (table as usize + 1) % TABLES.len();
                return self.pop(table, batches);
            }
            Err(error) => error,
        };
        sink.stats.failed_sends.fetch_add(1, Ordering::Relaxed);
        let (refused, why) = match error {
            SendError::Refused(why) => (true, why),
            SendError::Unreachable(why) => (false, why),
        };
        self.failing_since.get_or_insert_with(Instant::now);
        self.retry_at = Some(Instant::now() + RETRY_AFTER);
        if self.warned.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
            self.warned = Some(Instant::now());
            warn!(table = table.name(), refused, error = %why, waiting = self.waiting(), in_memory = self.in_memory, on_disk = self.on_disk, "ClickHouse didn't take a batch of app logs");
        }
        if refused {
            self.refusals += 1;
            if self.refusals >= REFUSALS {
                self.set_aside(sink, table, body);
                self.refusals = 0;
                self.retry_at = None;
                self.pop(table, batches);
            }
        }
    }

    /// Drop the table's `batches` oldest batches, and their files.
    fn pop(&mut self, table: Table, batches: usize) {
        for _ in 0..batches {
            match self.queues[table as usize].pop_front() {
                Some(Batch::Memory { body }) => self.in_memory -= body.len(),
                Some(Batch::File { path, bytes, .. }) => {
                    self.on_disk = self.on_disk.saturating_sub(bytes);
                    let _ = std::fs::remove_file(path);
                }
                None => {}
            }
        }
    }

    /// A batch to the spill directory, at the back of the queue. Dropped, and counted, when there
    /// is no directory or it is full.
    fn spill(&mut self, sink: &Sink, table: Table, body: Bytes) {
        let Some(dir) = &self.spill_dir else {
            return self.dropped(sink, body.len(), "no spill directory");
        };
        if self.on_disk + body.len() as u64 > self.spill_limit {
            return self.dropped(sink, body.len(), "the spill directory is full");
        }
        self.seq += 1;
        let path = dir.join(format!("{}.{}.{:08}.rows", table.name(), std::process::id(), self.seq));
        match std::fs::write(&path, &body) {
            Ok(()) => {
                sink.stats.spilled_batches.fetch_add(1, Ordering::Relaxed);
                self.on_disk += body.len() as u64;
                self.queues[table as usize].push_back(Batch::File { path, bytes: body.len() as u64 });
            }
            Err(e) => self.dropped(sink, body.len(), &e.to_string()),
        }
    }

    fn set_aside(&mut self, sink: &Sink, table: Table, body: Bytes) {
        let kept = self.spill_dir.as_ref().and_then(|dir| {
            self.seq += 1;
            let path = dir.join(format!("bad.{}.{}.{:08}.rows", table.name(), std::process::id(), self.seq));
            std::fs::write(&path, &body).ok().map(|()| path)
        });
        match kept {
            Some(path) => warn!(table = table.name(), bytes = body.len(), path = %path.display(), "ClickHouse keeps refusing a batch of app logs; set aside"),
            None => self.dropped(sink, body.len(), "ClickHouse keeps refusing it"),
        }
    }

    fn dropped(&mut self, sink: &Sink, bytes: usize, why: &str) {
        sink.stats.dropped_batches.fetch_add(1, Ordering::Relaxed);
        sink.stats.dropped_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        warn!(bytes, why, "a batch of app logs was dropped");
    }

    /// Take over the batches a stopped router left in the spill directory: files named for a
    /// process that is gone are renamed to this one's and queued.
    fn adopt(&mut self, sink: &Sink) {
        let Some(dir) = self.spill_dir.clone() else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let me = std::process::id();
        let mut found: Vec<(String, Table)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut parts = name.split('.');
            let (Some(table), Some(pid), Some(_seq), Some("rows"), None) = (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()) else { continue };
            let (Some(table), Ok(pid)) = (Table::named(table), pid.parse::<u32>()) else { continue };
            if pid != me && !alive(pid) {
                found.push((name, table));
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        let mut adopted = 0;
        for (name, table) in found {
            self.seq += 1;
            let path = dir.join(format!("{}.{}.{:08}.rows", table.name(), me, self.seq));
            // Another router may be doing the same: whoever renames it has it.
            if std::fs::rename(dir.join(&name), &path).is_err() {
                continue;
            }
            let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
            self.on_disk += bytes;
            self.queues[table as usize].push_back(Batch::File { path, bytes });
            adopted += 1;
        }
        if adopted > 0 {
            info!(batches = adopted, "took over spilled app logs from a stopped router");
            sink.wake.notify_one();
        }
    }
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> Sink {
        Sink {
            buffers: [Mutex::new(Vec::new()), Mutex::new(Vec::new()), Mutex::new(Vec::new())],
            wake: Notify::new(),
            router_fields: "\"router_region\":\"us-east-1\",\"router\":\"i-1\",".into(),
            usage_unsent: Default::default(),
            stats: Stats::default(),
        }
    }

    fn sender(name: &str) -> Sender {
        let dir = std::env::temp_dir().join(format!("forkrail-sink-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Sender::new(Config {
            url: "http://127.0.0.1:9".into(),
            database: "forkrail".into(),
            user: "ingest".into(),
            password: String::new(),
            ca_pem: None,
            gzip: false,
            spill_dir: Some(dir),
            spill_limit: 1 << 20,
            router: "i-1".into(),
            router_region: "us-east-1".into(),
        })
        .unwrap()
    }

    fn queue(sender: &mut Sender, table: Table, rows: &'static str) {
        sender.in_memory += rows.len();
        sender.queues[table as usize].push_back(Batch::Memory { body: Bytes::from_static(rows.as_bytes()) });
    }

    /// Rows that trickle into both tables wait as many small batches; each table's go in one INSERT.
    #[tokio::test]
    async fn a_tables_batches_go_together() {
        let (sink, mut sender) = (sink(), sender("together"));
        for n in ["a1\n", "a2\n", "a3\n"] {
            queue(&mut sender, Table::AppLogs, n);
        }
        queue(&mut sender, Table::Outgoing, "o1\n");
        sender.spill(&sink, Table::Outgoing, Bytes::from_static(b"o2\n"));
        queue(&mut sender, Table::Outgoing, "o3\n");

        let (table, body, batches) = sender.front().await.unwrap();
        assert_eq!((table.name(), &body[..], batches), ("app_logs", &b"a1\na2\na3\n"[..], 3));
        sender.sent(&sink, table, body, batches, Ok(()));
        // The other table's turn, its file read in its place.
        let (table, body, batches) = sender.front().await.unwrap();
        assert_eq!((table.name(), &body[..], batches), ("outgoing", &b"o1\no2\no3\n"[..], 3));
        let spilled: Vec<_> = std::fs::read_dir(sender.spill_dir.as_ref().unwrap()).unwrap().flatten().collect();
        assert_eq!(spilled.len(), 1);
        sender.sent(&sink, table, body, batches, Ok(()));
        assert!(sender.front().await.is_none());
        assert_eq!((sender.in_memory, sender.on_disk, sender.waiting()), (0, 0, 0));
        assert_eq!(std::fs::read_dir(sender.spill_dir.as_ref().unwrap()).unwrap().count(), 0);
    }

    /// A refused INSERT is tried again with its oldest batch alone, so only that one is set aside.
    #[tokio::test]
    async fn a_refused_batch_is_set_aside_alone() {
        let (sink, mut sender) = (sink(), sender("refused"));
        for n in ["bad\n", "good\n"] {
            queue(&mut sender, Table::AppLogs, n);
        }
        let (table, body, batches) = sender.front().await.unwrap();
        assert_eq!(batches, 2);
        sender.sent(&sink, table, body, batches, Err(SendError::Refused("400".into())));
        for _ in 1..REFUSALS {
            sender.retry_at = None;
            let (table, body, batches) = sender.front().await.unwrap();
            assert_eq!((&body[..], batches), (&b"bad\n"[..], 1));
            sender.sent(&sink, table, body, batches, Err(SendError::Refused("400".into())));
        }
        let (_, body, batches) = sender.front().await.unwrap();
        assert_eq!((&body[..], batches), (&b"good\n"[..], 1));
        let kept: Vec<String> = std::fs::read_dir(sender.spill_dir.as_ref().unwrap()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(kept.len() == 1 && kept[0].starts_with("bad.app_logs."), "{kept:?}");
    }

    /// Batches over `FLUSH_BYTES` together go in more than one INSERT.
    #[tokio::test]
    async fn an_insert_stops_at_the_flush_size() {
        let mut sender = sender("size");
        let big: &'static str = Box::leak(format!("{}\n", "x".repeat(FLUSH_BYTES * 2 / 3)).into_boxed_str());
        for _ in 0..3 {
            queue(&mut sender, Table::AppLogs, big);
        }
        let (_, body, batches) = sender.front().await.unwrap();
        assert_eq!((body.len(), batches), (big.len(), 1));
    }

    fn rows(sink: &Sink, table: Table) -> Vec<serde_json::Value> {
        let taken = sink.take(table);
        std::str::from_utf8(&taken).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn spend_counts_are_rows_clickhouse_reads() {
        use forkrail_common::prices::{Meter, Zone};
        let sink = sink();
        let row = crate::spend::Row {
            project: "orbit".into(),
            deployment: "8a146169".into(),
            region: "us-east-1".into(),
            meter: Meter::EgressBytes,
            zone: Some(Zone::EU),
            quantity: 1_000_000,
            cost: 98_000,
        };
        sink.usage(1_795_132_800, &row);
        sink.usage(1_795_132_800, &crate::spend::Row { project: "".into(), deployment: "".into(), zone: None, meter: Meter::S3Get, quantity: 3, cost: 2, ..row });
        let got = rows(&sink, Table::Usage);
        assert_eq!(got.len(), 2);
        assert_eq!((&got[0]["project"], &got[0]["meter"], &got[0]["zone"]), (&"orbit".into(), &"egress_bytes".into(), &"EU".into()));
        assert_eq!((got[0]["quantity"].as_u64(), got[0]["cost_n"].as_i64(), got[0]["time"].as_u64()), (Some(1_000_000), Some(98_000), Some(1_795_132_800)));
        assert_eq!((&got[0]["router"], &got[0]["router_region"], &got[0]["region"]), (&"i-1".into(), &"us-east-1".into(), &"us-east-1".into()));
        assert_eq!((&got[1]["project"], &got[1]["zone"], &got[1]["meter"]), (&"".into(), &"".into(), &"s3_get".into()));
    }

    #[test]
    fn app_lines_are_rows_clickhouse_reads() {
        let sink = sink();
        let prefix = sink.prefix("orbit", "8a146169", "8a146169.w\"1");
        let ts = (now_seconds() - 1.0).floor() + 0.123456;
        sink.app_line(&prefix, ts, Some("r1"), "stdout", "a \"quoted\"\tline\nwith a newline");
        sink.app_line(&prefix, 0.0, None, "stderr", "no time, no request");
        // A time made up far from now (a month apart, and so a partition apart) is the router's.
        sink.app_line(&prefix, 86_400.0 * 400.0, None, "stdout", "from 1971");
        let got = rows(&sink, Table::AppLogs);
        assert_eq!(got.len(), 3, "one row a line, however the text is escaped");
        assert_eq!(got[2]["time"], got[2]["received"]);
        assert_eq!(got[0]["project"], "orbit");
        assert_eq!(got[0]["worker"], "8a146169.w\"1");
        assert_eq!(got[0]["router"], "i-1");
        assert_eq!(got[0]["request"], "r1");
        assert_eq!(got[0]["line"], "a \"quoted\"\tline\nwith a newline");
        assert!((got[0]["time"].as_f64().unwrap() - ts).abs() < 1e-5);
        assert!(got[1].get("request").is_none());
        assert_eq!(got[1]["time"], got[1]["received"], "a line without a time has the router's");
        assert!(rows(&sink, Table::AppLogs).is_empty(), "taking empties the buffer");
    }

    #[test]
    fn outgoing_calls_are_rows() {
        let sink = sink();
        let prefix = sink.prefix("orbit", "d", "w");
        let call = Call { request: Some("r1"), start: Some(1_794_300_000.5), method: "GET", host: "api.example.org", path: "/v1/search", query: Some("al=1"), status: Some(200), ms: Some(12.5), bytes: Some(3_000), error: None };
        sink.outgoing(&prefix, &call);
        sink.outgoing(&prefix, &Call { request: None, start: None, query: None, status: None, ms: None, bytes: None, error: Some("timed out"), ..call });
        let got = rows(&sink, Table::Outgoing);
        assert_eq!((got[0]["host"].as_str(), got[0]["status"].as_u64(), got[0]["duration_ms"].as_f64(), got[0]["bytes"].as_u64()), (Some("api.example.org"), Some(200), Some(12.5), Some(3_000)));
        assert_eq!(got[0]["query"], "al=1");
        assert!(got[1].get("status").is_none() && got[1].get("bytes").is_none() && got[1].get("request").is_none());
        assert_eq!((got[1]["error"].as_str(), got[1]["duration_ms"].as_f64()), (Some("timed out"), Some(0.0)));
        assert!(rows(&sink, Table::AppLogs).is_empty());
    }
}
