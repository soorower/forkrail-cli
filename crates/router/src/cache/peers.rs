//! The routers' link for pages (docs/c16-frameworks.md, F4b): every router keeps the same pages, and
//! the home region's router (the install's state region, us-east-1) makes them. Vercel's shape: a
//! copy in every region and one function region that renders, its changes pushed to all ("global
//! content updates in 300ms").
//!
//! - **An edge** (any other router) keeps one HTTP/2 connection to home open. On it, its own
//!   requests (`Ask`): a page it has none of (`Fetch`), a stale one (`Refresh`: home makes it again
//!   once, however many routers ask), an on-demand revalidation (`Revalidate`); and one long
//!   request, `/subscribe`, down which home sends every page it makes (and every group revalidated
//!   on demand, and every purge by tag) as it keeps it. A page an edge's own request brought isn't
//!   pushed back to it.
//! - **Purges by tag** from a function reach the router its worker is connected to; an edge sends
//!   them to home (`/tags`), which keeps them and pushes them to the others. While home can't be
//!   reached an edge keeps them itself and sends them once the link is back.
//! - **Joining:** a router whose store is new asks a peer for everything it has (home, else any
//!   other); an edge that connects again asks home for what was made since it last heard (with a
//!   margin, since two renders finish out of order).
//! - **Home unreachable:** an edge does what a router alone does, and marks what it makes `local`;
//!   home's copy replaces it once home has a newer one, which is asked for when the page is next
//!   served (`cached.rs`).
//!
//! Security: a port of its own (`FORKRAIL_PEER_LISTEN`, 7445), which the security group opens to
//! the other routers' addresses only. TLS 1.3 with a certificate made at start (nothing to renew);
//! both sides then prove they hold the install's peer key (`FORKRAIL_PEER_KEY_PARAM`, an SSM
//! SecureString under the install key) by an HMAC of the TLS session's exported keying material
//! (RFC 5705, as RFC 9266 binds a channel), on every request and every answer. Someone between two
//! routers has two sessions with different keying material and can't make either proof, so neither
//! side trusts a certificate.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http2::SendRequest;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use ring::hmac;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{info, warn};

use super::{Meta, ResponseCache, TagMark};
use crate::BoxError;
use crate::proxy::ResponseBody;

pub const DEFAULT_LISTEN: &str = "0.0.0.0:7445";
pub(crate) const AUTH_HEADER: &str = "x-forkrail-peer-auth";
/// What home's answer to an `Ask` is: `kept` (a page, also stored here), `unkept` (an answer that
/// couldn't be one), `none`, `rendering` (none yet, being made), `not-found`, `failed`, `unknown`.
const REPLY_HEADER: &str = "x-forkrail-peer-reply";
/// With `rendering`: the page may have existed before every copy was lost (`ResponseCache::lost`).
const LOST_HEADER: &str = "x-forkrail-peer-lost";
/// With `kept`: made for this ask, rather than kept from before.
const MADE_HEADER: &str = "x-forkrail-peer-made";
const EXPORTER_LABEL: &[u8] = b"EXPORTER-forkrail-peer-link";
/// The name the client asks for; neither side reads certificates' names.
const SERVER_NAME: &str = "forkrail-peer";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Home says it's there this often on a quiet `/subscribe`; nothing for `SILENT_FOR` and the edge
/// takes the link as down and connects again.
const BEAT_EVERY: Duration = Duration::from_secs(10);
const SILENT_FOR: Duration = Duration::from_secs(30);
/// Pushes waiting for an edge that reads slowly; past it, its subscription is cut, and it asks
/// again for what it missed when it connects again.
const QUEUE: usize = 1024;
/// A reconnecting edge asks for pages made this long before the last it heard of: two renders
/// finish out of order, and the last one heard of isn't the last made.
const SINCE_MARGIN_MS: u64 = 15 * 60 * 1000;
const MAX_FRAME: usize = super::MAX_PAGE_BYTES + (1 << 20);
/// Kinds of frame on `/subscribe`: `[kind][length u32 BE][payload]`.
const PAGE: u8 = 1;
const GROUP: u8 = 2;
const SYNCED: u8 = 3;
const BEAT: u8 = 4;
const TAGS: u8 = 5;
/// A stale Next.js entry is being made again by one router's worker, until a time: `(hash, ms)`.
const LEASE: u8 = 6;
/// Marks in one `TAGS` frame when a router joins.
const TAGS_PER_FRAME: usize = 1000;
/// How long an edge waits for home to take a purge before keeping it to send later.
const TAGS_TIMEOUT: Duration = Duration::from_secs(5);
/// Requests at once on one connection from another router: its pages and the requests it runs
/// here (`relay.rs`), however busy it is.
const RUN_STREAMS: u32 = 10_000;
/// Windows for a connection across regions: a page of a few MB goes in one round trip or two.
const STREAM_WINDOW: u32 = 4 << 20;
const CONNECTION_WINDOW: u32 = 8 << 20;

/// `FORKRAIL_PEERS`: comma-separated `region=host:port`, every router of the install (this one
/// may be among them).
pub fn parse_peers(spec: &str) -> Result<BTreeMap<String, String>, String> {
    let mut peers = BTreeMap::new();
    for item in spec.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let Some((region, addr)) = item.split_once('=') else { return Err(format!("{item:?}: expected region=host:port")) };
        let (region, addr) = (region.trim(), addr.trim());
        if region.is_empty() || !region.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(format!("{region:?}: not a region"));
        }
        if addr.rsplit_once(':').is_none_or(|(host, port)| host.is_empty() || port.parse::<u16>().is_err()) {
            return Err(format!("{addr:?}: expected host:port"));
        }
        peers.insert(region.to_owned(), addr.to_owned());
    }
    Ok(peers)
}

/// What an edge asks of home.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// The page, which the asker hasn't got: home's copy, or made (while the asker waits with
    /// `wait`, else in the background with `render`).
    Fetch,
    /// The asker's copy (made at `have`) is stale: home's newer one, or made again once.
    Refresh,
    /// Made again now, on demand (`x-prerender-revalidate`).
    Revalidate,
    /// A Next.js page served stale at the edge: home has Next make it again (N3,
    /// `proxy/cached/nextjs.rs`); what Next keeps reaches the edge as any write does. Home
    /// answers at once (`Answer::None`). A home from before it can't read the ask, and the edge
    /// has it made itself.
    Regenerate,
}

/// One page's request, as the asker routed it: home runs the same function with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ask {
    pub op: Op,
    pub host: String,
    pub deployment: String,
    pub function: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_ms: Option<u64>,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub have: Option<u64>,
    #[serde(default)]
    pub wait: bool,
    #[serde(default)]
    pub render: bool,
    #[serde(default)]
    pub if_generated: bool,
    /// The request the function gets (query and headers as the cache made them).
    pub uri: String,
    #[serde(skip)]
    pub headers: HeaderMap,
    pub request_id: String,
    /// The asker's `Link::id`: the page it gets in the answer isn't pushed to it again.
    pub from: String,
}

impl Ask {
    fn encode(&self) -> Bytes {
        let mut out = serde_json::to_vec(self).expect("plain fields");
        out.push(b'\n');
        super::put_headers(&self.headers, &mut out);
        Bytes::from(out)
    }

    fn decode(bytes: &[u8]) -> Option<Ask> {
        let at = bytes.iter().position(|&b| b == b'\n')?;
        let mut ask: Ask = serde_json::from_slice(&bytes[..at]).ok()?;
        ask.headers = super::take_headers(bytes, at + 1)?.0;
        Some(ask)
    }
}

/// Home's answer, as its renderer makes it (`cached::peer_renders`).
pub enum Answer {
    /// A page as `super::encode` writes it, streamed as it's made; `kept: false` for an answer
    /// that couldn't be kept (a 500), which the asker serves once.
    Page { kept: bool, made: bool, body: ResponseBody },
    None { rendering: bool, lost: bool },
    NotFound,
    Failed,
    /// Home doesn't know that deployment or function yet (its table is behind): the asker does it.
    Unknown,
}

/// Home's answer, as the asker gets it.
pub enum Reply {
    Page { kept: bool, made: bool, meta: Meta, headers: HeaderMap, body: ResponseBody },
    /// None there; being made (and pushed when it's kept) if the ask said to render it.
    None { lost: bool },
    NotFound,
    Failed,
    Unknown,
}

/// The renderer home answers `Ask`s with.
pub type Renders = Arc<dyn Fn(Ask) -> Pin<Box<dyn Future<Output = Answer> + Send>> + Send + Sync>;

/// How this router runs another's request for a deployment whose version is here (`relay.rs`):
/// a request that names one (`relay::RUN_HEADER`), answered as its worker answered.
pub type Runs = Arc<dyn Fn(Request<Incoming>) -> Pin<Box<dyn Future<Output = Response<ResponseBody>> + Send>> + Send + Sync>;

/// One subscription to this router's pages.
struct Sub {
    from: String,
    tx: mpsc::Sender<Bytes>,
}

/// An edge's open connection to home, with this TLS session's proofs.
#[derive(Clone)]
struct Conn {
    send: SendRequest<Full<Bytes>>,
    ours: hyper::header::HeaderValue,
    theirs: String,
}

pub struct Link {
    /// This router's region, and home's.
    pub me: String,
    pub home: String,
    /// This process in the link: its region and process id (two processes on one router during a
    /// restart are two routers here).
    pub id: String,
    /// Every router of the install by region (`routers.rs`), this one among them; updated as
    /// routers come and go (`set_peers`).
    peers: Mutex<BTreeMap<String, String>>,
    key: hmac::Key,
    acceptor: TlsAcceptor,
    connector: TlsConnector,
    subs: Mutex<Vec<Sub>>,
    to_home: Mutex<Option<Conn>>,
    /// The newest page heard of from home, to be saved as the store's `home_since_ms`.
    heard_ms: AtomicU64,
    saved_ms: AtomicU64,
    synced: AtomicBool,
    /// Purges made at this edge while home couldn't be reached: sent when the link is back.
    pending_tags: Mutex<Vec<TagMark>>,
}

impl Link {
    pub fn new(me: String, home: String, peers: BTreeMap<String, String>, key: &[u8]) -> Result<Link, BoxError> {
        let tls = crate::tls::self_signed()?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth();
        // Its own on every router: home pushes a router's purges to every other one but the one
        // with the sender's id (several routers in a region can have one PID).
        let id = format!("{me}-{}", forkrail_common::random_hex(6));
        Ok(Link {
            me,
            home,
            id,
            peers: Mutex::new(peers),
            key: hmac::Key::new(hmac::HMAC_SHA256, key),
            acceptor: tls.acceptor,
            connector: TlsConnector::from(Arc::new(client)),
            subs: Mutex::default(),
            to_home: Mutex::default(),
            heard_ms: AtomicU64::new(0),
            saved_ms: AtomicU64::new(0),
            synced: AtomicBool::new(false),
            pending_tags: Mutex::default(),
        })
    }

    pub fn is_home(&self) -> bool {
        self.me == self.home
    }

    /// An edge with its connection to home open: its pages come from there.
    pub fn up(&self) -> bool {
        !self.is_home() && self.to_home.lock().unwrap().is_some()
    }

    /// The routers as `routers.rs` knows them now.
    pub fn set_peers(&self, peers: BTreeMap<String, String>) {
        *self.peers.lock().unwrap() = peers;
    }

    /// The other routers, home first.
    pub(crate) fn others(&self) -> Vec<(String, String)> {
        let mut others: Vec<(String, String)> = self.peers.lock().unwrap().iter().filter(|(r, _)| **r != self.me).map(|(r, a)| (r.clone(), a.clone())).collect();
        others.sort_by_key(|(r, _)| *r != self.home);
        others
    }

    fn proofs(&self, exporter: &[u8]) -> (String, String) {
        let proof = |side: &[u8]| forkrail_common::hex(hmac::sign(&self.key, &[side, exporter].concat()).as_ref());
        (proof(b"client"), proof(b"server"))
    }

    // ---- Pushes ----

    /// Send a frame to every router following this one's pages but `except` (the one whose request
    /// it answered already). One that can't keep up is cut off, and asks again when it reconnects.
    fn push(&self, frame: Bytes, except: Option<&str>) {
        self.subs.lock().unwrap().retain(|sub| {
            if except == Some(sub.from.as_str()) {
                return true;
            }
            match sub.tx.try_send(frame.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(router = %sub.from, "a router following the pages fell behind: cut off, it catches up when it connects again");
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }

    /// A page home has just kept, to every edge.
    pub fn push_page(&self, encoded: Bytes, except: Option<&str>) {
        // One an edge couldn't take would cut its link at the same page every time it caught up.
        if encoded.len() > MAX_FRAME {
            return;
        }
        if self.is_home() && !self.subs.lock().unwrap().is_empty() {
            self.push(frame(PAGE, &encoded), except);
        }
    }

    /// A group revalidated on demand at home.
    pub fn push_group(&self, deployment: &str, group: u64, ms: u64) {
        if self.is_home() {
            let payload = serde_json::to_vec(&(deployment, group, ms)).expect("plain");
            self.push(frame(GROUP, &payload), None);
        }
    }

    /// Purge marks home has kept, to every edge but `except` (the one that sent them).
    pub fn push_tags(&self, marks: &[TagMark], except: Option<&str>) {
        if self.is_home() && !self.subs.lock().unwrap().is_empty() {
            for chunk in marks.chunks(TAGS_PER_FRAME) {
                self.push(frame(TAGS, &serde_json::to_vec(chunk).expect("plain")), except);
            }
        }
    }

    /// Send a purge's marks to home (an edge); kept to send again when the link is back if home
    /// can't take them now.
    pub async fn send_tags(&self, marks: Vec<TagMark>) {
        if self.is_home() || marks.is_empty() {
            return;
        }
        if let Err(e) = self.post_tags(&marks).await {
            warn!(home = %self.home, error = %e, marks = marks.len(), "home couldn't take a purge: kept here, sent when the link is back");
            self.pending_tags.lock().unwrap().extend(marks);
        }
    }

    async fn post_tags(&self, marks: &[TagMark]) -> Result<(), String> {
        let conn = self.to_home.lock().unwrap().clone().ok_or("not connected to home")?;
        let mut send = conn.send.clone();
        let body = serde_json::to_vec(&(self.id.as_str(), marks)).expect("plain");
        let request = Request::post("/tags").header(AUTH_HEADER, conn.ours.clone()).body(Full::new(Bytes::from(body))).map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(TAGS_TIMEOUT, async {
            send.ready().await?;
            send.send_request(request).await
        })
        .await
        .map_err(|_| "home didn't answer in time".to_owned())?
        .map_err(|e| e.to_string())?;
        if !proven(response.headers(), &conn.theirs) {
            return Err("home's answer wasn't proven".into());
        }
        if response.status() != StatusCode::OK {
            return Err(format!("home answered {}", response.status()));
        }
        Ok(())
    }

    /// A stale Next.js entry is being made again, to every edge but `except`: their readers are
    /// served it meanwhile rather than making it too.
    pub fn push_lease(&self, hash: &str, until_ms: u64, except: Option<&str>) {
        if self.is_home() && !self.subs.lock().unwrap().is_empty() {
            self.push(frame(LEASE, &serde_json::to_vec(&(hash, until_ms)).expect("plain")), except);
        }
    }

    /// An edge's worker is making a stale Next.js entry again: home is told, which tells the
    /// others. Not waited for: at worst another region makes it once too.
    pub async fn send_lease(&self, hash: String, until_ms: u64) {
        if self.is_home() {
            return;
        }
        let Some(conn) = self.to_home.lock().unwrap().clone() else { return };
        let mut send = conn.send.clone();
        let body = serde_json::to_vec(&(self.id.as_str(), hash, until_ms)).expect("plain");
        let Ok(request) = Request::post("/lease").header(AUTH_HEADER, conn.ours.clone()).body(Full::new(Bytes::from(body))) else { return };
        let _ = tokio::time::timeout(TAGS_TIMEOUT, async {
            send.ready().await?;
            send.send_request(request).await
        })
        .await;
    }

    /// Send a page an edge's own Next.js worker wrote to home (`next.rs`), which keeps it if it's
    /// the newest and pushes it to the other routers. `Err` when home can't take it now: the edge
    /// has it, and home gets the next one.
    pub async fn send_entry(&self, encoded: Bytes) -> Result<(), String> {
        let conn = self.to_home.lock().unwrap().clone().ok_or("not connected to home")?;
        let mut send = conn.send.clone();
        let request = Request::post(format!("/entry?from={}", self.id)).header(AUTH_HEADER, conn.ours.clone()).body(Full::new(encoded)).map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(TAGS_TIMEOUT, async {
            send.ready().await?;
            send.send_request(request).await
        })
        .await
        .map_err(|_| "home didn't answer in time".to_owned())?
        .map_err(|e| e.to_string())?;
        if !proven(response.headers(), &conn.theirs) {
            return Err("home's answer wasn't proven".into());
        }
        if response.status() != StatusCode::OK {
            return Err(format!("home answered {}", response.status()));
        }
        Ok(())
    }

    /// Purges kept while home was away, sent now it's back.
    async fn send_pending_tags(&self) {
        let pending = std::mem::take(&mut *self.pending_tags.lock().unwrap());
        if pending.is_empty() {
            return;
        }
        match self.post_tags(&pending).await {
            Ok(()) => info!(marks = pending.len(), "purges made while home was away sent to it"),
            Err(e) => {
                warn!(error = %e, marks = pending.len(), "home couldn't take the purges made while it was away; tried again when the link is back");
                self.pending_tags.lock().unwrap().extend(pending);
            }
        }
    }

    // ---- Asking home ----

    /// Ask home, if this edge is connected to it. `Err` when it can't be asked or didn't answer
    /// within `limit`: the asker does it itself.
    pub async fn ask(&self, ask: &Ask, limit: Duration) -> Result<Reply, String> {
        let conn = self.to_home.lock().unwrap().clone().ok_or("not connected to home")?;
        let mut send = conn.send.clone();
        let request = Request::post("/page").header(AUTH_HEADER, conn.ours.clone()).body(Full::new(ask.encode())).map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(limit, async {
            send.ready().await?;
            send.send_request(request).await
        })
        .await
        .map_err(|_| "home didn't answer in time".to_owned())?
        .map_err(|e| e.to_string())?;
        if !proven(response.headers(), &conn.theirs) {
            return Err("home's answer wasn't proven".into());
        }
        let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
        let (reply, lost, made) = (header(REPLY_HEADER), header(LOST_HEADER) == "1", header(MADE_HEADER) == "1");
        Ok(match reply.as_str() {
            "kept" | "unkept" => {
                let (meta, headers, body) = tokio::time::timeout(limit, read_head(response.into_body())).await.map_err(|_| "home's page didn't come in time".to_owned())??;
                Reply::Page { kept: reply == "kept", made, meta, headers, body }
            }
            "none" | "rendering" => Reply::None { lost },
            "not-found" => Reply::NotFound,
            "failed" => Reply::Failed,
            "unknown" => Reply::Unknown,
            other => return Err(format!("home answered {other:?}")),
        })
    }

    async fn connect(&self, addr: &str) -> Result<Conn, String> {
        let (send, ours, theirs, _closed) = self.connect_as(addr).await?;
        Ok(Conn { send, ours, theirs })
    }

    /// A connection to the router at `addr` for requests with bodies of type `B`: what goes on it
    /// carries `ours`, and what comes back must carry `theirs` (`proven`). It runs in a task of its
    /// own, which ends when the connection does.
    pub(crate) async fn connect_as<B>(&self, addr: &str) -> Result<(SendRequest<B>, hyper::header::HeaderValue, String, tokio::task::JoinHandle<()>), String>
    where
        B: Body + Send + Unpin + 'static,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await.map_err(|_| "timed out".to_owned())?.map_err(|e| e.to_string())?;
        let _ = tcp.set_nodelay(true);
        let name = ServerName::try_from(SERVER_NAME).expect("a valid name");
        let tls = tokio::time::timeout(CONNECT_TIMEOUT, self.connector.connect(name, tcp)).await.map_err(|_| "TLS timed out".to_owned())?.map_err(|e| e.to_string())?;
        let exporter = tls.get_ref().1.export_keying_material([0u8; 32], EXPORTER_LABEL, None).map_err(|e| e.to_string())?;
        let (ours, theirs) = self.proofs(&exporter);
        let (send, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .keep_alive_interval(BEAT_EVERY)
            .keep_alive_timeout(BEAT_EVERY)
            .keep_alive_while_idle(true)
            .initial_stream_window_size(STREAM_WINDOW)
            .initial_connection_window_size(CONNECTION_WINDOW)
            .handshake(TokioIo::new(tls))
            .await
            .map_err(|e| e.to_string())?;
        let closed = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok((send, ours.parse().expect("hex"), theirs, closed))
    }

    // ---- Following home, and joining ----

    /// What this router does with its peers for good: home copies the pages from another router if
    /// its store is new; an edge follows home (copying everything first if its store is new, else
    /// what was made since it last heard), and if home can't be reached while its store is new,
    /// copies from another router. A new store nobody could copy to is lost (`ResponseCache::lost`).
    pub async fn run(self: Arc<Self>, cache: Arc<ResponseCache>, deployments: impl Fn() -> Vec<String> + Send + 'static) {
        let mut copy = cache.store_is_new();
        if self.is_home() {
            if copy && !self.copy_from_others(&cache).await {
                cache.mark_lost(deployments());
            }
            return;
        }
        let mut backoff = Duration::from_secs(1);
        let mut tried_others = false;
        let mut said_down = false;
        loop {
            // Where home is now: a router moved or heard of later is followed from the next try.
            let home = self.peers.lock().unwrap().get(&self.home).cloned();
            let connected = match &home {
                Some(addr) => self.connect(addr).await,
                None => Err("no router is known in the home region".to_owned()),
            };
            match connected {
                Ok(conn) => {
                    backoff = Duration::from_secs(1);
                    *self.to_home.lock().unwrap() = Some(conn.clone());
                    let link = self.clone();
                    tokio::spawn(async move { link.send_pending_tags().await });
                    let since = if copy { 0 } else { cache.home_since().saturating_sub(SINCE_MARGIN_MS) };
                    let outcome = self.follow(&conn, since, false, &cache).await;
                    *self.to_home.lock().unwrap() = None;
                    self.save_heard(&cache);
                    if outcome.synced && copy {
                        copy = false;
                        cache.clear_lost();
                    }
                    // Said once for each time it goes down, not on every try while it stays so.
                    if outcome.synced || !said_down {
                        warn!(home = %self.home, reason = %outcome.ended, "the link to home is down: pages are made here until it's back");
                    }
                    said_down = true;
                }
                Err(e) => {
                    if !said_down {
                        warn!(home = %self.home, error = %e, "can't reach home: pages are made here until it answers");
                    }
                    said_down = true;
                    if copy && !tried_others {
                        tried_others = true;
                        if self.copy_from_others(&cache).await {
                            copy = false;
                        } else {
                            cache.mark_lost(deployments());
                        }
                    }
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }

    /// Everything another router has, once. Whether one gave it.
    async fn copy_from_others(&self, cache: &Arc<ResponseCache>) -> bool {
        for (region, addr) in self.others().into_iter().filter(|(r, _)| *r != self.home || self.is_home()) {
            match self.connect(&addr).await {
                Ok(conn) => {
                    let outcome = self.follow(&conn, 0, true, cache).await;
                    if outcome.synced {
                        info!(from = %region, pages = outcome.pages, "pages copied from another router");
                        self.save_heard(cache);
                        return true;
                    }
                }
                Err(e) => warn!(router = %region, error = %e, "can't copy pages from this router"),
            }
        }
        false
    }

    fn save_heard(&self, cache: &ResponseCache) {
        let heard = self.heard_ms.load(Ordering::Relaxed);
        if heard > self.saved_ms.load(Ordering::Relaxed) {
            self.saved_ms.store(heard, Ordering::Relaxed);
            cache.set_home_since(heard);
        }
    }

    /// Read `/subscribe` until it ends: pages and groups into the cache.
    async fn follow(&self, conn: &Conn, since: u64, once: bool, cache: &Arc<ResponseCache>) -> Followed {
        let mut outcome = Followed { synced: false, pages: 0, ended: String::new() };
        let path = format!("/subscribe?since={since}&from={}&once={}", self.id, u8::from(once));
        let request = Request::get(path).header(AUTH_HEADER, conn.ours.clone()).body(Full::default()).expect("a valid request");
        let mut send = conn.send.clone();
        let response = match tokio::time::timeout(CONNECT_TIMEOUT, async {
            send.ready().await?;
            send.send_request(request).await
        })
        .await
        {
            Ok(Ok(r)) if r.status() == StatusCode::OK && proven(r.headers(), &conn.theirs) => r,
            Ok(Ok(r)) => {
                outcome.ended = format!("refused: {}", r.status());
                return outcome;
            }
            Ok(Err(e)) => {
                outcome.ended = e.to_string();
                return outcome;
            }
            Err(_) => {
                outcome.ended = "no answer".into();
                return outcome;
            }
        };
        let started = std::time::Instant::now();
        let mut body = response.into_body();
        let mut buf = BytesMut::new();
        let mut last_save = std::time::Instant::now();
        loop {
            let frame = match tokio::time::timeout(SILENT_FOR, body.frame()).await {
                Err(_) => {
                    outcome.ended = "silent".into();
                    break;
                }
                Ok(None) => {
                    outcome.ended = "ended".into();
                    break;
                }
                Ok(Some(Err(e))) => {
                    outcome.ended = e.to_string();
                    break;
                }
                Ok(Some(Ok(frame))) => frame,
            };
            let Ok(data) = frame.into_data() else { continue };
            buf.extend_from_slice(&data);
            while let Some((kind, payload)) = take_frame(&mut buf) {
                match kind {
                    PAGE => {
                        let Some((hash, entry)) = super::decode(payload) else { continue };
                        let (stored, deployment) = (entry.stored_ms, entry.deployment.clone());
                        outcome.pages += 1;
                        let live = outcome.synced;
                        if cache.receive(&hash, entry) && live {
                            // Push latency: from home keeping it to here having it.
                            info!(%deployment, ms = super::now_ms().saturating_sub(stored), "page pushed from home");
                        }
                        // A time ahead of the clock would move the next catch-up past pages
                        // made before it.
                        if !once {
                            self.heard_ms.fetch_max(stored.min(super::now_ms()), Ordering::Relaxed);
                        }
                    }
                    GROUP => {
                        if let Ok((deployment, group, ms)) = serde_json::from_slice::<(String, u64, u64)>(&payload) {
                            cache.apply_group(&deployment, group, ms);
                        }
                    }
                    TAGS => {
                        if let Ok(marks) = serde_json::from_slice::<Vec<TagMark>>(&payload) {
                            let changed = cache.tags.merge_in(&marks, super::now_ms());
                            if outcome.synced && !changed.is_empty() {
                                // Push latency: from the purge to here having it.
                                let at = changed.iter().map(|m| m.mark.stale_before.max(m.mark.deleted_before).max(m.mark.pending.iter().map(|p| p.0).max().unwrap_or(0))).max().unwrap_or(0);
                                info!(marks = changed.len(), ms = super::now_ms().saturating_sub(at), "purge pushed from home");
                            }
                        }
                    }
                    LEASE => {
                        if let Ok((hash, until)) = serde_json::from_slice::<(String, u64)>(&payload) {
                            cache.lease_heard(&hash, until, None);
                        }
                    }
                    SYNCED => {
                        outcome.synced = true;
                        if !once {
                            info!(home = %self.home, "following home's pages");
                        }
                        self.synced.store(true, Ordering::Relaxed);
                        info!(pages = outcome.pages, ms = started.elapsed().as_millis() as u64, since, "caught up with the pages");
                        if once {
                            return outcome;
                        }
                        self.save_heard(cache);
                    }
                    _ => {}
                }
            }
            if buf.len() > MAX_FRAME + 5 {
                outcome.ended = "a frame too large".into();
                break;
            }
            if last_save.elapsed() > Duration::from_secs(60) {
                last_save = std::time::Instant::now();
                self.save_heard(cache);
            }
        }
        outcome
    }

    // ---- Serving other routers ----

    /// Answer other routers on `listener`: `/subscribe` (every router: its pages, then home's
    /// pushes), `/page` (home: `Ask`s, answered by `renders`), and requests to run on a deployment
    /// whose version is here (any router: `runs`). Runs for good.
    pub async fn serve(self: Arc<Self>, listener: TcpListener, cache: Arc<ResponseCache>, renders: Renders, runs: Runs) {
        loop {
            let (tcp, addr) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    warn!(error = %e, "peer accept failed");
                    continue;
                }
            };
            let (link, cache, renders, runs) = (self.clone(), cache.clone(), renders.clone(), runs.clone());
            tokio::spawn(async move {
                let _ = tcp.set_nodelay(true);
                let tls = match tokio::time::timeout(CONNECT_TIMEOUT, link.acceptor.accept(tcp)).await {
                    Ok(Ok(tls)) => tls,
                    _ => return,
                };
                let Ok(exporter) = tls.get_ref().1.export_keying_material([0u8; 32], EXPORTER_LABEL, None) else { return };
                let (theirs, ours) = link.proofs(&exporter);
                let refused = Arc::new(AtomicBool::new(false));
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let (link, cache, renders, runs, theirs, ours, refused) =
                        (link.clone(), cache.clone(), renders.clone(), runs.clone(), theirs.clone(), ours.clone(), refused.clone());
                    async move {
                        if !proven(request.headers(), &theirs) {
                            if !refused.swap(true, Ordering::Relaxed) {
                                warn!(%addr, "a connection to the peer port without the install's key: refused");
                            }
                            return Ok::<_, std::convert::Infallible>(plain(StatusCode::UNAUTHORIZED));
                        }
                        let mut response = match request.headers().contains_key(crate::relay::RUN_HEADER) {
                            true => runs(request).await,
                            false => link.answer(request, &cache, &renders).await,
                        };
                        response.headers_mut().insert(AUTH_HEADER, ours.parse().expect("hex"));
                        Ok(response)
                    }
                });
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .timer(TokioTimer::new())
                    .keep_alive_interval(BEAT_EVERY)
                    .keep_alive_timeout(BEAT_EVERY)
                    // Every request the other router runs here goes on its one connection.
                    .max_concurrent_streams(RUN_STREAMS)
                    .initial_stream_window_size(STREAM_WINDOW)
                    .initial_connection_window_size(CONNECTION_WINDOW)
                    .serve_connection(TokioIo::new(tls), service)
                    .await;
            });
        }
    }

    async fn answer(self: &Arc<Self>, request: Request<Incoming>, cache: &Arc<ResponseCache>, renders: &Renders) -> Response<ResponseBody> {
        match (request.method().clone(), request.uri().path()) {
            (Method::GET, "/subscribe") => {
                let query = request.uri().query().unwrap_or("");
                let param = |name: &str| query.split('&').find_map(|p| p.strip_prefix(name)?.strip_prefix('=')).unwrap_or("").to_owned();
                let since = param("since").parse().unwrap_or(0);
                let once = param("once") == "1";
                let from = param("from");
                let mut response = Response::new(self.subscription(cache, since, &from, once));
                *response.status_mut() = StatusCode::OK;
                response
            }
            (Method::POST, "/tags") if self.is_home() => {
                let Ok(collected) = request.into_body().collect().await else { return plain(StatusCode::BAD_REQUEST) };
                let Ok((from, marks)) = serde_json::from_slice::<(String, Vec<TagMark>)>(&collected.to_bytes()) else { return plain(StatusCode::BAD_REQUEST) };
                let changed = cache.take_marks(&marks, Some(&from));
                info!(from = %from, marks = marks.len(), changed = changed.len(), "purge from another router");
                plain(StatusCode::OK)
            }
            (Method::POST, "/lease") if self.is_home() => {
                let Ok(collected) = request.into_body().collect().await else { return plain(StatusCode::BAD_REQUEST) };
                let Ok((from, hash, until)) = serde_json::from_slice::<(String, String, u64)>(&collected.to_bytes()) else { return plain(StatusCode::BAD_REQUEST) };
                cache.lease_heard(&hash, until, Some(&from));
                plain(StatusCode::OK)
            }
            (Method::POST, "/entry") if self.is_home() => {
                let from = request.uri().query().and_then(|q| q.split('&').find_map(|p| p.strip_prefix("from="))).unwrap_or("").to_owned();
                let Ok(collected) = request.into_body().collect().await else { return plain(StatusCode::BAD_REQUEST) };
                let bytes = collected.to_bytes();
                if bytes.len() > MAX_FRAME {
                    return plain(StatusCode::PAYLOAD_TOO_LARGE);
                }
                // Only Next.js's entries come this way: C16's pages are made at home.
                let Some((hash, entry)) = super::decode(bytes).filter(|(_, e)| e.headers.contains_key(forkrail_common::nextcache::MARK_HEADER)) else { return plain(StatusCode::BAD_REQUEST) };
                cache.receive_from(&hash, entry, Some(&from).filter(|f| !f.is_empty()).map(String::as_str));
                plain(StatusCode::OK)
            }
            (Method::POST, "/page") if self.is_home() => {
                let Ok(collected) = request.into_body().collect().await else { return plain(StatusCode::BAD_REQUEST) };
                let Some(ask) = Ask::decode(&collected.to_bytes()) else { return plain(StatusCode::BAD_REQUEST) };
                let answer = renders(ask).await;
                let (reply, kept_body) = match answer {
                    Answer::Page { kept, made, body } => ((if kept { "kept" } else { "unkept" }, false, made), Some(body)),
                    Answer::None { rendering, lost } => ((if rendering { "rendering" } else { "none" }, lost, false), None),
                    Answer::NotFound => (("not-found", false, false), None),
                    Answer::Failed => (("failed", false, false), None),
                    Answer::Unknown => (("unknown", false, false), None),
                };
                let mut response = Response::new(kept_body.unwrap_or_else(|| Full::new(Bytes::new()).map_err(|never| match never {}).boxed()));
                let headers = response.headers_mut();
                headers.insert(REPLY_HEADER, reply.0.parse().expect("a word"));
                if reply.1 {
                    headers.insert(LOST_HEADER, "1".parse().expect("a word"));
                }
                if reply.2 {
                    headers.insert(MADE_HEADER, "1".parse().expect("a word"));
                }
                response
            }
            _ => plain(StatusCode::NOT_FOUND),
        }
    }

    /// `/subscribe`'s body: the groups and every page made after `since`, then (home, unless
    /// `once`) each page as it's kept, with a beat when there's nothing.
    fn subscription(self: &Arc<Self>, cache: &Arc<ResponseCache>, since: u64, from: &str, once: bool) -> ResponseBody {
        let (sync_tx, sync_rx) = mpsc::channel::<Bytes>(8);
        let live = (!once && self.is_home()).then(|| {
            let (tx, rx) = mpsc::channel(QUEUE);
            self.subs.lock().unwrap().push(Sub { from: from.to_owned(), tx });
            rx
        });
        let cache = cache.clone();
        tokio::spawn(async move {
            for chunk in cache.tags.all().chunks(TAGS_PER_FRAME) {
                if sync_tx.send(frame(TAGS, &serde_json::to_vec(chunk).expect("plain"))).await.is_err() {
                    return;
                }
            }
            for (deployment, group, ms) in cache.groups() {
                let payload = serde_json::to_vec(&(deployment, group, ms)).expect("plain");
                if sync_tx.send(frame(GROUP, &payload)).await.is_err() {
                    return;
                }
            }
            for (deployment, hash) in cache.pages_since(since) {
                if let Some(bytes) = cache.page_bytes(&deployment, &hash).await
                    && bytes.len() <= MAX_FRAME
                    && sync_tx.send(frame(PAGE, &bytes)).await.is_err()
                {
                    return;
                }
            }
            let _ = sync_tx.send(frame(SYNCED, &[])).await;
        });
        Frames { sync: Some(sync_rx), live, beat: tokio::time::interval_at(tokio::time::Instant::now() + BEAT_EVERY, BEAT_EVERY) }.boxed()
    }
}

struct Followed {
    synced: bool,
    pages: usize,
    ended: String,
}

/// Whether a request or answer carries the proof expected on this TLS session.
pub(crate) fn proven(headers: &HeaderMap, expected: &str) -> bool {
    let given = headers.get(AUTH_HEADER).map(|v| v.as_bytes()).unwrap_or_default();
    given.len() == expected.len() && given.iter().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn plain(status: StatusCode) -> Response<ResponseBody> {
    let mut response = Response::new(Full::new(Bytes::new()).map_err(|never| match never {}).boxed());
    *response.status_mut() = status;
    response
}

fn frame(kind: u8, payload: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(5 + payload.len());
    out.put_u8(kind);
    out.put_u32(payload.len() as u32);
    out.extend_from_slice(payload);
    out.freeze()
}

/// The next whole frame in `buf`, taken out of it.
fn take_frame(buf: &mut BytesMut) -> Option<(u8, Bytes)> {
    if buf.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return None;
    }
    let kind = buf[0];
    buf.advance(5);
    Some((kind, buf.split_to(len).freeze()))
}

/// A page's format line, meta and headers from the start of a streamed answer, and its body as
/// the rest comes.
async fn read_head(mut body: Incoming) -> Result<(Meta, HeaderMap, ResponseBody), String> {
    let mut buf = BytesMut::new();
    loop {
        if let Some((meta, headers, at)) = super::decode_head(&buf) {
            let first = buf.split_off(at).freeze();
            return Ok((meta, headers, Arriving { first: Some(first), rest: body }.boxed()));
        }
        if buf.len() > 1 << 20 {
            return Err("a page's head over 1 MB".into());
        }
        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    buf.extend_from_slice(&data);
                }
            }
            Some(Err(e)) => return Err(e.to_string()),
            None => return Err("the page ended before its head".into()),
        }
    }
}

/// A body's first bytes, already read, then the rest.
struct Arriving {
    first: Option<Bytes>,
    rest: Incoming,
}

impl Body for Arriving {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some(first) = self.first.take()
            && !first.is_empty()
        {
            return Poll::Ready(Some(Ok(Frame::data(first))));
        }
        Pin::new(&mut self.rest).poll_frame(cx).map_err(Into::into)
    }

    fn is_end_stream(&self) -> bool {
        self.first.as_ref().is_none_or(Bytes::is_empty) && self.rest.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = self.rest.size_hint();
        let first = self.first.as_ref().map_or(0, |b| b.len() as u64);
        hint.set_lower(hint.lower() + first);
        if let Some(upper) = hint.upper() {
            hint.set_upper(upper + first);
        }
        hint
    }
}

/// `/subscribe`'s frames: what was there first, then what's pushed, with beats between.
struct Frames {
    sync: Option<mpsc::Receiver<Bytes>>,
    live: Option<mpsc::Receiver<Bytes>>,
    beat: tokio::time::Interval,
}

impl Body for Frames {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        if let Some(sync) = this.sync.as_mut() {
            match ready!(sync.poll_recv(cx)) {
                Some(bytes) => return Poll::Ready(Some(Ok(Frame::data(bytes)))),
                None => this.sync = None,
            }
        }
        let Some(live) = this.live.as_mut() else { return Poll::Ready(None) };
        if let Poll::Ready(found) = live.poll_recv(cx) {
            return Poll::Ready(found.map(|bytes| Ok(Frame::data(bytes))));
        }
        ready!(this.beat.poll_tick(cx));
        Poll::Ready(Some(Ok(Frame::data(frame(BEAT, &[])))))
    }
}

/// Neither side reads the other's certificate: the key's proofs over the session authenticate
/// both. Signatures are still checked, so the server holds the key of the certificate it sent.
#[derive(Debug)]
struct AnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A page from `Entry` as home sends it: what `super::encode` writes.
pub fn page_body(encoded: Bytes) -> ResponseBody {
    Full::new(encoded).map_err(|never| match never {}).boxed()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn peers_and_frames() {
        let peers = parse_peers("us-east-1=1.2.3.4:7445, us-west-1=5.6.7.8:7445").unwrap();
        assert_eq!(peers["us-west-1"], "5.6.7.8:7445");
        assert!(parse_peers("us-east-1").is_err() && parse_peers("x y=1.2.3.4:1").is_err() && parse_peers("a=host").is_err());
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&frame(PAGE, b"abc"));
        buf.extend_from_slice(&frame(BEAT, b"")[..3]);
        assert_eq!(take_frame(&mut buf), Some((PAGE, Bytes::from_static(b"abc"))));
        assert_eq!(take_frame(&mut buf), None, "half a frame waits for the rest");
        buf.extend_from_slice(&frame(BEAT, b"")[3..]);
        assert_eq!(take_frame(&mut buf), Some((BEAT, Bytes::new())));
    }

    #[test]
    fn asks_round_trip() {
        let mut headers = HeaderMap::new();
        headers.insert("x-now-route-matches", "slug=a%20b".parse().unwrap());
        headers.append("cookie", "a=1".parse().unwrap());
        let ask = Ask {
            op: Op::Refresh,
            host: "app.example.com".into(),
            deployment: "dep".into(),
            function: "/blog/[slug]".into(),
            max_duration_ms: Some(10_000),
            hash: "h".into(),
            have: Some(5),
            wait: false,
            render: false,
            if_generated: false,
            uri: "/blog/a?x=1".into(),
            headers: headers.clone(),
            request_id: "r".into(),
            from: "us-west-1-1".into(),
        };
        let back = Ask::decode(&ask.encode()).unwrap();
        assert_eq!((back.op, back.have, back.uri.as_str(), back.headers), (Op::Refresh, Some(5), "/blog/a?x=1", headers));
    }

    fn page(stored_ms: u64) -> super::super::Entry {
        super::super::Entry {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(format!("made at {stored_ms}")),
            stored_ms,
            fresh_until: None,
            stale_until: None,
            deployment: "dep".into(),
            group: Some(1),
            page: true,
            local: false,
        }
    }

    /// A router that runs nothing for others.
    pub(crate) fn no_runs() -> Runs {
        Arc::new(|_| Box::pin(async { crate::relay::refusal("unknown") }))
    }

    /// A request run over the link (`relay.rs`): it reaches `runs` with the link's proof and the
    /// deployment's name, its answer comes back proven, and a router without the key gets nothing.
    #[tokio::test]
    async fn a_request_runs_on_the_other_router() {
        use crate::proxy::ProxyBody;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let key = [7u8; 32];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let peers: BTreeMap<String, String> = [("us-east-1".to_owned(), addr.clone()), ("us-west-1".to_owned(), "127.0.0.1:9".to_owned())].into();
        let east = Arc::new(Link::new("us-east-1".into(), "us-east-1".into(), peers.clone(), &key).unwrap());
        let cache = Arc::new(ResponseCache::new(1 << 20, 1 << 20, None, Some(east.clone()), true));
        let renders: Renders = Arc::new(|_: Ask| Box::pin(async { Answer::Unknown }));
        // Says what it was asked, and echoes the body, as a worker's answer.
        let runs: Runs = Arc::new(|request: Request<Incoming>| {
            Box::pin(async move {
                let (mut parts, body) = request.into_parts();
                let (deployment, alone, max) = crate::relay::take_run(&mut parts.headers);
                let body = body.collect().await.unwrap().to_bytes();
                let mut response = Response::new(Full::new(body).map_err(|never| match never {}).boxed());
                let said = format!("{}|{}|{:?}|{}|{}", deployment.unwrap_or_default(), alone, max, parts.uri.path(), parts.headers.contains_key(AUTH_HEADER));
                response.headers_mut().insert("x-said", said.parse().unwrap());
                crate::relay::ran(response.headers_mut(), &crate::relay::Ran { start: "thaw", queue: Duration::from_millis(17), mem_kb: None, worker: "dpl.ab".into() });
                crate::relay::counted(response)
            })
        });
        tokio::spawn(east.clone().serve(listener, cache, renders, runs));

        let west = Link::new("us-west-1".into(), "us-east-1".into(), peers.clone(), &key).unwrap();
        assert_eq!(west.others(), [("us-east-1".to_owned(), addr.clone())]);
        let (mut send, ours, theirs, _closed) = west.connect_as::<ProxyBody>(&addr).await.unwrap();
        let proofs = crate::relay::Proofs { ours, theirs };
        let mut request = Request::post("http://api.example.com/v1/search?q=1").body(Full::new(Bytes::from_static(b"{\"a\":1}")).map_err(|never| match never {}).boxed()).unwrap();
        crate::relay::mark(&mut request, &proofs, "dpl", Some(Duration::from_secs(60)), true);
        let mut response = send.send_request(request).await.unwrap();
        assert!(crate::relay::proven(&proofs, response.headers()), "the answer is the other router's");
        assert_eq!(crate::relay::refused(response.headers()), None);
        assert_eq!(response.headers()["x-said"], "dpl|true|Some(60s)|/v1/search|false", "named, alone, its maxDuration, and no proof left for the worker");
        crate::relay::take_answer(response.headers_mut());
        let ran = crate::relay::take_ran(response.headers_mut()).unwrap();
        assert_eq!((ran.start, ran.worker.as_str()), ("thaw", "dpl.ab"));
        assert!(!response.headers().contains_key(AUTH_HEADER));
        assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), Bytes::from_static(b"{\"a\":1}"));

        // Without the key, nothing runs.
        let stranger = Link::new("us-west-1".into(), "us-east-1".into(), peers, &[8u8; 32]).unwrap();
        let (mut send, ours, theirs, _closed) = stranger.connect_as::<ProxyBody>(&addr).await.unwrap();
        let mut request = Request::get("http://api.example.com/").body(Full::new(Bytes::new()).map_err(|never| match never {}).boxed()).unwrap();
        crate::relay::mark(&mut request, &crate::relay::Proofs { ours, theirs }, "dpl", None, false);
        let response = send.send_request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!response.headers().contains_key("x-said"));
    }

    async fn until(what: impl Fn() -> bool) -> bool {
        for _ in 0..400 {
            if what() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// Two routers in one process over TLS on loopback: an edge joins (copying what home had),
    /// gets home's pages pushed (not those its own asks brought), asks home, and a router without
    /// the key gets nothing.
    #[tokio::test]
    async fn an_edge_joins_follows_and_asks_home() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let key = [5u8; 32];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let peers: BTreeMap<String, String> = [("us-east-1".to_owned(), addr.clone()), ("us-west-1".to_owned(), "127.0.0.1:9".to_owned())].into();
        let home_link = Arc::new(Link::new("us-east-1".into(), "us-east-1".into(), peers.clone(), &key).unwrap());
        let home = Arc::new(ResponseCache::new(1 << 20, 1 << 20, None, Some(home_link.clone()), true));
        home.put("before", page(10), None);
        home.apply_group("dep", 1, 5);
        let joined = forkrail_common::purge::Purge { op: forkrail_common::purge::PurgeOp::Delete, tags: vec!["old".into()], src_images: false, target: None, deadline_s: None, at_ms: super::super::now_ms(), by: None };
        home.take_marks(&super::super::TagMarks::marks_of("shop", &joined), None);
        let renders: Renders = Arc::new(|ask: Ask| Box::pin(async move { Answer::None { rendering: ask.render, lost: ask.render } }));
        tokio::spawn(home_link.clone().serve(listener, home.clone(), renders, no_runs()));

        let edge_link = Arc::new(Link::new("us-west-1".into(), "us-east-1".into(), peers.clone(), &key).unwrap());
        let edge = Arc::new(ResponseCache::new(1 << 20, 1 << 20, None, Some(edge_link.clone()), true));
        tokio::spawn(edge_link.clone().run(edge.clone(), Vec::new));
        let has = |cache: &Arc<ResponseCache>, hash: &'static str, stored: u64| {
            let cache = cache.clone();
            move || cache.current(hash).is_some_and(|(s, _)| s == stored)
        };
        assert!(until(has(&edge, "before", 10)).await, "joined: what home had is copied");
        assert!(until(|| edge.groups() == [("dep".to_owned(), 1, 5)]).await, "and its groups");
        assert_eq!(edge.tags.all().len(), 2, "and its purges' marks (both environments)");
        assert!(edge_link.up());

        // Kept at home: pushed.
        home.put("new", page(20), None);
        assert!(until(has(&edge, "new", 20)).await, "pushed");
        // Kept for the edge's own ask (it has the page in the answer): not pushed again.
        home.put("new", page(30), Some(&edge_link.id));
        home.put("other", page(31), None);
        assert!(until(has(&edge, "other", 31)).await);
        assert!(has(&edge, "new", 20)(), "not pushed back to the router that asked");
        home.revalidated("dep", Some(1), 40);
        assert!(until(|| edge.groups() == [("dep".to_owned(), 1, 40)]).await, "groups revalidated on demand are pushed");

        // Purges by tag: home's are pushed; an edge's go to home, which pushes them to the rest.
        let purge = |tag: &str, at_ms| forkrail_common::purge::Purge { op: forkrail_common::purge::PurgeOp::Invalidate, tags: vec![tag.to_owned()], src_images: false, target: Some("production".into()), deadline_s: None, at_ms, by: None };
        let mut tagged = HeaderMap::new();
        tagged.insert("vercel-cache-tag", "a,b".parse().unwrap());
        let stale = |cache: &Arc<ResponseCache>, tagged: &HeaderMap| cache.tags.judge("shop", "production", tagged, 50, 1_000) == Some(super::super::Verdict::Stale);
        home.take_marks(&super::super::TagMarks::marks_of("shop", &purge("a", 100)), None);
        assert!(until(|| stale(&edge, &tagged)).await, "a purge at home is pushed");
        let other = {
            let mut h = HeaderMap::new();
            h.insert("vercel-cache-tag", "c".parse().unwrap());
            h
        };
        let marks = super::super::TagMarks::marks_of("shop", &purge("c", 100));
        edge.take_marks(&marks, None);
        edge_link.send_tags(marks).await;
        assert!(stale(&home, &other), "an edge's purge reaches home before it's done");

        // Asking home.
        let ask = Ask {
            op: Op::Fetch,
            host: "h".into(),
            deployment: "dep".into(),
            function: "/p".into(),
            max_duration_ms: None,
            hash: "x".into(),
            have: None,
            wait: false,
            render: true,
            if_generated: false,
            uri: "http://h/p".into(),
            headers: HeaderMap::new(),
            request_id: "r".into(),
            from: edge_link.id.clone(),
        };
        assert!(matches!(edge_link.ask(&ask, Duration::from_secs(5)).await, Ok(Reply::None { lost: true })));

        // Without the install's key: refused.
        let stranger = Link::new("us-west-1".into(), "us-east-1".into(), peers, &[6u8; 32]).unwrap();
        let conn = stranger.connect(&addr).await.unwrap();
        let outcome = stranger.follow(&conn, 0, true, &edge).await;
        assert!(!outcome.synced && outcome.ended.contains("401"), "{}", outcome.ended);
    }

    #[test]
    fn proofs_differ_by_side_and_session() {
        let link = Link::new("us-east-1".into(), "us-east-1".into(), BTreeMap::new(), &[7; 32]).unwrap();
        let (client, server) = link.proofs(b"session one");
        assert_ne!(client, server);
        assert_ne!(link.proofs(b"session two").0, client);
        let other = Link::new("us-east-1".into(), "us-east-1".into(), BTreeMap::new(), &[8; 32]).unwrap();
        assert_ne!(other.proofs(b"session one").0, client, "another key");
        let mut headers = HeaderMap::new();
        headers.insert(AUTH_HEADER, client.parse().unwrap());
        assert!(proven(&headers, &client) && !proven(&headers, &server) && !proven(&HeaderMap::new(), &client));
    }
}
