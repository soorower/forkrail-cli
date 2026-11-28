//! A deployment's build log as it's written (track.rs starts one). Each line goes to the terminal,
//! and, with env var values hidden (`buildlog::Mask`), to the install: every half second to the
//! dashboard's builds endpoint (`POST <dashboard>/api/builds/<id>/log`, with the deployment's key),
//! which writes them into ClickHouse for the dashboard to show live, and as one file in S3 when the
//! deployment ends, which covers any batch that never arrived.
//!
//! Sending never holds the build up: a batch that fails is sent again with the next, and when the
//! deployment ends whatever hasn't gone after a few seconds is left to the S3 copy.
//!
//! Inside `scope`, `deploy::log` (and so every line publishing writes) goes to the current report.

use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::client::conn::http1::SendRequest;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST, USER_AGENT};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_rustls::TlsConnector;

use crate::buildlog::{BuildLog, Mask, Stream, print_line};
use crate::state::now_ms;

/// Vercel cuts build logs at 4 MB; so does forkrail, keeping the first 4 MB.
pub const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// A line longer than this (a minified bundle printed whole) is cut.
pub const MAX_LINE_BYTES: usize = 64 * 1024;
const BATCH_EVERY: Duration = Duration::from_millis(500);
const BATCH_MAX_LINES: usize = 2_000;
const BATCH_MAX_BYTES: usize = 512 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the end of a deployment waits for its last lines to be taken.
const FLUSH_WAIT: Duration = Duration::from_secs(10);

/// One line of the log, as ClickHouse and the S3 copy keep it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    /// From 1, in the order the lines were written.
    pub seq: u32,
    /// Unix milliseconds.
    pub t: u64,
    pub stream: String,
    pub text: String,
}

/// Where lines go as they're written: the deployment's endpoint and the key it accepts.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub url: String,
    pub key: String,
}

/// How the terminal shows lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Echo {
    /// As written: the CLI, whose user may see their own values.
    Raw,
    /// With values hidden: the control function, whose output is its CloudWatch log.
    Masked,
}

pub struct Report {
    echo: Echo,
    mask: Mask,
    state: Mutex<State>,
    sender: Mutex<Option<JoinHandle<Delivery>>>,
}

struct State {
    seq: u32,
    bytes: u64,
    cut: bool,
    step: Option<String>,
    lines: Vec<Line>,
    /// Lines on their way to the sender; gone once the report closes.
    tx: Option<mpsc::UnboundedSender<Line>>,
}

/// What the sender managed.
#[derive(Debug, Clone, Default)]
pub struct Delivery {
    /// The highest line the endpoint took.
    pub through: u32,
    /// Why it stopped sending, if it did.
    pub refused: Option<String>,
}

/// The log once closed: every line kept, and what reached the endpoint.
pub struct Closed {
    pub lines: Vec<Line>,
    pub bytes: u64,
    pub cut: bool,
    pub delivered: Delivery,
}

impl Report {
    /// A report that sends to `endpoint`, if there is one, from now on.
    pub fn start(endpoint: Option<Endpoint>, mask: Mask, echo: Echo) -> Arc<Self> {
        Self::start_after(endpoint, mask, echo, 0, 0)
    }

    /// A report that goes on from a log someone else wrote `seq` lines and `bytes` of: the control
    /// function's publishing lines after the builder's.
    pub fn start_after(endpoint: Option<Endpoint>, mask: Mask, echo: Echo, seq: u32, bytes: u64) -> Arc<Self> {
        let (tx, sender) = match endpoint {
            Some(endpoint) => {
                let (tx, rx) = mpsc::unbounded_channel();
                (Some(tx), Some(tokio::spawn(send(endpoint, rx))))
            }
            None => (None, None),
        };
        Arc::new(Report {
            echo,
            mask,
            state: Mutex::new(State { seq, bytes, cut: false, step: None, lines: Vec::new(), tx }),
            sender: Mutex::new(sender),
        })
    }

    /// `text` with the env var values hidden.
    pub fn mask(&self, text: &str) -> String {
        self.mask.apply(text)
    }

    /// The step the log is in.
    pub fn step_now(&self) -> Option<String> {
        self.state.lock().expect("report lock").step.clone()
    }

    /// Stop taking lines, and wait (a little) for the sender to deliver what it has.
    pub async fn close(&self) -> Closed {
        let (lines, bytes, cut) = {
            let mut state = self.state.lock().expect("report lock");
            state.tx = None;
            (state.lines.clone(), state.bytes, state.cut)
        };
        let sender = self.sender.lock().expect("report lock").take();
        let delivered = match sender {
            Some(mut task) => match tokio::time::timeout(FLUSH_WAIT, &mut task).await {
                Ok(Ok(delivery)) => delivery,
                Ok(Err(_)) => Delivery { refused: Some("the sender stopped".into()), ..Default::default() },
                Err(_) => {
                    task.abort();
                    eprintln!("note: some of the build log's lines didn't reach the dashboard live; the copy kept in S3 has them all");
                    Delivery { refused: Some(format!("the dashboard hadn't taken every line after {} s", FLUSH_WAIT.as_secs())), ..Default::default() }
                }
            },
            None => Delivery::default(),
        };
        Closed { lines, bytes, cut, delivered }
    }
}

impl BuildLog for Report {
    fn line(&self, stream: Stream, text: &str) {
        let masked = self.mask.apply(text);
        match self.echo {
            Echo::Raw => print_line(stream, text),
            Echo::Masked => print_line(stream, &masked),
        }
        let mut state = self.state.lock().expect("report lock");
        // A step already begun isn't begun again: the builder starts Preparing before the build
        // it runs does.
        if state.cut || (stream == Stream::Step && state.step.as_deref() == Some(text)) {
            return;
        }
        let (stream, text) = if state.bytes + masked.len() as u64 > MAX_LOG_BYTES {
            state.cut = true;
            (Stream::Forkrail, "The build log passed 4 MB; the rest of it isn't kept, as on Vercel.".to_owned())
        } else {
            (stream, cut_line(masked))
        };
        if stream == Stream::Step {
            state.step = Some(text.clone());
        }
        state.seq += 1;
        state.bytes += text.len() as u64;
        let line = Line { seq: state.seq, t: now_ms(), stream: stream.as_str().to_owned(), text };
        // Sent under the lock, so the sender gets lines in their order even from two threads.
        if let Some(tx) = &state.tx {
            let _ = tx.send(line.clone());
        }
        state.lines.push(line);
    }
}

/// A line cut to `MAX_LINE_BYTES`, on a character boundary.
fn cut_line(mut text: String) -> String {
    if text.len() > MAX_LINE_BYTES {
        let mut end = MAX_LINE_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" [line cut at 64 KB]");
    }
    text
}

tokio::task_local! {
    static CURRENT: Arc<Report>;
}

/// Run `work` with `report` as the log `deploy::log` writes to.
pub async fn scope<F: Future>(report: Arc<Report>, work: F) -> F::Output {
    CURRENT.scope(report, work).await
}

/// Write a line to the current report, if there is one; false if there isn't.
pub fn emit(stream: Stream, text: &str) -> bool {
    CURRENT.try_with(|r| r.line(stream, text)).is_ok()
}

/// The log as one file: gzipped JSON lines, as the S3 copy holds it.
pub fn gzip_lines(lines: &[Line]) -> Result<Vec<u8>> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    for line in lines {
        serde_json::to_writer(&mut gz, line)?;
        gz.write_all(b"\n")?;
    }
    Ok(gz.finish()?)
}

/// The lines of a file `gzip_lines` wrote.
/// The most a kept build log unpacks to: far more than a build's log holds (the dashboard keeps
/// 4 MB of lines), and little enough for the control function's memory. The file is written by
/// the builder, where the build's own code runs.
const MAX_LOG_TEXT_BYTES: u64 = 64 << 20;

pub fn gunzip_lines(bytes: &[u8]) -> Result<Vec<Line>> {
    let mut text = String::new();
    let mut unpacked = std::io::Read::take(flate2::read::GzDecoder::new(bytes), MAX_LOG_TEXT_BYTES + 1);
    std::io::Read::read_to_string(&mut unpacked, &mut text).context("unzipping the build log")?;
    if text.len() as u64 > MAX_LOG_TEXT_BYTES {
        bail!("the build log unpacks to more than {} MB", MAX_LOG_TEXT_BYTES >> 20);
    }
    text.lines().filter(|l| !l.is_empty()).map(|l| serde_json::from_str(l).context("reading a build log line")).collect()
}

#[derive(Serialize)]
struct Batch<'a> {
    lines: &'a [Line],
}

/// Send lines as they come, in batches, until the channel closes and everything has gone, or the
/// endpoint refuses them for good.
async fn send(endpoint: Endpoint, mut rx: mpsc::UnboundedReceiver<Line>) -> Delivery {
    let mut delivery = Delivery::default();
    let mut client = match Poster::new(&endpoint.url) {
        Ok(client) => client,
        Err(e) => {
            delivery.refused = Some(format!("{e:#}"));
            warn_once(&delivery);
            while rx.recv().await.is_some() {}
            return delivery;
        }
    };
    let mut pending: Vec<Line> = Vec::new();
    let mut closed = false;
    let mut failures = 0u32;
    loop {
        // Gather for half a second, or until there's a full batch.
        let until = Instant::now() + BATCH_EVERY;
        while !closed && pending.len() < BATCH_MAX_LINES {
            match tokio::time::timeout_at(until, rx.recv()).await {
                Ok(Some(line)) => pending.push(line),
                Ok(None) => closed = true,
                Err(_) => break,
            }
        }
        if pending.is_empty() {
            if closed {
                return delivery;
            }
            continue;
        }
        let take = batch_len(&pending);
        let body = match serde_json::to_vec(&Batch { lines: &pending[..take] }) {
            Ok(body) => body,
            Err(_) => return delivery,
        };
        match client.post(body, &endpoint.key).await {
            Ok((status, _)) if (200..300).contains(&status) => {
                delivery.through = pending[take - 1].seq;
                pending.drain(..take);
                failures = 0;
            }
            // Refused for good: the key, the deployment or the request is wrong, or the build is
            // over. The S3 copy is all there will be.
            Ok((status, text)) if (400..500).contains(&status) && status != 408 && status != 429 => {
                delivery.refused = Some(format!("the dashboard answered {status}: {}", text.trim()));
                warn_once(&delivery);
                while rx.recv().await.is_some() {}
                return delivery;
            }
            // Down or busy: keep the lines for the next try, a little later each time.
            Ok(_) | Err(_) => {
                failures += 1;
                let wait = Duration::from_millis((250 * 2u64.pow(failures.min(5))).min(5_000));
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// How many of the pending lines go in one request.
fn batch_len(pending: &[Line]) -> usize {
    let mut bytes = 0;
    for (i, line) in pending.iter().enumerate().take(BATCH_MAX_LINES) {
        bytes += line.text.len() + 64;
        if bytes > BATCH_MAX_BYTES && i > 0 {
            return i;
        }
    }
    pending.len().min(BATCH_MAX_LINES)
}

fn warn_once(delivery: &Delivery) {
    if let Some(why) = &delivery.refused {
        eprintln!("warning: the build log isn't going to the dashboard live ({why}); it's kept when the deployment ends");
    }
}

/// POSTs to one URL over one kept-alive connection, opened again when it closes.
struct Poster {
    https: bool,
    host: String,
    port: u16,
    path: String,
    connector: Option<TlsConnector>,
    conn: Option<SendRequest<Full<Bytes>>>,
}

impl Poster {
    fn new(url: &str) -> Result<Self> {
        let uri: hyper::Uri = url.parse().with_context(|| format!("{url:?} isn't a URL"))?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => bail!("{url}: the dashboard's address must start with https:// (or http:// for a local one)"),
        };
        let host = uri.host().context("no host in the dashboard's address")?.to_owned();
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let path = uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
        let connector = if https {
            let mut roots = rustls::RootCertStore::empty();
            roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            if roots.is_empty() {
                bail!("no root certificates found on this machine");
            }
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Some(TlsConnector::from(Arc::new(config)))
        } else {
            None
        };
        Ok(Poster { https, host, port, path, connector, conn: None })
    }

    async fn post(&mut self, body: Vec<u8>, key: &str) -> Result<(u16, String)> {
        tokio::time::timeout(REQUEST_TIMEOUT, self.post_once(body, key)).await.context("the dashboard took too long")?
    }

    async fn post_once(&mut self, body: Vec<u8>, key: &str) -> Result<(u16, String)> {
        if self.conn.as_ref().is_none_or(|c| c.is_closed()) {
            self.conn = Some(self.open().await?);
        }
        let sender = self.conn.as_mut().expect("just opened");
        let authority = if (self.https && self.port == 443) || (!self.https && self.port == 80) { self.host.clone() } else { format!("{}:{}", self.host, self.port) };
        let request = Request::post(&self.path)
            .header(HOST, authority)
            .header(AUTHORIZATION, format!("Bearer {key}"))
            .header(CONTENT_TYPE, "application/json")
            .header(USER_AGENT, concat!("forkrail/", env!("CARGO_PKG_VERSION")))
            .body(Full::new(Bytes::from(body)))?;
        let sent = match sender.ready().await {
            Ok(()) => sender.send_request(request).await,
            Err(e) => Err(e),
        };
        let response = match sent {
            Ok(response) => response,
            Err(e) => {
                self.conn = None;
                return Err(e).context("sending to the dashboard");
            }
        };
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.context("reading the dashboard's answer")?.to_bytes();
        Ok((status, String::from_utf8_lossy(&body).chars().take(300).collect()))
    }

    async fn open(&self) -> Result<SendRequest<Full<Bytes>>> {
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((self.host.as_str(), self.port)))
            .await
            .context("connecting to the dashboard timed out")?
            .with_context(|| format!("connecting to {}:{}", self.host, self.port))?;
        tcp.set_nodelay(true)?;
        match &self.connector {
            Some(connector) => {
                let name = ServerName::try_from(self.host.clone()).context("the dashboard's host name")?;
                let tls = connector.connect(name, tcp).await.context("TLS with the dashboard")?;
                let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                Ok(sender)
            }
            None => {
                let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                Ok(sender)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use http_body_util::BodyExt;
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use tokio::net::TcpListener;

    use super::*;

    /// A stand-in for the dashboard's endpoint: keeps what it's sent, refusing the first `fail`
    /// requests with a 503.
    async fn endpoint(fail: usize) -> (String, Arc<Mutex<Vec<Line>>>, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/builds/a1b2c3d4/log", listener.local_addr().unwrap());
        let got: Arc<Mutex<Vec<Line>>> = Arc::default();
        let keys: Arc<Mutex<Vec<String>>> = Arc::default();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (g, k) = (got.clone(), keys.clone());
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let (g, k, calls) = (g.clone(), k.clone(), calls.clone());
                tokio::spawn(async move {
                    let service = service_fn(move |req: hyper::Request<Incoming>| {
                        let (g, k, calls) = (g.clone(), k.clone(), calls.clone());
                        async move {
                            k.lock().unwrap().push(req.headers()[AUTHORIZATION].to_str().unwrap().to_owned());
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let status = if n < fail { 503 } else { 200 };
                            if status == 200 {
                                #[derive(Deserialize)]
                                struct B {
                                    lines: Vec<Line>,
                                }
                                g.lock().unwrap().extend(serde_json::from_slice::<B>(&body).unwrap().lines);
                            }
                            Ok::<_, std::convert::Infallible>(hyper::Response::builder().status(status).body(Full::new(Bytes::new())).unwrap())
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tcp), service).await;
                });
            }
        });
        (url, got, keys)
    }

    #[tokio::test]
    async fn lines_reach_the_endpoint_masked_in_order_after_failures() {
        let (url, got, keys) = endpoint(2).await;
        let env: BTreeMap<String, String> = [("TOKEN".to_owned(), "secret-value-123".to_owned())].into();
        let report = Report::start(Some(Endpoint { url, key: "k".repeat(64) }), Mask::new(&env), Echo::Masked);
        report.step("Building");
        for i in 0..50 {
            report.line(Stream::Stdout, &format!("line {i} token=secret-value-123"));
        }
        let closed = report.close().await;
        assert_eq!(closed.lines.len(), 51);
        assert_eq!(closed.delivered.through, 51, "{:?}", closed.delivered);
        let got = got.lock().unwrap().clone();
        assert_eq!(got, closed.lines);
        assert!(got.iter().all(|l| !l.text.contains("secret-value")));
        assert_eq!(got[1].text, "line 0 token=[TOKEN hidden]");
        assert_eq!((got[0].stream.as_str(), got[0].text.as_str()), ("step", "Building"));
        assert!(keys.lock().unwrap().iter().all(|k| *k == format!("Bearer {}", "k".repeat(64))));
        assert_eq!(report.step_now().as_deref(), Some("Building"));
        // The S3 copy reads back as the same lines.
        assert_eq!(gunzip_lines(&gzip_lines(&closed.lines).unwrap()).unwrap(), closed.lines);
    }

    #[tokio::test]
    async fn the_log_stops_at_four_megabytes_and_long_lines_are_cut() {
        let report = Report::start(None, Mask::default(), Echo::Masked);
        let long = "x".repeat(MAX_LINE_BYTES + 10);
        report.line(Stream::Stdout, &long);
        let chunk = "y".repeat(60_000);
        for _ in 0..80 {
            report.line(Stream::Stdout, &chunk);
        }
        let closed = report.close().await;
        assert!(closed.cut);
        assert!(closed.bytes <= MAX_LOG_BYTES);
        assert!(closed.lines[0].text.ends_with("[line cut at 64 KB]"));
        assert!(closed.lines.last().unwrap().text.contains("passed 4 MB"));
    }

    #[tokio::test]
    async fn a_refusal_stops_sending_without_holding_the_build() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/builds/a1b2c3d4/log", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let service = service_fn(|_req: hyper::Request<Incoming>| async {
                Ok::<_, std::convert::Infallible>(hyper::Response::builder().status(401).body(Full::new(Bytes::from("wrong key"))).unwrap())
            });
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tcp), service).await;
        });
        let report = Report::start(Some(Endpoint { url, key: "k".into() }), Mask::default(), Echo::Masked);
        report.info("one");
        tokio::time::sleep(Duration::from_millis(800)).await;
        report.info("two");
        let started = std::time::Instant::now();
        let closed = report.close().await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(closed.lines.len(), 2);
        assert!(closed.delivered.refused.unwrap().contains("401"));
    }
}
