//! The user's app, in Node or Python, or a program of its own (a Go or Rust web server). One
//! process per execution environment, started at init, Node and Python with a shim for their
//! language. It keeps running across invocations (Lambda freezes it with the runtime in between),
//! and the runtime proxies each tunnel request to it over pooled HTTP/1.1 connections.
//!
//! Configuration (the function's environment variables):
//! - `FORKRAIL_APP_ENTRY`: the app's entry, relative to the app directory. Unset means the
//!   built-in echo app.
//!   - Node: the script that starts the server, e.g. `server.mjs`. The shim is preloaded.
//!   - Python: an ASGI or WSGI app's file and attribute, the way Vercel finds it: `api/main.py`
//!     for its `app`, or `api/main.py:application`. The shim serves it with uvicorn, which must be
//!     among the app's packages.
//!   - `.forkrail/functions.mjs` or `.forkrail/functions.py`: a Node or Python deployment of
//!     several functions (Vercel's `api/*.ts`, `api/*.py`), as a Node or Python app whose shim
//!     dispatches to them: it gets the router's `x-forkrail-function` header, which any other app
//!     never sees.
//!   - A `.json` file: a deployment of several functions (Vercel's `api/*.go` and `api/*.rs`
//!     files), as its manifest lists them (`Manifest`): programs, each serving some of the
//!     functions' paths, run side by side. A request goes to the one whose function the router's
//!     `x-forkrail-function` header names, else the one its path is; no function is 404.
//!   - Anything else (no `.py`, `.mjs`, `.js` or `.cjs`): an executable, run as it is, that serves
//!     HTTP on `127.0.0.1:$PORT` (or every address). It has no shim, so the runtime tells it's
//!     ready when the port takes a connection, and a line it prints is kept with the request it
//!     was serving when only one was in flight.
//! - `FORKRAIL_APP_DIR`: the app directory, default `/var/task`. It becomes the working
//!   directory, and for Python the first entry on the import path.
//! - `FORKRAIL_NODE`, `FORKRAIL_PYTHON`: the interpreter, default `node` or `python3` on the
//!   `PATH`. On Lambda's managed runtimes, which run this runtime as their exec wrapper, those are
//!   in `/var/lang/bin`.
//! - `PORT`: passed to the app. If unset, the runtime picks a free port. A Node app may listen
//!   somewhere else; the shim reports where.
//! - `FORKRAIL_EGRESS`: private targets the app reaches through the router; the app gets
//!   `FORKRAIL_EGRESS_<NAME>=127.0.0.1:<port>` for each (see `egress.rs`).

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use forkrail_common::{
    APP_ERROR_HEADER, FUNCTION_HEADER, LOCAL_PAYLOAD_ENV, RETRY_APP_CONNECTION, RETRY_APP_EXITED, RETRY_HEADER, strip_hop_by_hop,
};
use http_body_util::{BodyExt, Empty};
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::header::{EXPECT, HOST, HeaderValue};
use hyper::{Request, Response, StatusCode, Version};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use lambda_runtime::Error;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader, Lines};
use tokio::net::unix::pipe;
use tokio::process::Command;
use tokio::sync::{Mutex, watch};
use tracing::{info, warn};

use crate::logs::LineSplitter;
use crate::{env_id, logs, mem};

const NODE_SHIM: &str = include_str!("shim.mjs");
const PYTHON_SHIM: &str = include_str!("shim.py");
const REQUEST_ID: &str = "x-forkrail-request-id";
/// On a shimmed app's answers: the id of the request it answers (shim.mjs, shim.py). Checked and
/// taken off here.
const REPLY_HEADER: &str = "x-forkrail-reply-to";
/// The fd the shim writes its messages to.
const CONTROL_FD: i32 = 3;
/// The app must be listening within this long. Lambda allows 10 s for init.
const START_TIMEOUT: Duration = Duration::from_secs(9);
/// Pooled connections to the app close after this long idle: under Node's and uvicorn's default
/// keep-alive timeouts (5 s), in case the app sets one again after the shim raised it.
const POOL_IDLE: Duration = Duration::from_secs(4);
/// After a request to the app fails, how long to wait to see whether the app died, and then for
/// the shim's last messages.
const EXIT_GRACE: Duration = Duration::from_millis(100);
/// What the app's own HTTP server must take of a request's headers: more than the router lets in
/// from a client (its `MAX_REQUEST_HEAD`, about 128 KB) with the headers it adds.
const APP_MAX_HEADER_BYTES: usize = 192 << 10;
/// How many of the app's last output lines a startup error includes.
const TAIL_LINES: usize = 20;
/// V8's heap limit (old and young generations together) as a percentage of the function's
/// memory: 75% of the router's budget, which keeps a 10% margin. A runaway heap then fails
/// inside Node, before Lambda kills the whole environment. A heap flag of the app's own in
/// `NODE_OPTIONS` takes precedence.
const HEAP_PCT: u64 = 67;

pub type AppBody = BoxBody<Bytes, hyper::Error>;

/// The request the app's first uncaught error happened in, once the shim reports one:
/// `Some(None)` if it happened outside any request.
type Culprit = std::sync::Mutex<Option<Option<String>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    Node,
    Python,
    /// A program of its own, run directly: a Go or Rust web server.
    Binary,
    /// Several programs, by a manifest (`Manifest`).
    Functions,
}

/// A deployment of several functions in one: `.forkrail/functions.json` in its package, written by
/// the CLI (crates/cli/src/functions.rs).
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub programs: Vec<Program>,
}

#[derive(Debug, Deserialize)]
pub struct Program {
    /// The executable, relative to the app directory.
    pub program: String,
    /// Its functions: each the file it came from (`api/users/[id].rs`) and its URL paths.
    pub functions: Vec<ManifestFunction>,
    /// Env vars it needs besides the app's; `{port}` is replaced by the port it's given (Vercel's
    /// Rust runtime listens on `VERCEL_DEV_PORT`).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// It picks the function itself, from the `x-forkrail-function` header (Go's dispatcher):
    /// otherwise the header is taken off.
    #[serde(default)]
    pub dispatches: bool,
}

#[derive(Debug, Deserialize)]
pub struct ManifestFunction {
    pub file: String,
    pub paths: Vec<String>,
}

/// Which program serves a request.
struct Table {
    authorities: Vec<String>,
    dispatches: Vec<bool>,
    exact: std::collections::HashMap<String, usize>,
    /// Files with `[param]` segments, as segments (extension off), for paths the router didn't name.
    dynamic: Vec<(Vec<String>, usize)>,
}

impl Table {
    /// The program for a request, and for a path the routes didn't name that matched a
    /// `[param]` file, its parameters (the router's routes put them in the query).
    fn pick(&self, named: Option<&str>, path: &str) -> Option<(usize, Vec<(String, String)>)> {
        if let Some(i) = named.and_then(|n| self.exact.get(n)) {
            return Some((*i, Vec::new()));
        }
        if let Some(i) = self.exact.get(path) {
            return Some((*i, Vec::new()));
        }
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        let Some((pattern, i)) = self.dynamic.iter().find(|(pattern, _)| pattern.len() == segments.len() && pattern.iter().zip(&segments).all(|(p, s)| p.starts_with('[') || p == s)) else {
            // A router from before the header sends a rewritten request with its own path: with
            // one program, it can only be that one's.
            return (named.is_none() && self.authorities.len() == 1).then(|| (0, Vec::new()));
        };
        let params = pattern.iter().zip(&segments).filter_map(|(p, s)| Some((p.strip_prefix('[')?.strip_suffix(']')?.to_owned(), (*s).to_owned()))).collect();
        Some((*i, params))
    }
}

/// A function file's segments for matching, without its extension; an `index` file is its folder.
fn file_segments(file: &str) -> Vec<String> {
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let mut parts: Vec<String> = stem.split('/').map(str::to_owned).collect();
    if parts.len() > 1 && parts.last().is_some_and(|l| l == "index") {
        parts.pop();
    }
    parts
}

/// The entries of Node and Python deployments of several functions, whose shims pick the function
/// from the router's `x-forkrail-function` header (crates/cli/src/{nodefns,pyfns}.rs).
const DISPATCHERS: [&str; 2] = [".forkrail/functions.mjs", ".forkrail/functions.py"];

/// Entries the Node shim runs.
const NODE_EXTENSIONS: [&str; 6] = ["mjs", "js", "cjs", "ts", "mts", "cts"];
/// How often a program without a shim is asked whether it listens yet.
const PORT_POLL: Duration = Duration::from_millis(5);

pub struct Config {
    language: Language,
    interpreter: PathBuf,
    dir: PathBuf,
    entry: String,
}

impl Config {
    /// The app from the environment, or `None` for the built-in echo app.
    pub fn from_env() -> Option<Config> {
        let entry = std::env::var("FORKRAIL_APP_ENTRY").ok()?;
        let file = entry.split(':').next().unwrap_or_default();
        let dir: PathBuf = std::env::var_os("FORKRAIL_APP_DIR").map_or_else(|| "/var/task".into(), PathBuf::from);
        let extension = file.rsplit_once('.').map(|(_, ext)| ext).filter(|ext| !ext.contains('/'));
        let (language, interpreter) = match extension {
            Some("py") => (Language::Python, std::env::var_os("FORKRAIL_PYTHON").map_or_else(|| "python3".into(), PathBuf::from)),
            Some("json") => (Language::Functions, dir.join(&entry)),
            Some(ext) if NODE_EXTENSIONS.contains(&ext) => (Language::Node, std::env::var_os("FORKRAIL_NODE").map_or_else(|| "node".into(), PathBuf::from)),
            _ => (Language::Binary, dir.join(&entry)),
        };
        Some(Config { language, interpreter, dir, entry })
    }
}

/// Keeps one app process running, and starts a new one after it exits.
pub struct Supervisor {
    config: Config,
    current: Mutex<Option<Arc<UserApp>>>,
}

impl Supervisor {
    pub fn new(config: Config) -> Self {
        Self { config, current: Mutex::new(None) }
    }

    /// The running app, started first if it isn't running.
    pub async fn ready(&self) -> Result<Arc<UserApp>, Error> {
        let mut current = self.current.lock().await;
        if let Some(app) = current.as_ref().filter(|app| !*app.exited.borrow()) {
            return Ok(app.clone());
        }
        *current = None;
        let app = Arc::new(UserApp::start(&self.config).await?);
        *current = Some(app.clone());
        Ok(app)
    }
}

pub struct UserApp {
    /// Where the app listens, as `host:port`.
    authority: String,
    /// Pooled connections to the app. Replaced, with its pool, when an answer turns out to be
    /// another request's: the connection it came on is out of step, and so may be others.
    client: std::sync::Mutex<Client<HttpConnector, AppBody>>,
    /// The shim stamps each answer with its request's id (`REPLY_HEADER`): one without its own
    /// request's id is bytes the app wrote outside an answer, never passed on.
    replies: bool,
    exited: watch::Receiver<bool>,
    /// Set once the control pipe ends, so the shim's messages are all in.
    control_closed: watch::Receiver<bool>,
    culprit: Arc<Culprit>,
    /// Requests left in flight after the app died, for the router to blame.
    held: AtomicUsize,
    /// How long the app took to start, until the first invocation reports it.
    start_ms: AtomicU64,
    /// A program without a shim: the requests in flight, so that a line it prints while only one
    /// is can be kept with it.
    serving: Option<Arc<Serving>>,
    /// Several programs: which serves what (`authority` is the first's).
    table: Option<Table>,
    /// The app dispatches to several functions itself: it gets `x-forkrail-function`.
    dispatches: bool,
}

/// The requests a program without a shim is serving, by the router's id.
#[derive(Default)]
pub struct Serving(std::sync::Mutex<Vec<String>>);

impl Serving {
    /// The request a line printed now belongs to: the only one in flight, if there's one.
    fn only(&self) -> Option<String> {
        match self.0.lock().unwrap().as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    fn enter(self: &Arc<Self>, id: String) -> ServingGuard {
        self.0.lock().unwrap().push(id.clone());
        ServingGuard { serving: self.clone(), id }
    }
}

/// Takes a request off `Serving` once its response is finished (or dropped).
struct ServingGuard {
    serving: Arc<Serving>,
    id: String,
}

impl Drop for ServingGuard {
    fn drop(&mut self) {
        let mut list = self.serving.0.lock().unwrap();
        if let Some(at) = list.iter().position(|id| *id == self.id) {
            list.swap_remove(at);
        }
    }
}

/// A response body that holds its request on `Serving` until it ends.
struct Held {
    body: AppBody,
    _guard: ServingGuard,
}

impl hyper::body::Body for Held {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, hyper::Error>>> {
        std::pin::Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

/// What the shim sends on the control fd.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Listening {
        host: Option<String>,
        port: Option<u16>,
        path: Option<String>,
        /// Its answers carry `REPLY_HEADER`.
        #[serde(default)]
        replies: bool,
        /// What priming took before this was sent, when the server primed itself (shim.mjs).
        #[serde(default)]
        prime: Option<Prime>,
    },
    Uncaught { origin: String, request_id: Option<String>, error: String },
    /// An outgoing HTTP call the app made: method, url, status or error, start time and duration.
    /// Passed on whole to the router, which masks the URL, with the request it was made for.
    Fetch { request: Option<String> },
    /// Work a function handed `waitUntil` started (`delta` 1) or settled (-1).
    Wait { delta: i64 },
}

/// How a server's priming went: `ok`, `failed`, `timeout` (still going at its deadline) or
/// `skipped` (no time left for it).
#[derive(Debug, Clone, Deserialize)]
struct Prime {
    ms: u64,
    outcome: String,
}

/// Work the app's functions handed `waitUntil` that hasn't settled: the invocation stays open
/// while there is any (worker.rs), as Vercel keeps a function running until its `waitUntil` work
/// is done. Lambda would otherwise freeze the environment as soon as the invocation returns, and
/// the work would wait for the next thaw.
static BACKGROUND: AtomicUsize = AtomicUsize::new(0);
static BACKGROUND_DONE: std::sync::LazyLock<tokio::sync::Notify> = std::sync::LazyLock::new(tokio::sync::Notify::new);
/// The same count for whoever follows it: the log stream tells the router (logs.rs).
static BACKGROUND_NOW: std::sync::LazyLock<watch::Sender<u64>> = std::sync::LazyLock::new(|| watch::channel(0).0);

/// How much `waitUntil` work is running, as it changes.
pub fn background_watch() -> watch::Receiver<u64> {
    BACKGROUND_NOW.subscribe()
}

/// How much `waitUntil` work is running.
pub fn background() -> usize {
    BACKGROUND.load(Ordering::Acquire)
}

/// Resolve once no `waitUntil` work is running.
pub async fn background_settled() {
    loop {
        let notified = BACKGROUND_DONE.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if background() == 0 {
            return;
        }
        notified.await;
    }
}

fn background_changed(delta: i64) {
    if delta > 0 {
        BACKGROUND.fetch_add(1, Ordering::AcqRel);
    } else {
        // Never below zero, whatever the shim sends.
        let _ = BACKGROUND.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        BACKGROUND_DONE.notify_waiters();
    }
    BACKGROUND_NOW.send_replace(background() as u64);
}

/// The app is gone, and whatever it was doing in the background with it.
fn background_reset() {
    BACKGROUND.store(0, Ordering::Release);
    BACKGROUND_DONE.notify_waiters();
    BACKGROUND_NOW.send_replace(0);
}

impl UserApp {
    async fn start(config: &Config) -> Result<UserApp, Error> {
        if config.language == Language::Functions {
            return Self::start_functions(config).await;
        }
        let started = Instant::now();
        let dir = std::env::temp_dir().join(format!("forkrail-{}", env_id()));
        std::fs::create_dir_all(&dir)?;
        let shim = match config.language {
            Language::Node => Some(("shim.mjs", NODE_SHIM)),
            Language::Python => Some(("shim.py", PYTHON_SHIM)),
            Language::Binary | Language::Functions => None,
        }
        .map(|(name, source)| -> std::io::Result<PathBuf> {
            let path = dir.join(name);
            std::fs::write(&path, source)?;
            Ok(path)
        })
        .transpose()?;
        let port = match crate::userenv::var("PORT") {
            Some(port) => port,
            None => TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port().to_string(),
        };

        let (reader, writer) = std::io::pipe()?;
        let writer = OwnedFd::from(writer);
        let mut command = app_command(&config.interpreter, &config.dir, &port);
        match config.language {
            Language::Node => {
                let limit_mb = mem::limit_kb() / 1024;
                if limit_mb > 0 {
                    // `--max-old-space-size` would leave out the young generation, about 96 MB more.
                    command.arg(format!("--max-heap-size={}", limit_mb * HEAP_PCT / 100));
                }
                // Node's own limit is 16 KiB of headers, under what the router lets through (about
                // 128 KB, as on Vercel): past it Node would answer 431 itself, outside the shim.
                // An app's own flag in NODE_OPTIONS takes precedence.
                if !std::env::var("NODE_OPTIONS").is_ok_and(|o| o.contains("max-http-header-size")) {
                    command.arg(format!("--max-http-header-size={APP_MAX_HEADER_BYTES}"));
                }
                command.arg("--import").arg(shim.as_ref().expect("Node has a shim"));
                // A Next.js app's caches are the router's: Next's cache interfaces, preloaded.
                if crate::nextcache::enabled() {
                    let path = dir.join("next-cache.mjs");
                    std::fs::write(&path, crate::nextcache::NEXT_CACHE_JS)?;
                    command.arg("--import").arg(path);
                }
                command.arg(&config.entry);
                // Vercel runs every Node function with NODE_ENV=production, previews too, unless
                // the project sets its own (seen on the React Router and Remix copies; Vercel's
                // forum, 2024-09: set by the platform, Express-style servers included). Express,
                // React and others choose their production behaviour by it as they load.
                command.envs(node_env());
            }
            Language::Binary | Language::Functions => {}
            Language::Python => {
                command.arg(shim.as_ref().expect("Python has a shim")).arg(&config.entry);
                command.env("FORKRAIL_MAX_HEADER_BYTES", APP_MAX_HEADER_BYTES.to_string());
                // Output goes to a pipe, where Python would otherwise buffer it.
                if !crate::userenv::is_set("PYTHONUNBUFFERED") {
                    command.env("PYTHONUNBUFFERED", "1");
                }
            }
        }
        let shimmed = config.language != Language::Binary;
        // A program without a shim has nothing to say on the control fd.
        if shimmed {
            command.env("FORKRAIL_CONTROL_FD", CONTROL_FD.to_string());
        }
        let fd = writer.as_raw_fd();
        // SAFETY: only async-signal-safe calls run between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if !shimmed {
                    return Ok(());
                }
                let ok = if fd == CONTROL_FD {
                    // dup2 onto itself would leave close-on-exec set.
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    flags != -1 && libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != -1
                } else {
                    libc::dup2(fd, CONTROL_FD) != -1
                };
                if ok { Ok(()) } else { Err(std::io::Error::last_os_error()) }
            });
        }
        let mut child =
            command.spawn().map_err(|e| format!("starting {}: {e}", config.interpreter.display()))?;
        // Only the child writes, so the pipe ends when the child does.
        drop(writer);
        let pid = child.id().unwrap_or(0);
        let tail = Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let serving = (!shimmed).then(|| Arc::new(Serving::default()));
        let output = [
            child.stdout.take().map(|out| tokio::spawn(forward_output(out, "stdout", tail.clone(), serving.clone()))),
            child.stderr.take().map(|out| tokio::spawn(forward_output(out, "stderr", tail.clone(), serving.clone()))),
        ];
        let mut messages = BufReader::new(pipe::Receiver::from_owned_fd(reader.into())?).lines();
        let culprit = Arc::new(Culprit::default());

        // If the app exits, the control pipe ends, so this covers a crash too. A program without
        // a shim is ready once its port takes a connection.
        let listening = tokio::select! {
            listening = listening(&mut messages, &culprit), if shimmed => listening,
            listening = port_open(&mut child, &port), if !shimmed => listening.map(|authority| (authority, false, None)),
            _ = tokio::time::sleep(START_TIMEOUT) => {
                Err(format!("app wasn't listening for HTTP after {}s", START_TIMEOUT.as_secs()).into())
            }
        };
        let (authority, replies, prime) = match listening {
            Ok(listening) => listening,
            Err(e) => {
                // Give the exit status and the last of the app's output a moment to arrive.
                let status = match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
                    Ok(status) => status?.to_string(),
                    Err(_) => "still running".to_owned(),
                };
                let _ = tokio::time::timeout(EXIT_GRACE, async {
                    for task in output.into_iter().flatten() {
                        let _ = task.await;
                    }
                })
                .await;
                let tail = Vec::from(tail.lock().unwrap().clone()).join("\n");
                return Err(format!("{e} ({status}). The app's last output:\n{tail}").into());
            }
        };
        let start_ms = started.elapsed().as_millis() as u64;
        match prime {
            // `start_ms` has the priming in it: what the first request no longer pays.
            Some(prime) => info!(pid, %authority, start_ms, prime_ms = prime.ms, prime = %prime.outcome, "app listening"),
            None => info!(pid, %authority, start_ms, "app listening"),
        }
        mem::set_app_pid(pid);

        let (exited_tx, exited) = watch::channel(false);
        tokio::spawn(async move {
            let status = child.wait().await;
            warn!(pid, ?status, "app exited");
            background_reset();
            let _ = exited_tx.send(true);
        });
        let (control_tx, control_closed) = watch::channel(false);
        tokio::spawn({
            let culprit = culprit.clone();
            async move {
                while let Ok(Some(line)) = messages.next_line().await {
                    handle_message(&line, &culprit);
                }
                let _ = control_tx.send(true);
            }
        });

        Ok(UserApp {
            authority,
            client: std::sync::Mutex::new(app_client()),
            replies,
            exited,
            control_closed,
            culprit,
            held: AtomicUsize::new(0),
            start_ms: AtomicU64::new(start_ms),
            serving,
            table: None,
            dispatches: DISPATCHERS.contains(&config.entry.as_str()),
        })
    }

    /// Start every program of a deployment of several functions, each on a port of its own, and
    /// wait for them all to listen. One exiting takes the others with it, so they start again
    /// together.
    async fn start_functions(config: &Config) -> Result<UserApp, Error> {
        let started = Instant::now();
        let text = std::fs::read(&config.interpreter).map_err(|e| format!("reading {}: {e}", config.interpreter.display()))?;
        let manifest: Manifest = serde_json::from_slice(&text).map_err(|e| format!("{} isn't a functions manifest: {e}", config.interpreter.display()))?;
        if manifest.programs.is_empty() {
            return Err(format!("{} lists no programs", config.interpreter.display()).into());
        }
        let tail = Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let serving = Arc::new(Serving::default());
        let mut children = Vec::new();
        let mut ports = Vec::new();
        for program in &manifest.programs {
            let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port().to_string();
            let path = config.dir.join(&program.program);
            let mut command = app_command(&path, &config.dir, &port);
            for (key, value) in &program.env {
                command.env(key, value.replace("{port}", &port));
            }
            let mut child = command.spawn().map_err(|e| format!("starting {}: {e}", path.display()))?;
            for (out, stream) in [(child.stdout.take().map(|o| Box::new(o) as Box<dyn AsyncRead + Unpin + Send>), "stdout"), (child.stderr.take().map(|o| Box::new(o) as Box<dyn AsyncRead + Unpin + Send>), "stderr")] {
                if let Some(out) = out {
                    tokio::spawn(forward_output(out, stream, tail.clone(), Some(serving.clone())));
                }
            }
            children.push(child);
            ports.push(port);
        }
        let pids: Vec<u32> = children.iter().filter_map(|c| c.id()).collect();
        // Every one listening, or the first that can't.
        let waited = tokio::time::timeout(START_TIMEOUT, async {
            let mut authorities = Vec::new();
            for (child, port) in children.iter_mut().zip(&ports) {
                authorities.push(port_open(child, port).await?);
            }
            Ok::<_, Error>(authorities)
        })
        .await;
        let authorities = match waited {
            Ok(Ok(authorities)) => authorities,
            failed => {
                let e = match failed {
                    Ok(Err(e)) => e.to_string(),
                    _ => format!("the functions weren't all listening for HTTP after {}s", START_TIMEOUT.as_secs()),
                };
                kill_all(&pids);
                tokio::time::sleep(EXIT_GRACE).await;
                let tail = Vec::from(tail.lock().unwrap().clone()).join("\n");
                return Err(format!("{e}. The functions' last output:\n{tail}").into());
            }
        };
        let start_ms = started.elapsed().as_millis() as u64;
        info!(programs = authorities.len(), start_ms, "functions listening");
        mem::set_app_pids(&pids);

        let mut exact = std::collections::HashMap::new();
        let mut dynamic = Vec::new();
        for (i, program) in manifest.programs.iter().enumerate() {
            for function in &program.functions {
                for path in &function.paths {
                    exact.entry(path.clone()).or_insert(i);
                }
                let segments = file_segments(&function.file);
                if segments.iter().any(|s| s.starts_with('[')) {
                    dynamic.push((segments, i));
                }
            }
        }
        // More segments first, then fewer dynamic ones at the end, as Vercel orders its routes.
        dynamic.sort_by_key(|(s, _)| (std::cmp::Reverse(s.len()), s.iter().fold(0, |n, p| if p.starts_with('[') { n + 1 } else { 0 })));
        let table = Table { authorities: authorities.clone(), dispatches: manifest.programs.iter().map(|p| p.dispatches).collect(), exact, dynamic };

        let (exited_tx, exited) = watch::channel(false);
        let exited_tx = Arc::new(exited_tx);
        for mut child in children {
            let (exited_tx, pids) = (exited_tx.clone(), pids.clone());
            tokio::spawn(async move {
                let pid = child.id().unwrap_or(0);
                let status = child.wait().await;
                warn!(pid, ?status, "function program exited");
                kill_all(&pids);
                let _ = exited_tx.send(true);
            });
        }
        // No shim talks to it: closed from the start.
        let (_, control_closed) = watch::channel(true);
        Ok(UserApp {
            authority: authorities[0].clone(),
            client: std::sync::Mutex::new(app_client()),
            replies: false,
            exited,
            control_closed,
            culprit: Arc::new(Culprit::default()),
            held: AtomicUsize::new(0),
            start_ms: AtomicU64::new(start_ms),
            serving: Some(serving),
            table: Some(table),
            dispatches: false,
        })
    }

    /// Resolves once the app has exited.
    pub async fn exited(&self) {
        let _ = self.exited.clone().wait_for(|exited| *exited).await;
    }

    /// How many requests the app's death is blamed on, still in flight so that the router sees them
    /// die with the tunnel.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Relaxed)
    }

    /// How long the app took to start, the first time this is called; zero after that.
    pub fn take_start_ms(&self) -> u64 {
        self.start_ms.swap(0, Ordering::Relaxed)
    }

    /// Send one request from the tunnel to the app.
    pub async fn handle(&self, req: Request<Incoming>) -> Result<Response<AppBody>, Error> {
        let (mut parts, body) = req.into_parts();
        let request_id = parts.headers.get(REQUEST_ID).and_then(|v| v.to_str().ok()).map(str::to_owned);
        // HTTP/2 carries the host in the URI; HTTP/1.1 wants the header.
        if let Some(host) = parts.uri.authority().and_then(|a| HeaderValue::from_str(a.as_str()).ok()) {
            parts.headers.insert(HOST, host);
        }
        // The router already answered it.
        parts.headers.remove(EXPECT);
        // Several functions: the one the router's routes reached, else the one the path is.
        let named = parts.headers.remove(FUNCTION_HEADER);
        let authority = match &self.table {
            None => {
                if self.dispatches
                    && let Some(named) = named
                {
                    parts.headers.insert(FUNCTION_HEADER, named);
                }
                &self.authority
            }
            Some(table) => match table.pick(named.as_ref().and_then(|v| v.to_str().ok()), parts.uri.path()) {
                Some((i, params)) => {
                    if table.dispatches[i]
                        && let Some(named) = named
                    {
                        parts.headers.insert(FUNCTION_HEADER, named);
                    }
                    if !params.is_empty() {
                        let mut query = parts.uri.query().unwrap_or("").to_owned();
                        for (name, value) in params {
                            if !query.is_empty() {
                                query.push('&');
                            }
                            query.push_str(&format!("{name}={value}"));
                        }
                        parts.uri = format!("{}?{query}", parts.uri.path()).parse()?;
                    }
                    &table.authorities[i]
                }
                None => return Ok(not_found()),
            },
        };
        let path_and_query = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
        parts.uri = format!("http://{authority}{path_and_query}").parse()?;
        parts.version = Version::HTTP_11;
        let guard = match (&self.serving, &request_id) {
            (Some(serving), Some(id)) => Some(serving.enter(id.clone())),
            _ => None,
        };

        let client = self.client.lock().unwrap().clone();
        match client.request(Request::from_parts(parts, body.boxed())).await {
            Ok(response) => {
                let (mut parts, body) = response.into_parts();
                let reply = parts.headers.remove(REPLY_HEADER);
                if self.replies
                    && let Some(id) = &request_id
                    && reply.as_ref().is_none_or(|r| r.as_bytes() != id.as_bytes())
                {
                    // Not this request's answer: bytes the app wrote outside its answers (to its
                    // socket, after an answer ended), read as this one's on a pooled connection.
                    // Never passed on, and no connection of this pool is used again.
                    warn!(request = %id, answered = ?reply, status = parts.status.as_u16(), "the app's answer isn't this request's: dropping its connections");
                    *self.client.lock().unwrap() = app_client();
                    drop(body);
                    return Ok(retry_elsewhere(RETRY_APP_CONNECTION, Some("the app answered out of step (it wrote outside an answer)")));
                }
                strip_hop_by_hop(&mut parts.headers);
                parts.version = Version::HTTP_2;
                let body = match guard {
                    Some(guard) => Held { body: body.boxed(), _guard: guard }.boxed(),
                    None => body.boxed(),
                };
                Ok(Response::from_parts(parts, body))
            }
            Err(e) => {
                if tokio::time::timeout(EXIT_GRACE, self.exited()).await.is_err() {
                    // The app lives on, but the connection failed under the request: uvicorn,
                    // for one, closes a connection after an exception in a request it has
                    // already answered, which can be just after the pool sent this one on it.
                    // The app may have started on it, so the router retries it only if that's
                    // harmless, but the worker is fine.
                    let error = error_chain(&e);
                    warn!(error, "connection to the app failed");
                    return Ok(retry_elsewhere(RETRY_APP_CONNECTION, Some(&error)));
                }
                let _ = tokio::time::timeout(EXIT_GRACE, self.control_closed.clone().wait_for(|closed| *closed)).await;
                match self.culprit.lock().unwrap().clone() {
                    // The app died of an error in another request, or in none. Tell the router
                    // this one wasn't the cause, so it retries it like any other unfinished request.
                    Some(culprit) if culprit != request_id => return Ok(retry_elsewhere(RETRY_APP_EXITED, None)),
                    // This request threw, or the cause is unknown (a heap overflow, `process.exit`).
                    _ => {}
                }
                // Leave it hanging. The worker drops the tunnel with it in flight, and the router
                // handles it as having killed the worker: retried alone if safe to repeat, and its
                // route treated as needing a whole worker.
                self.held.fetch_add(1, Ordering::Relaxed);
                std::future::pending().await
            }
        }
    }
}

/// `NODE_ENV=production` for a Node app, unless the project set `NODE_ENV` itself (to anything,
/// even empty: what it typed is kept).
fn node_env() -> Option<(&'static str, &'static str)> {
    (!crate::userenv::is_set("NODE_ENV")).then_some(("NODE_ENV", "production"))
}

/// The pooled HTTP/1.1 client to the app.
fn app_client() -> Client<HttpConnector, AppBody> {
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(POOL_IDLE)
        .pool_timer(TokioTimer::new())
        // The router's host goes to the app, not our loopback address.
        .set_host(false)
        .build(connector)
}

/// The command that runs `program` as the app: in the app directory, with the app's env vars and
/// `PORT`, its output piped to the runtime.
fn app_command(program: &std::path::Path, dir: &std::path::Path, port: &str) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        // A sealed file's variables, before the runtime's own; its key stays here.
        .envs(crate::userenv::from_file())
        .env_remove(forkrail_common::envfile::KEY_VAR)
        .envs(crate::userenv::region_vars())
        .env("PORT", port)
        .envs(crate::egress::app_env().iter().map(|(k, v)| (k, v)))
        .envs(crate::purge::app_env().iter().map(|(k, v)| (k, v)))
        .envs(crate::nextcache::app_env().iter().map(|(k, v)| (k, v)))
        .envs([crate::ownhosts::app_env()])
        // Local mode passes the worker's token this way; the app has no business with it.
        .env_remove(LOCAL_PAYLOAD_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // The project's own AWS keys without a token of their own: Lambda's belongs to the role's.
    if crate::userenv::own_keys_without_token() {
        command.env_remove("AWS_SESSION_TOKEN");
    }
    command
}

fn kill_all(pids: &[u32]) {
    for pid in pids {
        // SAFETY: a plain syscall on a pid this runtime started.
        unsafe {
            libc::kill(*pid as i32, libc::SIGKILL);
        }
    }
}

/// No function serves the path: Vercel's 404.
fn not_found() -> Response<AppBody> {
    let body = http_body_util::Full::new(Bytes::from_static(b"The page could not be found.\n\nNOT_FOUND\n")).map_err(|never| match never {}).boxed();
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::NOT_FOUND;
    response.headers_mut().insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    response.headers_mut().insert("x-forkrail-error", HeaderValue::from_static("NOT_FOUND"));
    response
}

fn retry_elsewhere(reason: &'static str, error: Option<&str>) -> Response<AppBody> {
    let mut response = Response::new(Empty::new().map_err(|never| match never {}).boxed());
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response.headers_mut().insert(RETRY_HEADER, HeaderValue::from_static(reason));
    if let Some(value) = error.and_then(|e| HeaderValue::from_str(&header_safe(e)).ok()) {
        response.headers_mut().insert(APP_ERROR_HEADER, value);
    }
    response
}

/// An error and its causes, as one line: hyper's own message alone ("client error (SendRequest)")
/// hides what happened.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(e) = source {
        text.push_str(": ");
        text.push_str(&e.to_string());
        source = e.source();
    }
    text
}

/// Printable ASCII, at most 200 characters, for a header value.
fn header_safe(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' }).take(200).collect()
}

/// Copy the app's output to our stderr, and into the log buffer the router reads (`logs.rs`),
/// keeping the last lines for startup errors. Our stdout carries the report in local mode.
async fn forward_output(mut output: impl AsyncRead + Unpin, stream: &'static str, tail: Arc<std::sync::Mutex<VecDeque<String>>>, serving: Option<Arc<Serving>>) {
    let mut splitter = LineSplitter::default();
    let mut chunk = vec![0u8; 8192];
    let keep = |line: String| {
        let (line, request) = logs::split_tag(line);
        let request = request.or_else(|| serving.as_ref().and_then(|s| s.only()));
        eprintln!("{line}");
        {
            let mut tail = tail.lock().unwrap();
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line.clone());
        }
        logs::push(stream, line, request);
    };
    loop {
        match output.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => splitter.feed(&chunk[..n], keep),
        }
    }
    splitter.finish(keep);
}

/// Wait for a program without a shim to take connections on `port`, and return the address it
/// took one on: IPv4's loopback, else IPv6's. An error if it exits first.
async fn port_open(child: &mut tokio::process::Child, port: &str) -> Result<String, Error> {
    let port: u16 = port.parse().map_err(|_| format!("PORT {port:?} isn't a port number"))?;
    loop {
        for authority in [format!("127.0.0.1:{port}"), format!("[::1]:{port}")] {
            if tokio::net::TcpStream::connect(&authority).await.is_ok() {
                return Ok(authority);
            }
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("app exited before listening for HTTP on port {port} ({status})").into());
        }
        tokio::time::sleep(PORT_POLL).await;
    }
}

/// Wait for the shim to report the app's HTTP server listening, and return its address and
/// whether it stamps its answers.
async fn listening(messages: &mut Lines<BufReader<pipe::Receiver>>, culprit: &Culprit) -> Result<(String, bool, Option<Prime>), Error> {
    while let Some(line) = messages.next_line().await? {
        match serde_json::from_str(&line) {
            Ok(Message::Listening { host: Some(host), port: Some(port), replies, prime, .. }) => {
                let authority = match host.parse::<IpAddr>() {
                    // Node listens dual-stack on `::` by default.
                    Ok(ip) if ip.is_unspecified() => format!("127.0.0.1:{port}"),
                    Ok(IpAddr::V6(ip)) => format!("[{ip}]:{port}"),
                    _ => format!("{host}:{port}"),
                };
                return Ok((authority, replies, prime));
            }
            Ok(Message::Listening { path, .. }) => {
                return Err(format!("app listens on {path:?}; only TCP is supported").into());
            }
            Ok(_) => handle_message(&line, culprit),
            Err(e) => warn!(error = %e, line, "bad message from the shim"),
        }
    }
    Err("app exited before listening for HTTP".into())
}

fn handle_message(line: &str, culprit: &Culprit) {
    match serde_json::from_str(line) {
        Ok(Message::Uncaught { origin, request_id, error }) => {
            warn!(origin, request_id, error, "app: uncaught error");
            // The first one is what kills the app, unless the app handles them itself.
            culprit.lock().unwrap().get_or_insert(request_id);
        }
        Ok(Message::Fetch { request }) => logs::push("fetch", line.to_owned(), request),
        Ok(Message::Wait { delta }) => background_changed(delta),
        Ok(Message::Listening { .. }) => {}
        Err(e) => warn!(error = %e, line, "bad message from the shim"),
    }
}
