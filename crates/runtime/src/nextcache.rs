//! Next.js's shared cache, the runtime's part (N2; the design is in the router's
//! `cache/next.rs`, the frames in `forkrail_common::nextcache`): a relay between Next's process
//! and the router.
//!
//! With `FORKRAIL_NEXT_CACHE=1` in the function's environment (the builder sets it for a Next.js
//! deployment) the runtime preloads `next-cache.mjs` into the app (Next's two cache interfaces),
//! listens on a loopback port for it (`FORKRAIL_NEXT_CACHE_AT`, `127.0.0.1:<port>:<secret>`, as
//! `FORKRAIL_PURGE`), and announces `cache` in the worker's preface. The router then opens the
//! cache stream on each invocation's tunnel (`attach`): the app's frames go up it and the
//! router's come down, with no connection of their own. Requests get an id of the runtime's on
//! the way up (the app may run more than one process), and their own back on the way down.
//!
//! While no stream is up but an invocation still runs (the router retired the worker and
//! `after()` / `waitUntil` work goes on) the frames go over a connection dialed as the worker
//! (`FORKRAIL-CACHE/1`), kept until the router closes it. With neither, a request is answered
//! here with an error, which the app takes as a miss or drops.

use std::collections::HashMap;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use forkrail_common::nextcache::{self as wire, Frame, kind};
use forkrail_common::{PurgeHello, send_cache};
use http_body_util::BodyExt;
use hyper::body::{Body, Incoming};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, watch};
use tracing::{debug, info, warn};

use crate::app::AppBody;
use crate::{egress, tls};

/// Next's cache interfaces, preloaded into the app with `--import`.
pub const NEXT_CACHE_JS: &str = include_str!("next-cache.mjs");

static RELAY: OnceLock<Arc<Relay>> = OnceLock::new();
static APP_ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();

/// A request waits this long for the router to open the stream on a new tunnel before dialing.
const STREAM_WAIT: Duration = Duration::from_millis(150);
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
const DIAL_AGAIN_AFTER: Duration = Duration::from_secs(30);
/// Frames waiting for the app or the router to read them.
const QUEUE: usize = 256;

/// Whether this function's app is a Next.js server whose caches are the router's.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::userenv::var("FORKRAIL_NEXT_CACHE").is_some_and(|v| v == "1"))
}

/// The variables to give the app.
pub fn app_env() -> &'static [(String, String)] {
    APP_ENV.get().map_or(&[], Vec::as_slice)
}

#[derive(Serialize, Deserialize, Default)]
struct Hello {
    secret: String,
}

/// What the app is told when a stream comes or goes.
#[derive(Serialize)]
struct Link {
    /// Which stream: each invocation's is a new one.
    r#gen: u64,
}

/// An answer made here: no router took the request.
#[derive(Serialize)]
struct Failed<'a> {
    error: &'a str,
}

struct Pending {
    conn: u64,
    id: u32,
    /// The stream or dialed connection it went up.
    via: u64,
}

struct Relay {
    secret: String,
    /// The app's connections, by number.
    conns: Mutex<HashMap<u64, mpsc::Sender<Bytes>>>,
    /// Requests sent up, by the id they went with.
    pending: Mutex<HashMap<u32, Pending>>,
    /// The tunnel's cache stream, when there's one: its number and the way up it.
    stream: Mutex<Option<(u64, mpsc::Sender<Bytes>)>>,
    stream_up: Notify,
    /// The worker whose stream isn't coming (it's gone, or the router never opened one).
    no_stream_for: Mutex<String>,
    dial: tokio::sync::Mutex<Option<(u64, mpsc::Sender<Bytes>)>>,
    /// When a dial last failed: not tried again for `DIAL_AGAIN_AFTER` (a router that keeps no
    /// cache, or one from before it, refuses every time).
    dial_failed: Mutex<Option<std::time::Instant>>,
    ids: AtomicU32,
    numbers: AtomicU64,
}

/// Listen for the app's cache connection. Called once, before the app starts.
pub fn start() {
    if !enabled() {
        return;
    }
    let listener = match std::net::TcpListener::bind(("127.0.0.1", 0)).and_then(|l| l.set_nonblocking(true).map(|()| l)).and_then(TcpListener::from_std) {
        Ok(listener) => listener,
        Err(e) => {
            warn!(error = %e, "cache listener failed: Next.js keeps its caches in each environment");
            return;
        }
    };
    let Ok(addr) = listener.local_addr() else { return };
    let secret = forkrail_common::random_hex(16);
    let _ = APP_ENV.set(vec![("FORKRAIL_NEXT_CACHE_AT".to_owned(), format!("{addr}:{secret}"))]);
    let relay = Arc::new(Relay {
        secret,
        conns: Mutex::default(),
        pending: Mutex::default(),
        stream: Mutex::default(),
        stream_up: Notify::new(),
        no_stream_for: Mutex::default(),
        dial: tokio::sync::Mutex::default(),
        dial_failed: Mutex::default(),
        ids: AtomicU32::new(1),
        numbers: AtomicU64::new(1),
    });
    let _ = RELAY.set(relay.clone());
    info!("Next.js cache listener up");
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((app, _)) => {
                    let relay = relay.clone();
                    tokio::spawn(async move { relay.serve_app(app).await });
                }
                Err(e) => warn!(error = %e, "cache accept failed"),
            }
        }
    });
}

fn same(a: &str, b: &str) -> bool {
    !b.is_empty() && a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Relay {
    fn number(&self) -> u64 {
        self.numbers.fetch_add(1, Ordering::Relaxed)
    }

    /// One of the app's connections: its secret first, then its requests.
    async fn serve_app(self: Arc<Self>, app: TcpStream) {
        let _ = app.set_nodelay(true);
        let (mut read, mut write) = app.into_split();
        let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if write.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        let conn = self.number();
        let mut known = false;
        let mut buf = BytesMut::with_capacity(64 << 10);
        'conn: loop {
            match read.read_buf(&mut buf).await {
                Ok(n) if n > 0 => {}
                _ => break,
            }
            loop {
                let frame = match wire::take_frame(&mut buf) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(_) => break 'conn,
                };
                if !known {
                    let ok = frame.kind == kind::HELLO && frame.open::<Hello>().is_some_and(|(hello, _)| same(&hello.secret, &self.secret));
                    if !ok {
                        warn!("a cache connection without this environment's secret: closed");
                        break 'conn;
                    }
                    known = true;
                    self.conns.lock().unwrap().insert(conn, tx.clone());
                    // A stream already up is one it may use.
                    let up = self.stream.lock().unwrap().as_ref().map(|(number, _)| *number);
                    if let Some(number) = up {
                        let _ = tx.send(Frame::new(kind::UP, 0, &Link { r#gen: number }, b"").encode()).await;
                    }
                    continue;
                }
                self.up(conn, frame).await;
            }
        }
        self.conns.lock().unwrap().remove(&conn);
        self.pending.lock().unwrap().retain(|_, p| p.conn != conn);
    }

    /// A request from the app, sent to the router: up the tunnel's stream, else a dialed connection.
    async fn up(self: &Arc<Self>, conn: u64, frame: Frame) {
        let id = loop {
            let id = self.ids.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                break id;
            }
        };
        let own = frame.id;
        let bytes = Frame { kind: frame.kind, id, payload: frame.payload }.encode();
        let mut way = self.stream.lock().unwrap().clone();
        if way.is_none() {
            // A new invocation's stream is opened by the router a moment after its tunnel: wait
            // for it once, unless this worker's is known not to come.
            let session = egress::session();
            let waits = session.as_ref().is_some_and(|s| *self.no_stream_for.lock().unwrap() != s.worker_id);
            if waits {
                let _ = tokio::time::timeout(STREAM_WAIT, self.stream_up.notified()).await;
                way = self.stream.lock().unwrap().clone();
                if way.is_none()
                    && let Some(session) = session
                {
                    *self.no_stream_for.lock().unwrap() = session.worker_id;
                }
            }
        }
        if way.is_none() {
            way = match self.dialed().await {
                Ok(way) => Some(way),
                Err(e) => {
                    debug!(error = %e, "no way to the router's cache");
                    None
                }
            };
        }
        let Some((via, tx)) = way else { return self.failed(conn, own, "the router can't be reached").await };
        self.pending.lock().unwrap().insert(id, Pending { conn, id: own, via });
        if tx.send(bytes).await.is_err() {
            self.pending.lock().unwrap().remove(&id);
            self.failed(conn, own, "the router can't be reached").await;
        }
    }

    async fn failed(&self, conn: u64, id: u32, error: &str) {
        let tx = self.conns.lock().unwrap().get(&conn).cloned();
        if let Some(tx) = tx {
            let _ = tx.send(Frame::new(kind::REPLY, id, &Failed { error }, b"").encode()).await;
        }
    }

    /// A frame from the router: an answer to the connection that asked, anything else to all.
    async fn down(&self, frame: Frame) {
        if frame.id == 0 {
            return self.tell(frame.encode()).await;
        }
        let Some(pending) = self.pending.lock().unwrap().remove(&frame.id) else { return };
        let tx = self.conns.lock().unwrap().get(&pending.conn).cloned();
        if let Some(tx) = tx {
            let _ = tx.send(Frame { kind: frame.kind, id: pending.id, payload: frame.payload }.encode()).await;
        }
    }

    async fn tell(&self, bytes: Bytes) {
        let conns: Vec<mpsc::Sender<Bytes>> = self.conns.lock().unwrap().values().cloned().collect();
        for tx in conns {
            let _ = tx.send(bytes.clone()).await;
        }
    }

    /// A way up is gone: what went up it and wasn't answered won't be.
    async fn gone(&self, via: u64) {
        let lost: Vec<Pending> = {
            let mut pending = self.pending.lock().unwrap();
            let ids: Vec<u32> = pending.iter().filter(|(_, p)| p.via == via).map(|(id, _)| *id).collect();
            ids.into_iter().filter_map(|id| pending.remove(&id)).collect()
        };
        for p in lost {
            self.failed(p.conn, p.id, "the router's cache connection ended").await;
        }
    }

    /// The dialed connection to the router's cache, as the invocation's worker: the one there is,
    /// or a new one.
    async fn dialed(self: &Arc<Self>) -> Result<(u64, mpsc::Sender<Bytes>), String> {
        let mut dial = self.dial.lock().await;
        if let Some((number, tx)) = dial.as_ref().filter(|(_, tx)| !tx.is_closed()) {
            return Ok((*number, tx.clone()));
        }
        let session = egress::session().ok_or("no invocation is running")?;
        if self.dial_failed.lock().unwrap().is_some_and(|at| at.elapsed() < DIAL_AGAIN_AFTER) {
            return Err("the router refused a moment ago".into());
        }
        let connect = async {
            let tcp = TcpStream::connect(&session.router_addr).await.map_err(|e| e.to_string())?;
            let _ = tcp.set_nodelay(true);
            let mut router = tls::connect(&session.cert_sha256, tcp).await.map_err(|e| e.to_string())?;
            send_cache(&mut router, &PurgeHello { worker_id: session.worker_id.clone(), token: session.token.clone() }).await.map_err(|e| e.to_string())?;
            Ok::<_, String>(router)
        };
        let router = match tokio::time::timeout(DIAL_TIMEOUT, connect).await.map_err(|_| "the router didn't answer in time".to_owned()).and_then(|r| r) {
            Ok(router) => router,
            Err(e) => {
                *self.dial_failed.lock().unwrap() = Some(std::time::Instant::now());
                return Err(e);
            }
        };
        *self.dial_failed.lock().unwrap() = None;
        let number = self.number();
        let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
        let this = self.clone();
        tokio::spawn(async move {
            let (mut read, mut write) = tokio::io::split(router);
            let mut buf = BytesMut::with_capacity(64 << 10);
            'conn: loop {
                tokio::select! {
                    bytes = rx.recv() => match bytes {
                        Some(bytes) if write.write_all(&bytes).await.is_ok() && write.flush().await.is_ok() => {}
                        _ => break,
                    },
                    read = read.read_buf(&mut buf) => {
                        if !read.is_ok_and(|n| n > 0) {
                            break;
                        }
                        loop {
                            match wire::take_frame(&mut buf) {
                                Ok(Some(frame)) => this.down(frame).await,
                                Ok(None) => break,
                                Err(_) => break 'conn,
                            }
                        }
                    }
                }
            }
            drop(rx);
            this.gone(number).await;
        });
        *dial = Some((number, tx.clone()));
        Ok((number, tx))
    }
}

/// The router opened the cache stream on this invocation's tunnel: its frames come in `body`,
/// the app's go out in the answer's, until the worker closes (`closing`) or the router does.
pub fn attach(body: Incoming, mut closing: watch::Receiver<bool>) -> AppBody {
    let Some(relay) = RELAY.get().cloned() else { return http_body_util::Empty::new().map_err(|never| match never {}).boxed() };
    let (tx, rx) = mpsc::channel::<Bytes>(QUEUE);
    let number = relay.number();
    *relay.stream.lock().unwrap() = Some((number, tx));
    relay.stream_up.notify_waiters();
    tokio::spawn(async move {
        relay.tell(Frame::new(kind::UP, 0, &Link { r#gen: number }, b"").encode()).await;
        let mut body = body;
        let mut buf = BytesMut::with_capacity(64 << 10);
        'stream: loop {
            let frame = tokio::select! {
                frame = body.frame() => frame,
                _ = closing.wait_for(|closing| *closing) => break,
            };
            let Some(Ok(frame)) = frame else { break };
            let Ok(data) = frame.into_data() else { continue };
            buf.extend_from_slice(&data);
            loop {
                match wire::take_frame(&mut buf) {
                    Ok(Some(frame)) => relay.down(frame).await,
                    Ok(None) => break,
                    Err(_) => break 'stream,
                }
            }
        }
        // Gone, unless a newer invocation's stream has taken its place already.
        let was_current = {
            let mut stream = relay.stream.lock().unwrap();
            let current = stream.as_ref().is_some_and(|(n, _)| *n == number);
            if current {
                *stream = None;
            }
            current
        };
        if was_current {
            // This invocation's stream won't come back: its later requests dial.
            if let Some(session) = egress::session() {
                *relay.no_stream_for.lock().unwrap() = session.worker_id;
            }
            relay.tell(Frame::new(kind::DOWN, 0, &Link { r#gen: number }, b"").encode()).await;
        }
        relay.gone(number).await;
    });
    Outgoing(rx).map_err(|never| match never {}).boxed()
}

/// The answer's body on the cache stream: the app's frames, until the stream's task ends.
struct Outgoing(mpsc::Receiver<Bytes>);

impl Body for Outgoing {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        self.0.poll_recv(cx).map(|bytes| bytes.map(|b| Ok(hyper::body::Frame::data(b))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay() -> Arc<Relay> {
        Arc::new(Relay {
            secret: "s3cret".into(),
            conns: Mutex::default(),
            pending: Mutex::default(),
            stream: Mutex::default(),
            stream_up: Notify::new(),
            no_stream_for: Mutex::default(),
            dial: tokio::sync::Mutex::default(),
            dial_failed: Mutex::default(),
            ids: AtomicU32::new(1),
            numbers: AtomicU64::new(1),
        })
    }

    async fn frames(app: &mut TcpStream, buf: &mut BytesMut, n: usize) -> Vec<Frame> {
        let mut out = Vec::new();
        while out.len() < n {
            while let Some(frame) = wire::take_frame(buf).unwrap() {
                out.push(frame);
            }
            if out.len() < n {
                assert!(tokio::time::timeout(Duration::from_secs(5), app.read_buf(buf)).await.unwrap().unwrap() > 0, "the runtime closed the connection");
            }
        }
        out
    }

    /// The app's requests go up the stream with the runtime's ids and their answers come back
    /// with the app's own; what the router sends unasked reaches the app; a stream that ends
    /// answers what it left unanswered; and without any way up, a request is answered at once.
    #[tokio::test]
    async fn frames_between_the_app_and_the_router() {
        let relay = relay();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn({
            let relay = relay.clone();
            async move {
                loop {
                    let (app, _) = listener.accept().await.unwrap();
                    tokio::spawn(relay.clone().serve_app(app));
                }
            }
        });
        // Without the secret: closed.
        let mut stranger = TcpStream::connect(addr).await.unwrap();
        stranger.write_all(&Frame::new(kind::HELLO, 0, &Hello { secret: "guess".into() }, b"").encode()).await.unwrap();
        assert_eq!(stranger.read(&mut [0u8; 16]).await.unwrap(), 0);

        let mut app = TcpStream::connect(addr).await.unwrap();
        let mut buf = BytesMut::new();
        app.write_all(&Frame::new(kind::HELLO, 0, &Hello { secret: "s3cret".into() }, b"").encode()).await.unwrap();
        // No invocation, no stream: answered here, as a failure the app takes as a miss.
        app.write_all(&Frame::new(kind::GET, 41, &wire::Get::default(), b"").encode()).await.unwrap();
        let got = frames(&mut app, &mut buf, 1).await;
        assert_eq!((got[0].kind, got[0].id), (kind::REPLY, 41));
        assert!(got[0].open::<wire::GetReply>().unwrap().0.error.is_some());

        // A stream comes up (what `attach` does with the tunnel's request).
        let (tx, mut to_router) = mpsc::channel::<Bytes>(8);
        *relay.stream.lock().unwrap() = Some((77, tx));
        app.write_all(&Frame::new(kind::GET, 42, &wire::Get { key_hash: "k".into(), ..wire::Get::default() }, b"").encode()).await.unwrap();
        let mut up = BytesMut::from(&to_router.recv().await.unwrap()[..]);
        let sent = wire::take_frame(&mut up).unwrap().unwrap();
        assert_eq!(sent.kind, kind::GET);
        assert_ne!(sent.id, 42, "the runtime's id, not the app's");
        assert_eq!(sent.open::<wire::Get>().unwrap().0.key_hash, "k");
        // The router's answer, and something it sends unasked.
        relay.down(Frame::new(kind::MARKS, 0, &wire::Marks { marks: vec![], synced: true, now: 5 }, b"")).await;
        relay.down(Frame::new(kind::REPLY, sent.id, &wire::GetReply { hit: true, t: 9, ..wire::GetReply::default() }, b"body")).await;
        let got = frames(&mut app, &mut buf, 2).await;
        assert_eq!((got[0].kind, got[0].id), (kind::MARKS, 0));
        assert_eq!((got[1].kind, got[1].id), (kind::REPLY, 42));
        let (reply, body) = got[1].open::<wire::GetReply>().unwrap();
        assert!(reply.hit && reply.t == 9 && &body[..] == b"body");
        // The stream ends with a request unanswered: the app is told, not left waiting.
        app.write_all(&Frame::new(kind::SET, 43, &wire::Set::default(), b"").encode()).await.unwrap();
        to_router.recv().await.unwrap();
        *relay.stream.lock().unwrap() = None;
        relay.gone(77).await;
        let got = frames(&mut app, &mut buf, 1).await;
        assert_eq!((got[0].kind, got[0].id), (kind::REPLY, 43));
        assert!(got[0].open::<wire::Done>().unwrap().0.error.is_some());
        assert!(relay.pending.lock().unwrap().is_empty());
    }
}
