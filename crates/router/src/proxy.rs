//! Public HTTP handler: find the deployment for the request's host, read small request bodies in
//! full, lease a worker, forward the request over its tunnel, and relay the response so the worker
//! is released once it has sent everything, however slow the client is.
//!
//! What the router holds of request and response bodies is capped router-wide
//! (`FORKRAIL_BUFFER_TOTAL_BYTES`), on top of the per-request `FORKRAIL_BUFFER_BYTES`: past it,
//! bodies wait for room. Request bodies take room as their bytes arrive, not by their declared
//! length, so a client that announces a large body and sends nothing holds nothing.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use forkrail_common::routes::normalize_host;
use forkrail_common::schedules::SCHEDULE_HEADER;
use forkrail_common::{
    APP_ERROR_HEADER, BUSY_HEADER, CPU_HEADER, PROFILE_HEADER, PROFILE_REQUEST_HEADER, RETRY_APP_CONNECTION, RETRY_HEADER,
    ROUTE_HEADER, RSS_HEADER, RSS_PEAK_HEADER,
    RSS_START_HEADER, strip_hop_by_hop,
};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName, HeaderValue, LOCATION, REFERER, RETRY_AFTER, SET_COOKIE};
use hyper::{HeaderMap, Method, Request, Response, StatusCode, Uri, Version};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::geo::{self, Geo, Location};
use forkrail_common::rules::{self, Engine, Files, NoFiles, Outcome, TransformKind};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tracing::{debug, info, warn};

use crate::BoxError;
use crate::logs::LogLimiter;
use crate::pool::{Lease, Pool};
use crate::firewall::{Firewalls, Judged};
use forkrail_common::firewall::{ApiCheck, Decision};
use crate::protection::{self, Access, Guard};
use crate::registry::{Registry, Target};
use crate::statics::{Statics, Unreadable};

/// The response cache's part of a request (`crate::cache`): what's served from it, and how a
/// function's answer is kept.
mod cached;
mod middleware;
mod nextjs;

/// What home answers other routers' asks for pages with (`cache/peers.rs`).
pub fn peer_renders(proxy: Arc<Proxy>) -> crate::cache::peers::Renders {
    cached::peer_renders(proxy)
}

/// How this router runs the requests other routers send it (`relay.rs`).
pub fn peer_runs(proxy: Arc<Proxy>) -> crate::cache::peers::Runs {
    Arc::new(move |request| {
        let proxy = proxy.clone();
        Box::pin(async move { crate::relay::counted(run_for_router(proxy, request).await) })
    })
}

/// A request another router sent to run on one of this router's deployments: on this router's
/// workers for it, as a request of its own, with no line in this router's access log (the one
/// that sent it writes it). Refused, for that router to run it itself, when the deployment's
/// version isn't here either (it's passed on once at most) and while this router is stopping.
async fn run_for_router(proxy: Arc<Proxy>, request: Request<Incoming>) -> Response<ResponseBody> {
    if crate::draining() {
        return crate::relay::refusal("draining");
    }
    let (mut parts, incoming) = request.into_parts();
    let (deployment, alone, max_duration) = crate::relay::take_run(&mut parts.headers);
    let Some(deployment) = deployment else { return crate::relay::refusal("unknown") };
    let host = parts.uri.authority().map(|a| a.as_str().to_owned()).unwrap_or_default();
    let Some(target) = proxy.registry.deployment_target(&normalize_host(&host), &deployment) else { return crate::relay::refusal("unknown") };
    let Some(pool) = target.pool.clone() else { return crate::relay::refusal("unknown") };
    // Its version here is in another region: whoever asked runs it, rather than pass it on again.
    if !pool.config().region.is_empty() && pool.config().region != proxy.registry.here() {
        return crate::relay::refusal("elsewhere");
    }
    let request_id = parts.headers.get(REQUEST_ID).and_then(|v| v.to_str().ok()).map(str::to_owned).unwrap_or_else(forkrail_common::quick_id);
    let head = UpstreamHead { host: host.clone(), method: parts.method, uri: parts.uri, headers: parts.headers, payload: 0 };
    // The size limit was the asking router's to apply.
    let body = match read_body(incoming, &head.headers, &proxy, u64::MAX, Version::HTTP_2).await {
        Ok(body) => body,
        Err(_) => return error(StatusCode::BAD_REQUEST, "could not read request body"),
    };
    let mut log = AccessLog::spent();
    log.written = false;
    log.quiet = true;
    log.request_id = request_id;
    log.entry = Entry::Https;
    log.host = normalize_host(&host);
    log.deployment = Some(deployment);
    log.project = target.project.clone();
    log.region = target.region.clone();
    log.method = head.method.clone();
    log.path = head.uri.path().to_owned();
    log.function = Some(FunctionLog::of(pool.config()));
    log.started = Instant::now();
    log.bytes_in = body.len();
    // Its app's lines are this router's to keep, by the request's id, as for one of its own.
    pool.logs().begin(&log.request_id);
    log.app_logs = Some(pool.logs().clone());
    let effects = Effects { max_duration, for_router: true, run_alone: alone, ..Effects::default() };
    let (client, response) = oneshot::channel();
    tokio::spawn(exchange(head, body, pool, Version::HTTP_2, proxy, log, effects, client));
    response.await.unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "worker failed"))
}

/// What the worker measured and how its request ran, back on its answer for the router that sent
/// the request (`relay.rs`), which takes them off as from a worker of its own (`worker_measures`).
fn for_router(headers: &mut HeaderMap, log: &AccessLog) {
    crate::relay::take_apps(headers);
    let mut put = |name: &'static str, value: Option<String>| {
        if let Some(value) = value.and_then(|v| HeaderValue::from_str(&v).ok()) {
            headers.insert(name, value);
        }
    };
    put(RSS_HEADER, log.rss_kb.map(|v| v.to_string()));
    put(CPU_HEADER, log.cpu_us.map(|v| v.to_string()));
    put(BUSY_HEADER, log.busy_us.map(|v| v.to_string()));
    put(PROFILE_HEADER, log.cpu_profile.clone());
    put(ROUTE_HEADER, log.route.clone());
    let ran = crate::relay::Ran { start: log.start.unwrap_or("warm"), queue: log.queue, mem_kb: log.mem_kb, worker: log.worker.clone().unwrap_or_default() };
    crate::relay::ran(headers, &ran);
}

/// Request bodies on their way to a worker.
pub type ProxyBody = BoxBody<Bytes, hyper::Error>;
/// Response bodies on their way to the client.
pub type ResponseBody = BoxBody<Bytes, BoxError>;

/// Tries per request, including the first.
const MAX_ATTEMPTS: u32 = 3;
/// Frames the relay may hold for one response, on top of its byte limit.
const RELAY_FRAMES: usize = 1024;
/// A response this small that the worker has already sent in full goes to the client without a
/// relay task (see `relay`).
const COMPLETE_MAX: usize = 64 << 10;
/// How long a response's first body frame may take to follow its headers and still be checked for
/// being the whole response.
const FIRST_FRAME_WAIT: Duration = Duration::from_millis(2);

const REQUEST_ID: HeaderName = HeaderName::from_static("x-forkrail-request-id");
const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
/// On every response from a deployment: its id. Every response also has `x-forkrail-request-id`.
const DEPLOYMENT_HEADER: HeaderName = HeaderName::from_static("x-forkrail-deployment");
/// On responses the router makes itself for a reason a client can act on.
const ERROR_HEADER: HeaderName = HeaderName::from_static("x-forkrail-error");
/// Answered by the router itself on every host, for health checks from outside: it never reaches
/// an app and isn't in the access log.
pub const HEALTH_PATH: &str = "/_forkrail/health";
/// The longest query string and user agent the access log keeps.
const MAX_LOGGED_QUERY: usize = 1024;
const MAX_LOGGED_AGENT: usize = 256;
const MAX_LOGGED_ROUTE: usize = 256;
/// The longest `Accept-Encoding` and `Content-Encoding` the access log keeps.
const MAX_LOGGED_ENCODING: usize = 64;
/// How long a URL elsewhere that a rewrite sends to may take to start answering.
const EXTERNAL_TIMEOUT: Duration = Duration::from_secs(120);

/// For URLs elsewhere that rewrites send to: public addresses only (`PublicResolver`).
type External = Client<HttpsConnector<HttpConnector<PublicResolver>>, ProxyBody>;

/// How deployments route without rules of their own: their files, then their function if they
/// have one.
static PLAIN_FUNCTION: std::sync::LazyLock<Arc<Engine>> = std::sync::LazyLock::new(|| Arc::new(Engine::plain(true)));
static PLAIN_STATIC: std::sync::LazyLock<Arc<Engine>> = std::sync::LazyLock::new(|| Arc::new(Engine::plain(false)));

/// Query parameters whose names, lowercase and without punctuation, end in one of these have their
/// values masked in the access log: `api_key`, `accessToken`, `X-Amz-Signature`, but not `keyword`.
/// `verifier` is OAuth's (`oauth_verifier`, `code_verifier`), `share` a share link's token
/// (`_vercel_share`).
const SECRET_PARAMS: [&str; 15] =
    ["key", "token", "secret", "password", "passwd", "signature", "sig", "auth", "credential", "credentials", "session", "jwt", "bypass", "verifier", "share"];
/// And these, as whole names: an OAuth sign-in's `code` (GitHub's, the dashboard's own, any app's)
/// signs in whoever holds it with the verifier they began the sign-in with, so a team member who
/// read it in the logs could finish an owner's sign-in as their own; a one-time password; SAML's
/// assertion when it comes in the query.
const SECRET_NAMES: [&str; 3] = ["code", "otp", "samlresponse"];

pub struct Proxy {
    pub registry: Arc<Registry>,
    /// Request bodies up to this size are read in full before a worker is leased,
    /// and the relay holds up to this much of a response the client hasn't taken yet.
    pub buffer_bytes: usize,
    /// What all requests and responses together may hold, in bytes: one permit per byte.
    pub buffered: Arc<Semaphore>,
    pub buffer_total: usize,
    /// Where clients are, for the log and the app's `x-vercel-ip-*` headers.
    pub geo: Arc<Geo>,
    /// Deployments' static files and routing rules, from S3 in this region; `None` sends
    /// everything to functions.
    pub statics: Option<Arc<Statics>>,
    /// The client for rewrites to URLs elsewhere; `None` if it couldn't be built.
    external: Option<External>,
    /// Where the issuer writes HTTP-01 key authorizations, which port 80 answers for custom
    /// domains; `None` answers none.
    pub challenges: Option<Challenges>,
    /// Schedules' runs (`schedules.rs`); `None` runs none.
    pub schedules: Option<Arc<crate::schedules::Runs>>,
    /// The install's and projects' firewalls (`firewall.rs`).
    pub firewalls: Arc<Firewalls>,
    /// The system mitigations: limits on each address across every project (`shield.rs`).
    pub shield: Arc<crate::shield::Shield>,
    /// The challenge page and its passes (`challenge.rs`).
    pub challenger: Arc<crate::challenge::Challenger>,
    /// Verified crawlers' and webhook senders' addresses, which attack mode lets by (`bots.rs`).
    pub bots: Arc<crate::bots::Bots>,
    /// Image Optimization (`images.rs`); `None` answers a deployment's `/_vercel/image` with 502.
    pub images: Option<Arc<crate::images::Images>>,
    /// Functions' answers kept and served again (`cache.rs`); `None` keeps none.
    pub cache: Option<Arc<crate::cache::ResponseCache>>,
    /// Who a function's call to its own project is for (`visitor.rs`).
    pub vouches: Arc<crate::visitor::Vouches>,
    /// How deep a request is in a chain of functions' calls back to the install (`loops.rs`).
    pub loops: Arc<crate::loops::Marks>,
    /// Next.js deployments' configs (`proxy/nextjs.rs`).
    next_sites: nextjs::Sites,
    /// The days' salts for Web Analytics' visitor numbers (`insights.rs`).
    pub salts: Arc<crate::insights::Salts>,
}

/// HTTP-01 key authorizations, read from the store the issuer writes them to (`acme.rs`), and kept
/// a little while: Let's Encrypt asks for one token from several places within seconds.
pub struct Challenges {
    store: crate::certs::Store,
    cache: std::sync::Mutex<std::collections::HashMap<String, (Instant, Option<Bytes>)>>,
}

impl Challenges {
    /// How long a token's answer is kept, found or not.
    const FOUND_FOR: Duration = Duration::from_secs(30);
    const MISSING_FOR: Duration = Duration::from_secs(2);

    pub fn new(store: crate::certs::Store) -> Self {
        Self { store, cache: Default::default() }
    }

    pub fn describe(&self) -> String {
        self.store.describe()
    }

    async fn get(&self, token: &str) -> Option<Bytes> {
        {
            let mut cache = self.cache.lock().unwrap();
            cache.retain(|_, (at, found)| at.elapsed() < if found.is_some() { Self::FOUND_FOR } else { Self::MISSING_FOR });
            if let Some((_, found)) = cache.get(token) {
                return found.clone();
            }
        }
        let found = match self.store.get(token).await {
            Ok(found) => found.map(Bytes::from),
            Err(e) => {
                warn!(token, error = %e, "reading an HTTP challenge failed");
                return None;
            }
        };
        self.cache.lock().unwrap().insert(token.to_owned(), (Instant::now(), found.clone()));
        found
    }
}

impl Proxy {
    pub fn new(registry: Arc<Registry>, buffer_bytes: usize, buffer_total: usize, geo: Arc<Geo>, statics: Option<Arc<Statics>>) -> Self {
        let buffer_total = buffer_total.clamp(1, Semaphore::MAX_PERMITS).min(u32::MAX as usize);
        let external = match external_client() {
            Ok(client) => Some(client),
            Err(e) => {
                warn!(error = %e, "no client for rewrites to other sites; they'll answer 502");
                None
            }
        };
        Self {
            registry,
            buffer_bytes,
            buffered: Arc::new(Semaphore::new(buffer_total)),
            buffer_total,
            geo,
            statics,
            external,
            challenges: None,
            schedules: None,
            firewalls: Firewalls::new(),
            shield: Arc::default(),
            challenger: Arc::new(crate::challenge::Challenger::local(crate::challenge::DEFAULT_BITS)),
            bots: Arc::default(),
            images: None,
            cache: None,
            vouches: Arc::new(crate::visitor::Vouches::local()),
            loops: Arc::new(crate::loops::Marks::local(crate::loops::DEFAULT_MAX_DEPTH, crate::loops::DEFAULT_MAX_CHAIN)),
            next_sites: nextjs::Sites::default(),
            salts: Arc::new(crate::insights::Salts::local()),
        }
    }

    /// Room for `len` bytes in the router-wide buffer.
    async fn room(&self, len: usize) -> OwnedSemaphorePermit {
        let n = len.min(self.buffer_total) as u32;
        self.buffered.clone().acquire_many_owned(n).await.expect("the semaphore is never closed")
    }
}

/// The listener a request came in on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// Plain HTTP on `FORKRAIL_LISTEN` (8080), open to the tester only: the default pool serves
    /// hosts the table doesn't know.
    Tester,
    /// HTTPS on `FORKRAIL_TLS_LISTEN` (443): deployments only.
    Https,
    /// This process's own loopback listener, where it sends schedules' runs to itself
    /// (`schedules.rs`): only requests with a run's token, each to its project's production.
    Schedule,
}

impl Entry {
    fn name(self) -> &'static str {
        match self {
            Entry::Tester => "tester",
            Entry::Https => "https",
            Entry::Schedule => "schedule",
        }
    }
}

pub async fn handle(
    mut req: Request<Incoming>,
    proxy: Arc<Proxy>,
    peer: SocketAddr,
    entry: Entry,
) -> Result<Response<ResponseBody>, Infallible> {
    if req.uri().path() == HEALTH_PATH && matches!(*req.method(), Method::GET | Method::HEAD) {
        return Ok(health());
    }
    // A test's run of a schedule, as if from the queue: the tester's port, from this machine only.
    if entry == Entry::Tester && req.uri().path() == SCHEDULE_RUN_PATH && peer.ip().to_canonical().is_loopback() {
        return Ok(schedule_run(req, &proxy).await);
    }
    // A schedule's run: only with the token of a run this process sent itself.
    let run = match entry {
        Entry::Schedule => {
            let token = req.headers().get(crate::schedules::RUN_HEADER).and_then(|v| v.to_str().ok()).unwrap_or_default();
            match proxy.schedules.as_ref().and_then(|runs| runs.take(token)) {
                Some(run) => Some(run),
                None => return Ok(coded(StatusCode::FORBIDDEN, "NOT_A_SCHEDULED_RUN", "This port takes scheduled runs only.")),
            }
        }
        _ => None,
    };
    // One host per request: an HTTP/2 `:authority` (or an absolute-form target) naming another
    // host than `Host` is refused, as RFC 9113 §8.3.1 has them agree. Otherwise the vouch, the
    // shield and routing could each read a different one: a vouch from one project, sent to it
    // by `:authority`, would pass on another project's `Host`.
    if let (Some(authority), Some(named)) = (req.uri().host(), req.headers().get(HOST))
        && named.to_str().map(normalize_host).ok() != Some(normalize_host(authority))
    {
        return Ok(coded(StatusCode::MISDIRECTED_REQUEST, "MISDIRECTED_REQUEST", "The request names two different hosts."));
    }
    // The system mitigations, before anything else is spent on the request (or written about it:
    // what they refuse is summed up once a minute instead).
    if run.is_none()
        && let Some(blocked) = proxy.shield.request(peer.ip(), &request_host(&req), || proxy.registry.project_of(&request_host(&req)).map(|p| p.0))
    {
        return Ok(refused_by_shield(blocked));
    }
    // A function's call to its own project, with the visitor's headers copied (Astro's edge
    // middleware): from here on it is the visitor's, as on Vercel (`visitor.rs`).
    // A rate-limit question counts only from the project's own function: one that carries a
    // vouch for this project, its own or among the visitor's headers `@vercel/firewall` copies
    // as `x-rr-<name>`. Anyone else's would run up any key's count and lock its user out.
    // A vouched call keeps its vouch's expiry: what the app is sent for it expires no later.
    let (peer, vouch_expires, from_its_function) = match run {
        None => {
            let own = vouched(&proxy, &req, peer, crate::visitor::HEADER);
            let copied = own.is_some() || vouched(&proxy, &req, peer, COPIED_VOUCH).is_some();
            (own.map_or(peer, |(ip, _)| ip), own.map(|(_, expires)| expires), copied)
        }
        Some(_) => (peer, None, false),
    };
    let mut log = AccessLog::new(&req, peer);
    log.entry = entry;
    log.ja4 = req.extensions().get::<Ja4>().map(|j| j.0.clone());
    if let Some(response) = oversized_head(&req) {
        log.host = normalize_host(&request_host(&req));
        return Ok(log.finish_now(response, "headers_too_large"));
    }
    // A raw backslash in the path: no browser sends one (they send `/` or `%5C`), and a WHATWG URL
    // reads it as `/` (`/\other.host/admin` is another host's `/admin` to a fetch-style app), so
    // the firewall and routes would judge one path while the app serves another.
    if req.uri().path().contains('\\') {
        log.host = normalize_host(&request_host(&req));
        return Ok(log.finish_now(coded(StatusCode::BAD_REQUEST, "BAD_REQUEST", "A path can't hold a backslash: send it as %5C."), "bad_request"));
    }
    // On 443, a host naming another port names no address here; an app taking it as given would
    // write it into a page the shared cache then serves to everyone at the host without one.
    if entry == Entry::Https && foreign_port(&req) {
        log.host = normalize_host(&request_host(&req));
        return Ok(log.finish_now(coded(StatusCode::MISDIRECTED_REQUEST, "MISDIRECTED_REQUEST", "The host names a port this server doesn't answer on."), "misdirected"));
    }
    // Percent-encoded letters, digits and `-._~` are those characters (RFC 3986 6.2.2.2): decoded
    // once here, the firewall, the routes, the files and the app all read one spelling
    // (`/api/%61dmin` is `/api/admin` to every rule, as it is to the app).
    if let Some(path) = unreserved_decoded(req.uri().path()) {
        let path_and_query = match req.uri().query() {
            Some(query) => format!("{path}?{query}"),
            None => path,
        };
        let mut parts = req.uri().clone().into_parts();
        if let Ok(path_and_query) = path_and_query.parse() {
            parts.path_and_query = Some(path_and_query);
            if let Ok(uri) = Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }
    // A `.` or `..` segment, written plainly or as `%2e`: browsers resolve them before sending, and
    // an app that resolves `/login/../admin` serves `/admin` while the firewall, the middleware's
    // matchers and the routes judged the path as written.
    if dot_segment(req.uri().path()) {
        log.host = normalize_host(&request_host(&req));
        return Ok(log.finish_now(coded(StatusCode::BAD_REQUEST, "BAD_REQUEST", "A path can't hold a `.` or `..` segment."), "bad_request"));
    }
    if let Some(run) = run {
        log.client = String::new();
        log.schedule = Some(run);
    }
    log.location = proxy.geo.lookup(peer.ip().to_canonical());
    let client_version = req.version();
    // Taken off with every `x-middleware-*` below; a Next deployment gets it back (`nextjs.rs`).
    let prefetch = req.headers().get(nextjs_prefetch()).cloned();
    let loop_mark = req.headers().get(crate::loops::HEADER).and_then(|v| v.to_str().ok()).map(str::to_owned);
    // A function's server `track` names its visitor's address (`insights.rs`), in a header taken
    // off below with every `x-vercel-*` a client sends.
    let tracked_for = server_tracked_for(&req);
    let (mut head, body) = to_upstream(req, peer, &log.request_id, entry, &log.location, &proxy.registry.here());
    log.host = normalize_host(&head.host);
    let host_scope = proxy.registry.project_of(&log.host);
    let host_project = host_scope.as_ref().map(|(project, _)| project.clone());
    // A paused project (spend protection, `budgets.rs`): nothing of it runs or is served, its
    // files and schedules' runs included, until a person resumes it.
    if let Some(project) = &host_project
        && let Some(why) = crate::budgets::paused(project)
    {
        log.project = project.clone();
        debug!(request = %log.request_id, %project, why = why.name(), "paused");
        let response = paused_response(&head.headers, &log.request_id);
        return Ok(log.finish_now(response, "paused"));
    }
    if log.schedule.is_none()
        && let Some((project, environment)) = &host_scope
        && let Ok(value) = HeaderValue::from_str(&proxy.vouches.vouch(peer.ip(), project, environment, vouch_expires.unwrap_or(unix_now() + crate::visitor::LIFETIME_S)))
    {
        head.headers.insert(HeaderName::from_static(crate::visitor::HEADER), value);
    }
    if let Some(run) = &log.schedule {
        // No client to name; the worker adds `CRON_SECRET` when it sees the key.
        for name in ["x-forwarded-for", "x-real-ip"] {
            head.headers.remove(name);
        }
        head.headers.insert(HeaderName::from_static(SCHEDULE_HEADER), HeaderValue::from_str(&run.key).expect("keys are header-safe"));
    }
    // The loop guard (`loops.rs`): a chain of functions' calls back to this install stops past its
    // limit, before anything of a deployment runs or is served for it.
    match proxy.loops.step(loop_mark.as_deref(), &log.request_id) {
        crate::loops::Step::Pass { mark, .. } => {
            if let Ok(value) = HeaderValue::from_str(&mark) {
                head.headers.insert(HeaderName::from_static(crate::loops::HEADER), value);
            }
        }
        stopped @ (crate::loops::Step::Deep { depth } | crate::loops::Step::Wide { depth }) => {
            log.project = proxy.registry.project_of(&log.host).map(|(project, _)| project).unwrap_or_default();
            let wide = matches!(stopped, crate::loops::Step::Wide { .. });
            warn!(request = %log.request_id, host = %log.host, project = %log.project, depth, wide, "a loop of calls back to the install stopped");
            let message = match wide {
                false => format!(
                    "This request was stopped: it is {depth} calls deep in a chain of calls the app made back to itself, past the limit of {}.",
                    proxy.loops.max_depth()
                ),
                true => format!(
                    "This request was stopped: the chain of calls the app made back to itself it belongs to has made more than {} requests at depth 2 and deeper.",
                    proxy.loops.max_chain()
                ),
            };
            return Ok(log.finish_now(coded(StatusCode::LOOP_DETECTED, "INFINITE_LOOP_DETECTED", &message), "loop"));
        }
    }
    // A custom domain that redirects serves nothing itself: path and query go along. Its project's
    // firewall still looks at the request first.
    if let Some(redirect) = proxy.registry.domain_redirect(&log.host) {
        if log.schedule.is_none()
            && let Some((project, environment)) = proxy.registry.project_of(&log.host)
            && (proxy.firewalls.active(&project) || proxy.shield.attack_mode(&project))
            && let Some(response) = screen(&proxy, &project, &environment, &head, peer, entry, None, &mut log)
        {
            return Ok(log.finish_now(response, "firewall"));
        }
        let status = StatusCode::from_u16(redirect.status).unwrap_or(StatusCode::PERMANENT_REDIRECT);
        let location = format!("https://{}{}", redirect.to, head.uri.path_and_query().map_or("/", |pq| pq.as_str()));
        log.rule = Some("domain".to_owned());
        let response = redirect_response(status, &location, &log);
        let outcome = if response.status() == status { "redirect" } else { "redirect_failed" };
        return Ok(log.finish_now(response, outcome));
    }
    let Some(target) = proxy.registry.resolve(&log.host, entry == Entry::Tester).await else {
        let response = not_found_response(&head.headers, &log.request_id);
        return Ok(log.finish_now(response, "not_found"));
    };
    // Skew protection (skew.rs): a request naming another of the project's deployments may be
    // served by it, from here on (firewall, protection, routes, files, function). A schedule's
    // run is production's.
    let (target, pin) = match log.schedule {
        Some(_) => (target, None),
        None => proxy.registry.pin(target, &log.host, head.uri.query(), &head.headers, unix_now(), || proxy.bots.verified(peer.ip())),
    };
    if let Some(pin) = pin {
        log.skew = Some(pin.outcome.name());
        if pin.strip_query
            && let Some(query) = head.uri.query()
        {
            head.uri = with_query(&head.uri, &crate::skew::strip(query).unwrap_or_default());
        }
    }
    log.deployment = target.deployment.clone();
    log.project = target.project.clone();
    log.region = target.region.clone();
    log.spend_begin();
    log.function = target.pool.as_ref().map(|pool| FunctionLog::of(pool.config()));
    // A browser's answer to the challenge: before the firewall, which would challenge it again.
    if log.schedule.is_none() && head.uri.path() == crate::challenge::PATH && head.method == Method::POST {
        let (response, outcome) = solve_challenge(&proxy, &head, body, peer, entry, &log).await;
        return Ok(log.finish_now(response, outcome));
    }
    // An app asking about its own rate limits: the firewall answers, never the deployment. A
    // question from anyone else is any other request.
    if log.schedule.is_none()
        && from_its_function
        && let Some(id) = rate_limit_api_id(head.uri.path())
    {
        let response = rate_limit_api(&proxy, &target, &head, peer, entry, &id, &mut log);
        return Ok(log.finish_now(response, "rate_limit_api"));
    }
    // The firewall, before protection and anything of the deployment's. Schedules' runs come from
    // the router itself and pass.
    if log.schedule.is_none() && (proxy.firewalls.active(&target.project) || proxy.shield.attack_mode(&target.project)) {
        let target_path = match proxy.firewalls.uses_target_path(&target.project) {
            true => target_path(&proxy, &target, &head, &log.host).await,
            false => None,
        };
        if let Some(response) = screen(&proxy, &target.project, &target.environment, &head, peer, entry, target_path.as_deref(), &mut log) {
            return Ok(log.finish_now(response, "firewall"));
        }
    }
    // Deployment protection, before anything of the deployment's runs. Never on the tester's port.
    if entry == Entry::Https
        && let Some(gate) = &target.protection
        && let Err((response, outcome)) = guard(gate, &head, peer, &mut log)
    {
        return Ok(log.finish_now(response, outcome));
    }
    protection::strip_cookies(&mut head.headers);
    // Web Analytics' and Speed Insights' paths (`insights.rs`), before the deployment's own routes.
    if let Some(ask) = crate::insights::ask(&head.method, head.uri.path()) {
        return Ok(insights(&proxy, &target, &head, body, peer, ask, tracked_for, log).await);
    }
    // Only the routes say which of a deployment's functions a request is for, and only the router
    // calls a middleware or speaks its protocol.
    head.headers.remove(forkrail_common::FUNCTION_HEADER);
    middleware::strip_client_headers(&mut head.headers);
    // A Next.js deployment: its config, and the headers its visitors may send it.
    let next = match &target.next {
        Some(next) => {
            nextjs::request_headers(&mut head.headers, prefetch);
            let ctx = nextjs::ctx(&proxy, next, target.static_files.clone()).await;
            // `<assetPrefix's path>/_next/…`, which Next's own server answers as `/_next/…`.
            if let Some(path) = ctx.site.as_ref().and_then(|s| s.unprefixed(head.uri.path()))
                && let Ok(pq) = format!("{path}{}", head.uri.query().map(|q| format!("?{q}")).unwrap_or_default()).parse::<hyper::http::uri::PathAndQuery>()
            {
                let mut parts = head.uri.clone().into_parts();
                parts.path_and_query = Some(pq);
                if let Ok(uri) = Uri::from_parts(parts) {
                    head.uri = uri;
                }
            }
            Some(ctx)
        }
        None => None,
    };
    // Rules and static files decide first; the function gets what they leave to it.
    let mut effects = Effects::default();
    // Paths Vercel answers before any function: repeated slashes (a 308) and encoded `..` (refused).
    let path = head.uri.path();
    let guarded = path.contains("//") || (path.contains('%') && path.to_ascii_lowercase().contains("%2e"));
    // The body, once a middleware has needed it read.
    let mut body = Some(body);
    let mut read: Option<UpstreamBody> = None;
    if target.routes() || guarded {
        let mut routed = route(&proxy, &target, &head, guarded, &mut log).await;
        if let Routed::Middleware(pending) = routed {
            let incoming = body.take().expect("not read yet");
            match middleware::run(&proxy, &target, &mut head, incoming, *pending, client_version, &mut log).await {
                middleware::Ran::Answered(response) => return Ok(response),
                middleware::Ran::Routed(next, upstream) => {
                    routed = next;
                    read = Some(upstream);
                }
            }
        }
        match routed {
            Routed::Done(response) => return Ok(response),
            Routed::Middleware(_) => unreachable!("middleware::run routes on to the end"),
            Routed::External { url, mut effects } => {
                // Query transforms change the destination's query (its own parameters over the
                // client's), not the client's alone, which would drop the destination's.
                if effects.query.is_none() {
                    let query = url.split('#').next().unwrap_or_default().split_once('?').map_or("", |(_, q)| q);
                    effects.query = Some(query.to_owned());
                }
                effects.transform_request(&mut head);
                let url = match &effects.query {
                    Some(q) => with_url_query(&url, q),
                    None => url,
                };
                let body = match (read.as_mut(), body.take()) {
                    (Some(upstream), _) => upstream.take().expect("the middleware gave it back"),
                    (None, Some(incoming)) => incoming.boxed(),
                    (None, None) => unreachable!("the body is in one or the other"),
                };
                return Ok(external(&proxy, head, body, &url, effects, client_version, log).await);
            }
            Routed::Function(fx) => {
                effects = fx;
                effects.transform_request(&mut head);
                if let Some(value) = effects.function.as_deref().and_then(|f| HeaderValue::from_str(f).ok()) {
                    head.headers.insert(forkrail_common::FUNCTION_HEADER, value);
                }
            }
        }
    } else if let Some(query) = head.uri.query() {
        // Vercel rebuilds every query string before the function sees it (`a+b` as `a%20b`).
        effects.query = rules::normalize_query(query);
    }
    let Some(pool) = target.pool else {
        return Ok(log.finish_now(coded(StatusCode::NOT_FOUND, "NOT_FOUND", "The page could not be found."), "not_found"));
    };
    if let Some(query) = &effects.query {
        head.uri = with_query(&head.uri, query);
    }
    // A Next.js route's own `maxDuration`, whether it's a route handler's request, a file only
    // Next's server has.
    if let Some(mut next) = next {
        next.file = next.site.as_ref().is_some_and(|s| s.server_file(head.uri.path()));
        if let Some(route) = next.site.as_ref().and_then(|s| s.route_of_path(head.uri.path())) {
            if effects.max_duration.is_none() {
                effects.max_duration = route.max_duration_s.map(Duration::from_secs);
            }
            next.handler = matches!(route.kind, forkrail_common::next::RouteKind::AppRoute | forkrail_common::next::RouteKind::Api);
        }
        effects.next = Some(Arc::new(next));
    }
    // A page or answer the cache has is served from it; otherwise the cache notes how the
    // function's answer is to be kept.
    if let Some(cache) = proxy.cache.clone()
        && let Some(response) = cached::before(&proxy, &cache, &pool, target.static_files.as_deref(), &mut head, &mut effects, &mut log).await
    {
        return Ok(response);
    }
    let body = match (read, body) {
        (Some(read), _) => read,
        (None, Some(incoming)) => match read_body(incoming, &head.headers, &proxy, max_body(head.payload), client_version).await {
            Ok(body) => body,
            Err(Unread::TooLarge) => return Ok(log.finish_now(payload_too_large(&proxy, &effects), "too_large")),
            Err(Unread::Failed(e)) => {
                warn!(request = %log.request_id, error = %e, "reading request body failed");
                return Ok(log.finish_now(error(StatusCode::BAD_REQUEST, "could not read request body"), "bad_request"));
            }
        },
        (None, None) => unreachable!("the body is in one or the other"),
    };
    log.bytes_in = body.len();
    // From here the app's lines and calls for it are counted, for its log line to say what's missing
    // (from its middleware's call, if it had one).
    if log.app_logs.is_none() {
        pool.logs().begin(&log.request_id);
        log.app_logs = Some(pool.logs().clone());
    }

    // The exchange with a worker runs in a task of its own, since hyper drops this handler when
    // the client leaves. A request no worker has yet goes with its client; one a worker has runs
    // on until the app answers (see `abandon`).
    let (client, response) = oneshot::channel();
    tokio::spawn(exchange(head, body, pool, client_version, proxy, log, effects, client));
    // The task answers unless the client has left, and then no one is waiting here.
    Ok(response.await.unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "worker failed")))
}

/// Lease a worker, send it the request, and hand its response to `client`, retrying by how an
/// attempt failed.
async fn exchange(
    head: UpstreamHead,
    mut body: UpstreamBody,
    pool: Arc<Pool>,
    client_version: Version,
    proxy: Arc<Proxy>,
    mut log: AccessLog,
    mut effects: Effects,
    mut client: oneshot::Sender<Response<ResponseBody>>,
) {
    // The deployment's maxDuration (or the function's own, if shorter), from when the router has the
    // request, queueing included.
    let limit = effects.max_duration.map_or(pool.max_duration(), |own| own.min(pool.max_duration()));
    let deadline = tokio::time::Instant::now() + limit;
    pool.earn_retry();
    let route = head.uri.path().to_owned();
    let mut tried = Vec::new();
    // A body that can't be sent again doesn't go to another router, which may refuse it
    // (`relay.rs`): the pool's own workers take it.
    if !body.replayable()
        && let Some(slot) = &pool.config().relay
    {
        tried.push(crate::relay::id(slot.region()));
    }
    // After a worker died under this request, it runs alone: it may be what killed it.
    let mut alone = effects.run_alone;
    loop {
        log.attempts += 1;
        let queued = Instant::now();
        let acquired = tokio::select! {
            biased;
            // No worker has it yet, so it goes with its client.
            () = client.closed() => {
                log.queue += queued.elapsed();
                return log.write("client_gone");
            }
            acquired = tokio::time::timeout_at(deadline, pool.acquire(&route, &tried, alone)) => acquired,
        };
        log.queue += queued.elapsed();
        let lease = match acquired {
            Ok(Ok(lease)) => lease,
            // Paused while it waited, or its worker let go by the pause: said as such.
            Ok(Err(_)) if crate::budgets::paused(&log.project).is_some() => {
                let response = paused_response(&head.headers, &log.request_id);
                return answer(client, log.finish_now(response, "paused"));
            }
            Ok(Err(unavailable)) => return answer(client, log.finish_now(service_unavailable(unavailable.retry_after), "unavailable")),
            Err(_) => return answer(client, log.finish_now(timed_out(), "timeout")),
        };
        log.worker = Some(lease.worker.id.clone());
        log.start = Some(lease.start);
        let mut request = head.request(body.take().expect("a failed attempt either returns the body or doesn't retry"));
        let relayed = lease.worker.relayed();
        match relayed {
            Some(proofs) => crate::relay::mark(&mut request, proofs, &pool.config().name, effects.max_duration, alone),
            None if pool.profile(&lease) => {
                request.headers_mut().insert(PROFILE_REQUEST_HEADER, HeaderValue::from_static("1"));
            }
            None => {}
        }
        let mut sender = lease.sender();
        // Past the deadline, dropping the send resets the worker's stream, which cancels the
        // request in the app, and the lease goes with it, freeing the slot. A client leaving
        // doesn't: the app would carry on regardless.
        let Ok(sent) = tokio::time::timeout_at(deadline, sender.try_send_request(request)).await else {
            warn!(request = %log.request_id, worker = %lease.worker.id, "max duration reached before the response started");
            return answer(client, log.finish_now(timed_out(), "timeout"));
        };
        // Another attempt is only for a client still waiting.
        let retry = |log: &AccessLog, body: &UpstreamBody, client: &oneshot::Sender<_>| {
            log.attempts < MAX_ATTEMPTS && body.replayable() && safe_to_repeat(&head) && !client.is_closed() && pool.take_retry()
        };
        let sent = match (sent, relayed) {
            (Ok(response), Some(proofs)) if !crate::relay::proven(proofs, response.headers()) => {
                // Not the router the link was opened with: nothing from it is taken.
                warn!(request = %log.request_id, link = %lease.worker.id, "an answer on the link to another router wasn't proven");
                tried.push(lease.worker.id.clone());
                lease.worker.close_link();
                if retry(&log, &body, &client) {
                    continue;
                }
                return answer(client, log.finish_now(error(StatusCode::BAD_GATEWAY, "worker failed"), "worker_failed"));
            }
            (Ok(response), Some(_)) if let Some(why) = crate::relay::refused(response.headers()) => {
                // That router didn't run it, so no worker saw it: it runs here. One that's
                // stopping is let go of, and the link opens again to the router that took over.
                info!(request = %log.request_id, link = %lease.worker.id, why, "the router there didn't run it; starting a worker for it from here");
                if why == "old" {
                    crate::relay::old(&lease.worker.id);
                }
                if why == "draining" || why == "old" {
                    lease.worker.close_link();
                }
                tried.push(lease.worker.id.clone());
                continue;
            }
            (sent, _) => sent,
        };
        let mut e = match sent {
            Ok(response) if response.headers().get(RETRY_HEADER).is_some_and(|v| v == RETRY_APP_CONNECTION) => {
                // The connection to the app failed under it, with the app still running (uvicorn
                // closes one after an exception in a request it has already answered). The
                // worker is fine. The app may have started on the request, so retry only what's
                // harmless to repeat.
                let detail = response.headers().get(APP_ERROR_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
                if retry(&log, &body, &client) {
                    warn!(request = %log.request_id, worker = %lease.worker.id, error = %detail, "app connection failed, retrying");
                    continue;
                }
                warn!(request = %log.request_id, worker = %lease.worker.id, error = %detail, "app connection failed");
                return answer(client, log.finish_now(error(StatusCode::BAD_GATEWAY, "worker failed"), "worker_failed"));
            }
            Ok(response) if response.headers().contains_key(RETRY_HEADER) => {
                // The app died under another request. This one may have run partway, so retry
                // only what's harmless to repeat, and not alone: it didn't cause it.
                tried.push(lease.worker.id.clone());
                pool.remove(&lease.worker.id);
                if retry(&log, &body, &client) {
                    warn!(request = %log.request_id, worker = %lease.worker.id, "app exited under it, retrying");
                    continue;
                }
                warn!(request = %log.request_id, worker = %lease.worker.id, "app exited under it");
                return answer(client, log.finish_now(error(StatusCode::BAD_GATEWAY, "worker failed"), "worker_failed"));
            }
            Ok(response) if client.is_closed() => return abandon(response, lease, &pool, deadline, log).await,
            // A Next.js deployment's worker naming one of the deployment's static files.
            Ok(response) if effects.next.is_some() && response.headers().contains_key(nextjs::STATIC_HEADER) => {
                return answer(client, worker_file(response, lease, &pool, &head, &proxy, &effects, log).await);
            }
            Ok(response) => {
                let response = relay(response, lease, &pool, deadline, client_version, &proxy, &effects, log).await;
                // Another router's request: its cache is that router's to fill (`relay.rs`).
                if effects.for_router {
                    return answer(client, response);
                }
                return answer(client, cached::after(&proxy, effects.cache.take(), response));
            }
            Err(e) => e,
        };
        if let Some(request) = e.take_message() {
            // Never reached the worker, so any request is safe to retry.
            tried.push(lease.worker.id.clone());
            pool.remove(&lease.worker.id);
            body.put_back(request.into_body());
            if log.attempts < MAX_ATTEMPTS {
                continue;
            }
        } else if let Some(reason) = stream_reset(e.error()).filter(|_| !lease.sender().is_closed()) {
            // The worker reset this one stream and its tunnel is still up: it failed this
            // request, not itself (a runtime before `app-connection` did this when its
            // connection to the app failed). So the worker stays, and its routes aren't blamed.
            if !header_overflow(e.error()) && retry(&log, &body, &client) {
                warn!(request = %log.request_id, worker = %lease.worker.id, %reason, "worker reset the stream, retrying");
                continue;
            }
            warn!(request = %log.request_id, worker = %lease.worker.id, %reason, "worker reset the stream");
            return answer(client, log.finish_now(error(StatusCode::BAD_GATEWAY, "worker failed"), "worker_failed"));
        } else {
            tried.push(lease.worker.id.clone());
            pool.worker_died(&lease.worker);
            // The worker may have acted on it, so retry only what's harmless to repeat; never one
            // whose headers ended the tunnel, which would end the next worker's too.
            if !header_overflow(e.error()) && retry(&log, &body, &client) {
                warn!(request = %log.request_id, worker = %lease.worker.id, error = %error_chain(e.error()), "retrying alone");
                alone = true;
                continue;
            }
        }
        warn!(request = %log.request_id, worker = %lease.worker.id, error = %error_chain(e.error()), "upstream failed");
        return answer(client, log.finish_now(error(StatusCode::BAD_GATEWAY, "worker failed"), "worker_failed"));
    }
}

/// To the client, if it's still there. If it isn't, dropping the response ends it: its log line
/// says `client_gone`, and a relay resets the worker's stream.
fn answer(client: oneshot::Sender<Response<ResponseBody>>, response: Response<ResponseBody>) {
    let _ = client.send(response);
}

/// The rest of a request whose client left once a worker had it. The app goes on with it either
/// way (neither uvicorn nor Node's `http` stops a handler when its client leaves), so the worker
/// stays leased until the app has answered, and the answer is dropped. Released at once, the
/// worker would retire and Lambda would freeze the environment mid-request: the handler would
/// finish in whichever invocation thaws it next, minutes later by its own clock, beside that
/// invocation's requests.
///
/// A response of known length is read to its end. One without a length may be a stream with no
/// end, so it's dropped, which resets the worker's stream and tells the app its client has gone.
async fn abandon(response: Response<Incoming>, lease: Lease, pool: &Pool, deadline: tokio::time::Instant, mut log: AccessLog) {
    let (mut parts, mut upstream) = response.into_parts();
    worker_measures(&mut parts.headers, &lease, pool, &mut log);
    log.status = parts.status.as_u16();
    log.encoding = header_text(&parts.headers, CONTENT_ENCODING);
    log.ttfb = Some(log.started.elapsed());
    if parts.headers.contains_key(CONTENT_LENGTH) {
        while !upstream.is_end_stream() {
            match tokio::time::timeout_at(deadline, upstream.frame()).await {
                Ok(Some(Ok(_))) => {}
                Ok(None | Some(Err(_))) => break,
                Err(_) => {
                    warn!(request = %log.request_id, worker = %lease.worker.id, "max duration reached on a request its client left");
                    break;
                }
            }
        }
    }
    log.worker_ms.store(lease.leased_at.elapsed().as_millis() as u64, Ordering::Relaxed);
    drop(upstream);
    drop(lease);
    log.write("client_gone");
}

/// What the worker measured, taken off its response's headers into the log and the pool's
/// learned costs.
fn worker_measures(headers: &mut HeaderMap, lease: &Lease, pool: &Pool, log: &mut AccessLog) {
    let rss_start = take_kb(headers, RSS_START_HEADER);
    let rss = take_kb(headers, RSS_HEADER);
    let rss_peak = take_kb(headers, RSS_PEAK_HEADER);
    log.cpu_us = take_kb(headers, CPU_HEADER);
    log.busy_us = take_kb(headers, BUSY_HEADER);
    if lease.worker.relayed().is_some() {
        // Another router ran it (`relay.rs`): what it learned is its own, and the line says
        // which of its workers took it, how, and how long it waited there.
        crate::relay::take_answer(headers);
        log.relayed = true;
        if let Some(ran) = crate::relay::take_ran(headers) {
            log.mem_kb = ran.mem_kb;
            log.start = Some(ran.start);
            log.queue += ran.queue;
            log.worker = Some(ran.worker);
        }
    } else {
        log.mem_kb = pool.observe(lease, rss_start, rss, rss_peak);
        pool.observe_cpu(lease, log.cpu_us, log.busy_us);
    }
    log.cpu_profile = headers.remove(PROFILE_HEADER).and_then(|v| v.to_str().ok().map(str::to_owned));
    log.rss_kb = rss;
    log.route = headers.remove(ROUTE_HEADER).map(|v| truncate(&String::from_utf8_lossy(v.as_bytes()), MAX_LOGGED_ROUTE));
}

/// Pass the worker's response on, and release the worker once it has sent everything. A small
/// response that's complete within `FIRST_FRAME_WAIT` goes out as it is; otherwise a task pulls the
/// body off the tunnel as fast as the worker sends it, up to `buffer_bytes` ahead of the client.
/// Past `deadline`, a response still streaming from the worker is cut off.
async fn relay(
    response: Response<Incoming>,
    lease: Lease,
    pool: &Pool,
    deadline: tokio::time::Instant,
    client_version: Version,
    proxy: &Proxy,
    effects: &Effects,
    mut log: AccessLog,
) -> Response<ResponseBody> {
    let (mut parts, mut upstream) = response.into_parts();
    worker_measures(&mut parts.headers, &lease, pool, &mut log);
    parts.version = client_version;
    if effects.for_router {
        // The asking router does the rest with the worker's answer as it came, and writes the line.
        for_router(&mut parts.headers, &log);
    } else {
        // A worker's word for the router alone, here on an answer it isn't honoured on (`worker_file`).
        parts.headers.remove(nextjs::STATIC_HEADER);
        // Routing Middleware's protocol is the router's, never the client's: Vercel drops it from any
        // function's answer too (seen: Qwik's empty answer for a static path, `x-middleware-next: 1`).
        // A Next deployment's answer keeps what Next's own client reads (`nextjs.rs`).
        match &effects.next {
            Some(next) => {
                nextjs::answer_headers(&mut parts.headers, next.handler);
                if let Some(label) = nextjs::label(&parts.headers)
                    && log.cache.is_none_or(|c| c == crate::cache::Status::Miss.as_str())
                {
                    log.cache = Some(label.as_str());
                }
            }
            None => middleware::strip_protocol(&mut parts.headers),
        }
        effects.apply(&mut parts.status, &mut parts.headers);
        default_content_type(parts.status, &mut parts.headers);
        parts.headers.insert(REQUEST_ID, HeaderValue::from_str(&log.request_id).expect("hex is a valid header"));
        log.tag(&mut parts.headers);
    }
    log.status = parts.status.as_u16();
    log.encoding = header_text(&parts.headers, CONTENT_ENCODING);
    log.ttfb = Some(log.started.elapsed());
    log.expected_out = parts.headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok());

    // A response that's complete as it arrives, with no body or one frame that ends it, as most API
    // answers are, goes out as it is: no relay task, channel or timers, and the worker is free at
    // once. Its bytes still take room in the router's buffer, without waiting for it; with no room,
    // or for anything longer, the relay below pulls the body.
    let mut first = None;
    if !upstream.is_end_stream() {
        // The body usually comes right behind the headers; a stream that sends nothing this soon
        // goes to the relay, its headers this much later.
        match tokio::time::timeout(FIRST_FRAME_WAIT, upstream.frame()).await {
            Ok(Some(Ok(frame))) => first = Some(frame),
            Ok(None) => {}
            Ok(Some(Err(e))) => return relay_task(parts, upstream, Some(Err(e)), lease, deadline, proxy, log),
            Err(_) => return relay_task(parts, upstream, None, lease, deadline, proxy, log),
        }
    }
    let len = first.as_ref().and_then(Frame::data_ref).map_or(0, Bytes::len);
    let complete = upstream.is_end_stream() || first.is_none();
    if complete
        && len <= COMPLETE_MAX
        && let Ok(room) = proxy.buffered.clone().try_acquire_many_owned(len as u32)
    {
        log.worker_ms.store(lease.leased_at.elapsed().as_millis() as u64, Ordering::Relaxed);
        drop(upstream);
        drop(lease);
        let body = CompleteBody { frame: first, len: len as u64, _room: room, log: Some(log) };
        return Response::from_parts(parts, body.boxed());
    }
    relay_task(parts, upstream, first.map(Ok), lease, deadline, proxy, log)
}

/// A Next.js deployment's worker answered with one of the deployment's own static files
/// (`nextjs::STATIC_HEADER`): Next's routing landed on a file the router serves and the package
/// doesn't hold (a config rewrite of `/docs/:path*` to `/:path*`, onto `public/` or
/// `/_next/static/`). The worker is let go at once (its answer has no body), and the file goes out
/// as it does to a request for its own path (`settle`): its headers, a byte range or a 304, the
/// routes' headers, the config's `headers()` for the path asked for; then the worker's own
/// headers where the file's answer has none of the name (a cookie the middleware set). A file the
/// deployment hasn't got is a 404, and the worker is never asked again.
async fn worker_file(response: Response<Incoming>, lease: Lease, pool: &Pool, head: &UpstreamHead, proxy: &Proxy, effects: &Effects, mut log: AccessLog) -> Response<ResponseBody> {
    let (mut parts, upstream) = response.into_parts();
    worker_measures(&mut parts.headers, &lease, pool, &mut log);
    let named = parts.headers.remove(nextjs::STATIC_HEADER).as_ref().and_then(nextjs::named_file);
    log.worker_ms.store(lease.leased_at.elapsed().as_millis() as u64, Ordering::Relaxed);
    drop(upstream);
    drop(lease);
    let next = effects.next.as_deref();
    nextjs::answer_headers(&mut parts.headers, false);
    nextjs::take_label(&mut parts.headers);
    log.ttfb = Some(log.started.elapsed());
    let not_found = |log: AccessLog| log.finish_now(coded(StatusCode::NOT_FOUND, "NOT_FOUND", "The page could not be found."), "not_found");
    let Some(path) = named else {
        warn!(request = %log.request_id, "a worker named a static file that isn't a path");
        return not_found(log);
    };
    let manifest = match (proxy.statics.as_deref(), next.and_then(|n| n.files.as_deref())) {
        (Some(statics), Some(id)) => statics.manifest(id).await,
        _ => None,
    };
    let Some(found) = manifest.as_ref().and_then(|m| m.files.get(&path)).cloned() else {
        warn!(request = %log.request_id, path, "a worker named a static file the deployment hasn't got");
        return not_found(log);
    };
    if !matches!(head.method, Method::GET | Method::HEAD) {
        let mut response = empty(StatusCode::METHOD_NOT_ALLOWED);
        response.headers_mut().insert(hyper::header::ALLOW, HeaderValue::from_static("GET, HEAD, OPTIONS"));
        return log.finish_now(response, "static");
    }
    let status = effects.status.or((parts.status != StatusCode::OK).then_some(parts.status));
    let statics = proxy.statics.as_deref().expect("a manifest came from it");
    match statics.respond(&found, &head.method, &head.headers, status, crate::statics::Serve::File, |len| proxy.room(len)).await {
        // Vercel's 304 for a file carries its caching headers alone.
        Some(served) if served.not_modified => log.finish_now(served.response, "static"),
        Some(served) => {
            log.encoding = served.encoding.map(str::to_owned);
            log.bytes_out = served.bytes;
            let mut response = respond_headers(served.response, &effects.headers, &effects.transforms);
            let site = next.and_then(|n| n.site.as_deref());
            file_headers(response.headers_mut(), &path, site, head, &log.host);
            nextjs::carry_over(&parts.headers, response.headers_mut());
            for (name, value) in &effects.added {
                response.headers_mut().append(name.clone(), value.clone());
            }
            log.finish_now(response, "static")
        }
        None => {
            warn!(request = %log.request_id, path, "a static file a worker named couldn't be read");
            log.finish_now(coded(StatusCode::BAD_GATEWAY, "ROUTER_STATIC_UNAVAILABLE", "The file couldn't be read."), "static_failed")
        }
    }
}

/// Pull the rest of a response off the tunnel, starting with `first` if a frame already came.
fn relay_task(
    parts: hyper::http::response::Parts,
    mut upstream: Incoming,
    mut first: Option<Result<Frame<Bytes>, hyper::Error>>,
    lease: Lease,
    deadline: tokio::time::Instant,
    proxy: &Proxy,
    log: AccessLog,
) -> Response<ResponseBody> {
    let limit = proxy.buffer_bytes;
    let permits = Arc::new(Semaphore::new(limit));
    let buffered = proxy.buffered.clone();
    let buffer_total = proxy.buffer_total;
    let (tx, rx) = mpsc::channel::<RelayItem>(RELAY_FRAMES);
    let worker_ms = log.worker_ms.clone();
    let request_id = log.request_id.clone();
    tokio::spawn(async move {
        let mut timed_out = false;
        loop {
            let frame = if let Some(frame) = first.take() {
                Some(frame)
            } else {
                tokio::select! {
                    frame = upstream.frame() => frame,
                    // The client went away. Dropping the body resets the worker's stream.
                    _ = tx.closed() => break,
                    _ = tokio::time::sleep_until(deadline) => {
                        timed_out = true;
                        break;
                    }
                }
            };
            let frame = match frame {
                None => break,
                Some(Ok(frame)) => frame,
                Some(Err(e)) => {
                    let _ = tx.send(Err(e.into())).await;
                    break;
                }
            };
            // Record before handing over the last frame, since the client may log as soon as it has it.
            let last = upstream.is_end_stream();
            if last {
                worker_ms.store(lease.leased_at.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
            let len = frame.data_ref().map_or(0, Bytes::len);
            // Room in this response's buffer, then in the router's.
            let room = async {
                let own = permits.clone().acquire_many_owned(len.min(limit) as u32).await;
                let shared = buffered.clone().acquire_many_owned(len.min(buffer_total) as u32).await;
                (own.expect("the semaphore is never closed"), shared.expect("the semaphore is never closed"))
            };
            let permit = tokio::select! {
                permit = room => permit,
                _ = tx.closed() => break,
                _ = tokio::time::sleep_until(deadline), if !last => {
                    timed_out = true;
                    break;
                }
            };
            if tx.send(Ok((frame, permit))).await.is_err() || last {
                break;
            }
        }
        let _ = worker_ms.compare_exchange(
            u64::MAX,
            lease.leased_at.elapsed().as_millis() as u64,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        if timed_out {
            warn!(request = %request_id, worker = %lease.worker.id, "max duration reached while streaming; cutting the response off");
            // Free the worker first: resetting its stream cancels the request in the app.
            drop(upstream);
            drop(lease);
            let _ = tx.send(Err(TimedOut.into())).await;
        }
    });
    Response::from_parts(parts, ClientBody { rx, log: Some(log) }.boxed())
}

/// The error a response cut off at its deadline ends with.
#[derive(Debug)]
struct TimedOut;

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FUNCTION_INVOCATION_TIMEOUT: the response passed its maxDuration")
    }
}

impl std::error::Error for TimedOut {}

/// A response the worker had already sent in full: at most one frame, with its room in the
/// router's buffer. Writes the access log line when it's done.
struct CompleteBody {
    frame: Option<Frame<Bytes>>,
    len: u64,
    _room: OwnedSemaphorePermit,
    log: Option<AccessLog>,
}

impl Body for CompleteBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        match this.frame.take() {
            Some(frame) => {
                if let (Some(log), Some(data)) = (this.log.as_mut(), frame.data_ref()) {
                    log.bytes_out += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            None => {
                if let Some(log) = this.log.take() {
                    log.write("ok");
                }
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.frame.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(if self.frame.as_ref().is_some_and(Frame::is_data) { self.len } else { 0 })
    }
}

impl Drop for CompleteBody {
    fn drop(&mut self) {
        // Once its one frame is out, hyper may stop without asking for the end.
        if let Some(log) = self.log.take() {
            log.write(if self.frame.is_none() { "ok" } else { "client_gone" });
        }
    }
}

/// A frame, with the room it takes in the response's buffer and in the router's.
type RelayItem = Result<(Frame<Bytes>, (OwnedSemaphorePermit, OwnedSemaphorePermit)), BoxError>;

/// The client's side of a relayed response. Writes the access log line when it ends.
struct ClientBody {
    rx: mpsc::Receiver<RelayItem>,
    log: Option<AccessLog>,
}

impl ClientBody {
    fn finish(&mut self, outcome: &str) {
        if let Some(log) = self.log.take() {
            log.write(outcome);
        }
    }
}

impl Body for ClientBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        match ready!(this.rx.poll_recv(cx)) {
            // Dropping the permits makes room for the relay to pull more.
            Some(Ok((frame, _permits))) => {
                if let (Some(log), Some(data)) = (this.log.as_mut(), frame.data_ref()) {
                    log.bytes_out += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Some(Err(e)) => {
                this.finish(if e.is::<TimedOut>() { "timeout" } else { "worker_failed" });
                Poll::Ready(Some(Err(e)))
            }
            None => {
                this.finish("ok");
                Poll::Ready(None)
            }
        }
    }
}

impl Drop for ClientBody {
    fn drop(&mut self) {
        // HTTP/1.1 can stop polling once Content-Length bytes are out, without asking for the end.
        let complete = self.log.as_ref().is_some_and(|log| log.expected_out == Some(log.bytes_out));
        self.finish(if complete { "ok" } else { "client_gone" });
    }
}

/// One line per request, written once the response is fully sent or has failed. One dropped
/// without a line (its handler went with a client that left) writes itself as `client_gone`.
struct AccessLog {
    request_id: String,
    /// The connection it came on, whose bytes the spend ledger shares by its requests (`spend.rs`).
    meter: Option<Arc<crate::spend::Conn>>,
    /// The listener it came in on.
    entry: Entry,
    /// Normalised, as the routing table keys it.
    host: String,
    /// `None` for the default pool, or before the host is looked up.
    deployment: Option<String>,
    /// The deployment's project, and the region its workers run in; empty until the host is
    /// looked up, and for the default pool.
    project: String,
    region: String,
    method: Method,
    path: String,
    /// With secret-looking parameters masked (see `mask_query`).
    query: Option<String>,
    /// The client's address, as the router saw it.
    client: String,
    /// The request's `Referer`, masked like `query`.
    referer: Option<String>,
    /// Where the client is, by its address.
    location: Location,
    /// `h1.1` or `h2`, and the client's user agent.
    proto: &'static str,
    agent: Option<String>,
    /// The request's `Accept-Encoding` and the response's `Content-Encoding`: what a client could
    /// take compressed, and what it got.
    accept_encoding: Option<String>,
    encoding: Option<String>,
    started: Instant,
    attempts: u32,
    /// Time spent waiting for a worker, over all attempts.
    queue: Duration,
    worker: Option<String>,
    status: u16,
    /// Time until the worker's response headers arrived.
    ttfb: Option<Duration>,
    /// How long the worker was held, set when it has sent the whole body. `u64::MAX` until then.
    worker_ms: Arc<AtomicU64>,
    bytes_in: Option<u64>,
    bytes_out: u64,
    expected_out: Option<u64>,
    /// What the worker measured for a request that had it to itself: its memory cost in KiB,
    /// the CPU it used and the time it took until the response started, in microseconds, and
    /// for a profiled request, its CPU use over time.
    mem_kb: Option<u64>,
    cpu_us: Option<u64>,
    busy_us: Option<u64>,
    cpu_profile: Option<String>,
    /// The deployment's environment, runtime, entry, memory size and time limit, once the host is
    /// looked up.
    function: Option<FunctionLog>,
    /// How it got its worker: `warm` (an open one), `thaw` or `cold` (it waited for that one to
    /// start).
    start: Option<&'static str>,
    /// The worker's resident memory when the response started, in KiB (the runtime and the app).
    rss_kb: Option<u64>,
    /// The app's route template, from the shim.
    route: Option<String>,
    /// The deployment's routing rule that decided: where it came from in `vercel.json`
    /// (`redirects[3]`).
    rule: Option<String>,
    /// The schedule's run it is, if it's one.
    schedule: Option<crate::schedules::RunInfo>,
    /// What the firewall did (`deny`, `redirect`, `bypass`, `log`), and the rule or IP block that
    /// did it, by id; `held` when a persistent block answered.
    firewall: Option<&'static str>,
    firewall_rule: Option<String>,
    /// The TLS client's JA4 fingerprint (`ja4.rs`); none over plain HTTP.
    ja4: Option<Arc<str>>,
    /// What skew protection made of a request that named a deployment (`skew::Outcome`):
    /// `pinned` when that deployment served it rather than the host's.
    skew: Option<&'static str>,
    /// What the deployment's Routing Middleware said (`next`, `rewrite`, `external`, `redirect`,
    /// `answer`) or that it `failed`, `timeout`, `unavailable`; and how long routing spent on it.
    middleware: Option<&'static str>,
    middleware_ms: Option<u64>,
    /// Its deployment's log budgets, once it's headed for a worker: they know how many of the
    /// app's lines and outgoing calls for it weren't kept.
    app_logs: Option<Arc<LogLimiter>>,
    /// What the response cache did (`x-vercel-cache`'s value), or `background` for a page or
    /// answer the router made again itself.
    cache: Option<&'static str>,
    written: bool,
    /// A request another router sent to run here (`relay.rs`): that router writes its line.
    quiet: bool,
    /// Run by another router (`relay.rs`): what crossed to it and back is the project's, between
    /// regions, as a worker's tunnel from there would have been.
    relayed: bool,
}

/// What the access log says about the function that served a request.
struct FunctionLog {
    environment: Option<String>,
    runtime: Option<String>,
    entry: Option<String>,
    memory_mb: Option<u32>,
    max_duration_s: u64,
}

impl FunctionLog {
    fn of(cfg: &crate::pool::PoolConfig) -> Self {
        let some = |s: &str| Some(s.to_owned()).filter(|s| !s.is_empty());
        Self {
            environment: some(&cfg.environment),
            runtime: some(&cfg.runtime),
            entry: some(&cfg.entry),
            memory_mb: Some(cfg.memory_mb).filter(|&m| m > 0),
            max_duration_s: cfg.max_duration.as_secs(),
        }
    }
}

impl AccessLog {
    fn new(req: &Request<Incoming>, peer: SocketAddr) -> Self {
        let agent = req.headers().get(hyper::header::USER_AGENT).map(|v| truncate(&String::from_utf8_lossy(v.as_bytes()), MAX_LOGGED_AGENT));
        Self {
            request_id: forkrail_common::quick_id(),
            meter: req.extensions().get::<Arc<crate::spend::Conn>>().cloned(),
            entry: Entry::Tester,
            host: String::new(),
            deployment: None,
            project: String::new(),
            region: String::new(),
            method: req.method().clone(),
            path: mask_path(req.uri().path()),
            query: req.uri().query().filter(|q| !q.is_empty()).map(|q| truncate(&mask_query(q), MAX_LOGGED_QUERY)),
            client: peer.ip().to_canonical().to_string(),
            location: Location::default(),
            referer: req.headers().get(REFERER).map(|v| truncate(&mask_url(&String::from_utf8_lossy(v.as_bytes())), MAX_LOGGED_QUERY)),
            proto: match req.version() {
                Version::HTTP_2 => "h2",
                Version::HTTP_10 => "h1.0",
                _ => "h1.1",
            },
            agent,
            accept_encoding: header_text(req.headers(), ACCEPT_ENCODING),
            encoding: None,
            started: Instant::now(),
            attempts: 0,
            queue: Duration::ZERO,
            worker: None,
            status: 0,
            ttfb: None,
            worker_ms: Arc::new(AtomicU64::new(u64::MAX)),
            bytes_in: None,
            bytes_out: 0,
            expected_out: None,
            mem_kb: None,
            cpu_us: None,
            busy_us: None,
            cpu_profile: None,
            function: None,
            start: None,
            rss_kb: None,
            route: None,
            rule: None,
            schedule: None,
            firewall: None,
            firewall_rule: None,
            ja4: None,
            skew: None,
            middleware: None,
            middleware_ms: None,
            app_logs: None,
            cache: None,
            written: false,
            quiet: false,
            relayed: false,
        }
    }

    /// What's left in place of a log line that was taken and written: writes nothing.
    fn spent() -> Self {
        Self {
            request_id: String::new(),
            meter: None,
            entry: Entry::Tester,
            host: String::new(),
            deployment: None,
            project: String::new(),
            region: String::new(),
            method: Method::GET,
            path: String::new(),
            query: None,
            client: String::new(),
            referer: None,
            location: Location::default(),
            proto: "",
            agent: None,
            accept_encoding: None,
            encoding: None,
            started: Instant::now(),
            attempts: 0,
            queue: Duration::ZERO,
            worker: None,
            status: 0,
            ttfb: None,
            worker_ms: Arc::new(AtomicU64::new(u64::MAX)),
            bytes_in: None,
            bytes_out: 0,
            expected_out: None,
            mem_kb: None,
            cpu_us: None,
            busy_us: None,
            cpu_profile: None,
            function: None,
            start: None,
            rss_kb: None,
            route: None,
            rule: None,
            schedule: None,
            firewall: None,
            firewall_rule: None,
            ja4: None,
            skew: None,
            middleware: None,
            middleware_ms: None,
            app_logs: None,
            cache: None,
            written: true,
            quiet: false,
            relayed: false,
        }
    }

    /// For responses the router makes itself.
    fn finish_now(mut self, mut response: Response<ResponseBody>, outcome: &str) -> Response<ResponseBody> {
        self.status = response.status().as_u16();
        self.tag(response.headers_mut());
        self.write(outcome);
        response
    }

    /// Name the deployment that answered, and the request's id in the access log, so a client's
    /// report can be matched to its line.
    fn tag(&self, headers: &mut HeaderMap) {
        if let Some(value) = self.deployment.as_deref().and_then(|d| HeaderValue::from_str(d).ok()) {
            headers.insert(DEPLOYMENT_HEADER, value);
        }
        headers.insert(REQUEST_ID, HeaderValue::from_str(&self.request_id).expect("hex is a valid header"));
    }

    fn write(mut self, outcome: &str) {
        self.emit(outcome);
        self.written = true;
    }

    /// For a beacon written as its own line (`insights.rs`): its bytes count for the project as a
    /// request's do, and no request line is written.
    fn finish_unlogged(mut self, response: Response<ResponseBody>) -> Response<ResponseBody> {
        if let Some(meter) = &self.meter {
            meter.served(&self.spend_owner(), self.bytes_in.unwrap_or(0) + self.bytes_out + 512);
        }
        self.written = true;
        response
    }

    /// Whose the request's bytes on its connection are: its project's (the install's until the
    /// host is known), priced where this router is.
    fn spend_owner(&self) -> crate::spend::Owner {
        crate::spend::Owner::new(&self.project, self.deployment.as_deref().unwrap_or_default(), crate::spend::ledger().region())
    }

    /// Tell the connection's meter whose it is from now on (a long answer's bytes are counted
    /// while it streams, not only once it ends).
    fn spend_begin(&self) {
        if let Some(meter) = &self.meter {
            meter.served(&self.spend_owner(), 1);
        }
    }

    fn emit(&self, outcome: &str) {
        if let Some(meter) = &self.meter {
            // Headers aren't in either count; a few hundred bytes each way is near enough to share by.
            meter.served(&self.spend_owner(), self.bytes_in.unwrap_or(0) + self.bytes_out + 512);
        }
        if self.relayed {
            let owner = crate::spend::Owner::new(&self.project, self.deployment.as_deref().unwrap_or_default(), &self.region);
            crate::spend::ledger().tunnel_bytes(&owner, self.bytes_in.unwrap_or(0) + 512, self.bytes_out + 512);
        }
        let ms = |d: Duration| d.as_millis() as u64;
        let worker_ms = Some(self.worker_ms.load(Ordering::Relaxed)).filter(|&v| v != u64::MAX);
        // Lines that come after this one aren't counted: the app's last usually beat its response.
        let dropped = self.app_logs.as_ref().map(|logs| logs.end(&self.request_id)).unwrap_or_default();
        if self.quiet {
            return;
        }
        info!(
            request = %self.request_id,
            entry = self.entry.name(),
            host = %self.host,
            deployment = self.deployment.as_deref(),
            project = Some(self.project.as_str()).filter(|p| !p.is_empty()),
            region = Some(self.region.as_str()).filter(|r| !r.is_empty()),
            method = %self.method,
            path = %self.path,
            query = self.query.as_deref(),
            client = %self.client,
            country = self.location.country.as_deref(),
            continent = self.location.continent.as_deref(),
            subdivision = self.location.subdivision.as_deref(),
            city = self.location.city.as_deref(),
            latitude = self.location.latitude,
            longitude = self.location.longitude,
            asn = self.location.asn,
            as_org = self.location.as_org.as_deref(),
            referer = self.referer.as_deref(),
            proto = self.proto,
            agent = self.agent.as_deref(),
            accept_encoding = self.accept_encoding.as_deref(),
            encoding = self.encoding.as_deref(),
            status = self.status,
            outcome,
            attempts = self.attempts,
            queue_ms = ms(self.queue),
            ttfb_ms = self.ttfb.map(ms),
            total_ms = ms(self.started.elapsed()),
            worker_ms,
            bytes_in = self.bytes_in,
            bytes_out = self.bytes_out,
            worker = self.worker.as_deref(),
            mem_kb = self.mem_kb,
            cpu_us = self.cpu_us,
            busy_us = self.busy_us,
            cpu_profile = self.cpu_profile.as_deref(),
            rss_kb = self.rss_kb,
            route = self.route.as_deref(),
            rule = self.rule.as_deref(),
            schedule = self.schedule.as_ref().map(|r| r.key.as_str()),
            schedule_run = self.schedule.as_ref().map(crate::schedules::schedule_run),
            schedule_delay_ms = self.schedule.as_ref().and_then(|r| r.delay_ms),
            schedule_queue_ms = self.schedule.as_ref().and_then(|r| r.queue_ms),
            firewall = self.firewall,
            firewall_rule = self.firewall_rule.as_deref(),
            ja4 = self.ja4.as_deref(),
            skew = self.skew,
            cache = self.cache,
            middleware = self.middleware,
            middleware_ms = self.middleware_ms,
            start = self.start,
            environment = self.function.as_ref().and_then(|f| f.environment.as_deref()),
            runtime = self.function.as_ref().and_then(|f| f.runtime.as_deref()),
            entrypoint = self.function.as_ref().and_then(|f| f.entry.as_deref()),
            memory_mb = self.function.as_ref().and_then(|f| f.memory_mb),
            max_duration_s = self.function.as_ref().map(|f| f.max_duration_s),
            lines_dropped = Some(dropped.lines).filter(|&n| n > 0),
            calls_dropped = Some(dropped.calls).filter(|&n| n > 0),
            "request"
        );
    }
}

impl Drop for AccessLog {
    fn drop(&mut self) {
        if !self.written {
            self.emit("client_gone");
        }
    }
}

/// The request body as it goes to the worker.
enum UpstreamBody {
    /// Read in full, so it can be sent again, with its room in the router-wide buffer.
    Buffered { bytes: Bytes, _room: Option<OwnedSemaphorePermit> },
    /// Passed through as it arrives. `None` while a send attempt has it.
    Streaming(Option<ProxyBody>),
}

impl UpstreamBody {
    fn take(&mut self) -> Option<ProxyBody> {
        match self {
            Self::Buffered { bytes, .. } => Some(Full::new(bytes.clone()).map_err(|never| match never {}).boxed()),
            Self::Streaming(body) => body.take(),
        }
    }

    /// A send that never reached the worker hands the body back.
    fn put_back(&mut self, body: ProxyBody) {
        if let Self::Streaming(slot) = self {
            *slot = Some(body);
        }
    }

    fn replayable(&self) -> bool {
        matches!(self, Self::Buffered { .. })
    }

    fn len(&self) -> Option<u64> {
        match self {
            Self::Buffered { bytes, .. } => Some(bytes.len() as u64),
            Self::Streaming(_) => None,
        }
    }
}

/// Why a request's body wasn't read.
pub(crate) enum Unread {
    /// Past the function payload limit (`max_body`): answered as Vercel answers it, 413.
    TooLarge,
    Failed(hyper::Error),
}

/// Read a body for the function, at most `max` bytes (`max_body`): one of known, modest size in
/// full, so a slow upload waits on the router rather than on a billed worker; one larger than the
/// router reads for itself (`FORKRAIL_BUFFER_BYTES`) passes through as it arrives. A body without
/// a length is read in full too, as Vercel reads every body before its function starts (seen: a
/// chunked upload's 405 came only once it ended), so one past the limit is refused, not cut short.
/// What's left of one past the limit is read and thrown away first (`drain_refused`).
async fn read_body(mut body: Incoming, headers: &HeaderMap, proxy: &Proxy, max: u64, version: Version) -> Result<UpstreamBody, Unread> {
    if body.is_end_stream() {
        return Ok(UpstreamBody::Buffered { bytes: Bytes::new(), _room: None });
    }
    let length = headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if length.is_some_and(|n| n > max) {
        drain_refused(body, version, headers, 0).await;
        return Err(Unread::TooLarge);
    }
    if length.is_some_and(|n| n > proxy.buffer_bytes as u64) {
        return Ok(UpstreamBody::Streaming(Some(body.boxed())));
    }
    let mut bytes = BytesMut::new();
    let mut room: Option<OwnedSemaphorePermit> = None;
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame.map_err(Unread::Failed)?.into_data() else { continue };
        if (bytes.len() + data.len()) as u64 > max {
            let read = (bytes.len() + data.len()) as u64;
            drop((bytes, room));
            drain_refused(body, version, headers, read).await;
            return Err(Unread::TooLarge);
        }
        let permit = proxy.room(data.len()).await;
        match &mut room {
            Some(room) => room.merge(permit),
            None => room = Some(permit),
        }
        bytes.extend_from_slice(&data);
    }
    Ok(UpstreamBody::Buffered { bytes: bytes.freeze(), _room: room })
}

/// The visitor a request is a function's call for, when it carries the router's vouch for that
/// visitor on the same project (`visitor.rs`).
/// The router's vouch as `@vercel/firewall` passes it on, among the request's headers it copies.
const COPIED_VOUCH: &str = "x-rr-x-forkrail-visitor";

/// The visitor a request's vouch in `header` names, for the project and environment its host
/// serves, and when the vouch expires.
fn vouched(proxy: &Proxy, req: &Request<Incoming>, peer: SocketAddr, header: &str) -> Option<(SocketAddr, u64)> {
    let value = req.headers().get(header)?.to_str().ok()?;
    let (project, environment) = proxy.registry.project_of(&normalize_host(&request_host(req)))?;
    let (ip, expires) = proxy.vouches.vouched(value, &project, &environment, unix_now())?;
    Some((SocketAddr::new(ip, peer.port()), expires))
}

/// How much more of a body past the limit `drain_refused` reads, and for how long at most (nginx's
/// `lingering_time` is 30 s too).
const DRAIN_BYTES: u64 = 64 << 20;
const DRAIN_FOR: Duration = Duration::from_secs(30);

/// Read and throw away the rest of a body past the limit before its 413, as Vercel reads a body
/// whole before answering: an HTTP/1.1 client that sends all of it before reading the answer then
/// gets the 413, where closing the connection on unread bytes resets it (seen on AWS 2026-11-16: a
/// broken pipe instead of the 413). Not over HTTP/2 (the stream is reset on its own, the
/// connection goes on), nor for a client that waits for `100 Continue` and so has sent nothing
/// (`read` 0): it reads the 413 and never sends the body. At most `DRAIN_BYTES` more, for at most
/// `DRAIN_FOR`; past either the connection closes as before.
pub(crate) async fn drain_refused(mut body: Incoming, version: Version, headers: &HeaderMap, read: u64) {
    if !matches!(version, Version::HTTP_10 | Version::HTTP_11) || body.is_end_stream() {
        return;
    }
    let waits_to_send = headers.get(hyper::header::EXPECT).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("100-continue"));
    if read == 0 && waits_to_send {
        return;
    }
    let _ = tokio::time::timeout(DRAIN_FOR, async {
        let mut thrown = 0u64;
        while let Some(Ok(frame)) = body.frame().await {
            thrown += frame.data_ref().map_or(0, |d| d.len() as u64);
            if thrown > DRAIN_BYTES {
                break;
            }
        }
    })
    .await;
}

/// Requests a worker may already have acted on are repeated only when that's harmless.
/// If the worker reset this request's stream (RST_STREAM from its side), the reason. A dead
/// tunnel shows as an I/O error, a closed connection or GOAWAY instead.
fn stream_reset(error: &hyper::Error) -> Option<h2::Reason> {
    let mut source = std::error::Error::source(error);
    while let Some(e) = source {
        if let Some(h2) = e.downcast_ref::<h2::Error>() {
            return (h2.is_reset() && h2.is_remote()).then(|| h2.reason()).flatten();
        }
        source = e.source();
    }
    None
}

/// An error and its causes, as one line: hyper's own message ("http2 error") hides what happened.
fn error_chain(error: &hyper::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(e) = source {
        text.push_str(": ");
        text.push_str(&e.to_string());
        source = e.source();
    }
    text
}

/// One header field (name, `: `, value and CRLF) may be 32 KiB, as on Vercel; past it, its
/// `494 REQUEST_HEADER_TOO_LARGE`.
const MAX_HEADER_FIELD: usize = 32 << 10;
/// The request line and every header field together. Vercel's own limit counts the headers its edge
/// adds too, so where it falls moves by a few bytes; measured 2026-11-16 on a Vercel deployment,
/// it took 127,499 bytes of request line and fields and refused 127,500 (an empty 431).
const MAX_REQUEST_HEAD: usize = 127_499;

/// Vercel's answer to a request whose header fields are too big, if they are: refused before any
/// worker is chosen (a block too big for the tunnel would end it, and every request on it).
fn oversized_head<B>(req: &Request<B>) -> Option<Response<ResponseBody>> {
    let target = req.uri().path_and_query().map_or(1, |pq| pq.as_str().len());
    // `GET /path HTTP/1.1` and its CRLF.
    let mut total = req.method().as_str().len() + 1 + target + 11;
    if !req.headers().contains_key(HOST)
        && let Some(authority) = req.uri().authority()
    {
        total += "host: ".len() + authority.as_str().len() + 2;
    }
    for (name, value) in req.headers() {
        let field = name.as_str().len() + 2 + value.len() + 2;
        if field > MAX_HEADER_FIELD {
            let mut response = coded(StatusCode::from_u16(494).expect("a valid status"), "REQUEST_HEADER_TOO_LARGE", "An error occurred");
            response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=0, must-revalidate"));
            return Some(response);
        }
        total += field;
    }
    (total > MAX_REQUEST_HEAD).then(|| {
        let mut response = error(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE, "");
        response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=0, must-revalidate"));
        response
    })
}

/// Whether a tunnel failed over a header block too big to take (the worker's answer's, now that
/// the router refuses big requests itself): the same request would do it again on any worker.
fn header_overflow(error: &hyper::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(e) = source {
        if let Some(h2) = e.downcast_ref::<h2::Error>() {
            let text = h2.to_string();
            return h2.reason() == Some(h2::Reason::ENHANCE_YOUR_CALM) || text.contains("continuation") || text.contains("header_list");
        }
        source = e.source();
    }
    false
}

fn safe_to_repeat(head: &UpstreamHead) -> bool {
    matches!(head.method, Method::GET | Method::HEAD | Method::OPTIONS) || head.headers.contains_key(IDEMPOTENCY_KEY)
}

/// Everything of the upstream request but its body, so it can be sent again.
#[derive(Clone)]
struct UpstreamHead {
    /// The `Host` the client sent, as it sent it.
    host: String,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    /// What the client's request weighs towards a function's payload limit without its body
    /// (`payload_head`): 0 for a request the router makes itself.
    payload: u64,
}

impl UpstreamHead {
    fn request(&self, body: ProxyBody) -> Request<ProxyBody> {
        let mut request = Request::new(body);
        *request.method_mut() = self.method.clone();
        *request.uri_mut() = self.uri.clone();
        *request.version_mut() = Version::HTTP_2;
        *request.headers_mut() = self.headers.clone();
        request
    }
}

fn to_upstream(req: Request<Incoming>, peer: SocketAddr, request_id: &str, entry: Entry, location: &Location, region: &str) -> (UpstreamHead, Incoming) {
    let (mut parts, body) = req.into_parts();
    let host = parts
        .headers
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .or_else(|| parts.uri.authority().map(|a| a.to_string()))
        .unwrap_or_else(|| "localhost".to_owned());
    // As the client sent it, before anything here is taken off or added.
    let payload = payload_head(parts.uri.path_and_query().map_or("/", |pq| pq.as_str()), &host, &parts.headers);
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.remove(HOST);
    // Only the router says a request is a schedule's run, or whose it is (`handle` adds its own).
    parts.headers.remove(crate::visitor::HEADER);
    parts.headers.remove(crate::loops::HEADER);
    parts.headers.remove(SCHEDULE_HEADER);
    parts.headers.remove(crate::schedules::RUN_HEADER);
    // Only the router asks a worker for a CPU profile (`FORKRAIL_PROFILE_PCT`).
    parts.headers.remove(PROFILE_REQUEST_HEADER);

    // HTTP/2 carries the host in :authority, so the URI must be absolute.
    let path_and_query = parts.uri.path_and_query().map_or("/", |pq| pq.as_str());
    let uri = format!("http://{host}{path_and_query}")
        .parse()
        .or_else(|_| format!("http://localhost{path_and_query}").parse())
        .expect("path and query came from a valid URI");

    let headers = &mut parts.headers;
    // Headers only the platform sets come from the router alone, as from Vercel's edge (the
    // client's location among them). A schedule's run is the router's own request, whose cron
    // expression Nitro's handler checks (`x-vercel-cron-schedule`).
    let spoofed: Vec<HeaderName> = headers
        .keys()
        .filter(|k| {
            (platform_only(k.as_str()) && !(entry == Entry::Schedule && (forkrail_common::schedules::EXPRESSION_HEADERS.contains(&k.as_str()) || k.as_str().starts_with("ce-vss"))))
                || aliases_platform(k.as_str())
        })
        .cloned()
        .collect();
    for name in spoofed {
        headers.remove(name);
    }
    let client = peer.ip().to_canonical().to_string();
    if let Ok(value) = HeaderValue::from_str(&client) {
        headers.insert(HeaderName::from_static("x-forwarded-for"), value.clone());
        headers.insert(HeaderName::from_static("x-vercel-forwarded-for"), value.clone());
        headers.insert(HeaderName::from_static("x-real-ip"), value);
    }
    // The regions the request went through and the request's id, as Vercel's `x-vercel-id`
    // (`iad1::iad1::<id>`): `@vercel/functions`' `geolocation().region` is its first part.
    if let Some(code) = forkrail_common::vercel_region(region)
        && let Ok(value) = HeaderValue::from_str(&format!("{code}::{code}::{request_id}"))
    {
        headers.insert(HeaderName::from_static("x-vercel-id"), value);
    }
    for (name, value) in geo::headers(location) {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&host) {
        headers.insert(HeaderName::from_static("x-forwarded-host"), value);
    }
    let proto = if matches!(entry, Entry::Https | Entry::Schedule) { "https" } else { "http" };
    headers.insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_static(proto));
    // The port the visitor asked on, as Vercel's edge sends it: an app's own server would
    // otherwise fill in the port it listens on (Next.js does), and a client's own went through.
    let port = forwarded_port(&host, proto);
    if let Ok(value) = HeaderValue::from_str(&port) {
        headers.insert(HeaderName::from_static("x-forwarded-port"), value);
    }
    // RFC 7239's `Forwarded` and `True-Client-IP` say what the router saw too, and the other names
    // apps read a visitor's address from go: a visitor's own would pass for the router's (Spring's
    // and Quarkus' forwarding filters read `Forwarded`, chi's RealIP `True-Client-IP` first).
    for name in ["x-client-ip", "x-cluster-client-ip", "client-ip", "fastly-client-ip"] {
        headers.remove(name);
    }
    let node = match peer.ip().to_canonical() {
        std::net::IpAddr::V6(v6) => format!("\"[{v6}]\""),
        v4 => v4.to_string(),
    };
    let named = if host.contains(['"', '\\']) { String::new() } else { format!(";host=\"{host}\"") };
    match HeaderValue::from_str(&format!("for={node}{named};proto={proto}")) {
        Ok(value) => headers.insert(HeaderName::from_static("forwarded"), value),
        Err(_) => headers.remove("forwarded"),
    };
    match HeaderValue::from_str(&client) {
        Ok(value) => headers.insert(HeaderName::from_static("true-client-ip"), value),
        Err(_) => headers.remove("true-client-ip"),
    };
    headers.insert(REQUEST_ID, HeaderValue::from_str(request_id).expect("hex is a valid header"));
    (UpstreamHead { host, method: parts.method, uri, headers: parts.headers, payload }, body)
}

/// `x-forwarded-port` for a request to `host` (as the client sent it): the port it names, else the
/// scheme's own.
fn forwarded_port(host: &str, proto: &str) -> String {
    let named = host.rsplit_once(':').map(|(_, port)| port).filter(|port| !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()));
    named.unwrap_or(if proto == "https" { "443" } else { "80" }).to_owned()
}

/// Vercel's limit on what a function is sent, measured on forkrail-check-fw-sveltekit (2026-11-16,
/// POSTs to `/api/env` with curl's few headers): a body of 4,493,999 bytes ran and 4,494,000 got
/// `413 FUNCTION_PAYLOAD_TOO_LARGE`, also when sent without a length (Vercel reads the whole body
/// before the function starts, and stops at the limit); a 1,000-byte header lowered the largest
/// body by 758 bytes, a 1,000-character query by 752. So the body counts as base64 (4/3 of its
/// bytes) and the rest as it is, up to 6,000,000 bytes: Lambda's invocation payload. Its docs say
/// "4.5 MB", for the body.
const PAYLOAD_LIMIT: u64 = 6_000_000;
/// What Vercel adds to every invocation's payload itself (its own headers): the measurements
/// above fit 7,834 bytes exactly, all three to the byte.
const PLATFORM_PAYLOAD: u64 = 7_834;

/// What a request weighs towards the function payload limit without its body: its path with
/// the query, each header as `"name":"value",` (the host's among them), and Vercel's own part.
fn payload_head(path_and_query: &str, host: &str, headers: &HeaderMap) -> u64 {
    let header = |name: usize, value: usize| (name + value + 6) as u64;
    let others: u64 = headers.iter().filter(|(name, _)| *name != HOST).map(|(name, value)| header(name.as_str().len(), value.len())).sum();
    path_and_query.len() as u64 + header(4, host.len()) + others + PLATFORM_PAYLOAD
}

/// The largest body a request whose rest weighs `head` (`payload_head`) may send a function:
/// the body's base64 and `head` together stay under `PAYLOAD_LIMIT`.
pub(crate) fn max_body(head: u64) -> u64 {
    (PAYLOAD_LIMIT * 3).saturating_sub(3 * head + 1) / 4
}

/// Vercel's answer to a body over the limit, with the routes' headers and, as on Vercel, the
/// default caching headers of a function's answer.
fn payload_too_large(proxy: &Proxy, effects: &Effects) -> Response<ResponseBody> {
    let mut response = respond_headers(coded(StatusCode::PAYLOAD_TOO_LARGE, "FUNCTION_PAYLOAD_TOO_LARGE", "Request Entity Too Large"), &effects.headers, &effects.transforms);
    if let Some(cache) = &proxy.cache {
        crate::cache::uncacheable_headers(response.headers_mut(), crate::cache::Status::Miss, cache.defaults);
    }
    response
}

/// Where Let's Encrypt asks for an HTTP-01 key authorization.
const ACME_CHALLENGE_PATH: &str = "/.well-known/acme-challenge/";

/// Port 80: the key authorization of a custom domain's HTTP-01 challenge, from the store every
/// router reads (so Let's Encrypt may reach either region); everything else as `redirect`.
pub async fn port80<B>(req: Request<B>, proxy: Arc<Proxy>, peer: SocketAddr) -> Response<ResponseBody> {
    if req.uri().path() != HEALTH_PATH
        && let Some(blocked) = proxy.shield.request(peer.ip(), &request_host(&req), || proxy.registry.project_of(&request_host(&req)).map(|p| p.0))
    {
        return refused_by_shield(blocked);
    }
    if let (Some(token), Some(challenges)) = (req.uri().path().strip_prefix(ACME_CHALLENGE_PATH), &proxy.challenges)
        && matches!(*req.method(), Method::GET | Method::HEAD)
    {
        let host = req.headers().get(HOST).and_then(|h| h.to_str().ok()).map(normalize_host).unwrap_or_default();
        // Only the table's custom domains, and only tokens shaped like Let's Encrypt's: anything
        // else costs no read.
        let found = if crate::acme::valid_token(token) && proxy.registry.is_custom_domain(&host) { challenges.get(token).await } else { None };
        return match found {
            Some(body) => {
                info!(%host, token, "answered an HTTP challenge");
                let mut response = Response::new(Full::new(body).map_err(|never| match never {}).boxed());
                response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
                response
            }
            None => error(StatusCode::NOT_FOUND, "no such challenge\n"),
        };
    }
    // The firewall's IP blocks, persistent blocks and denying rules hold here too; anything else
    // it would do waits for the HTTPS request, and rate limits count that one only.
    let host = req.headers().get(HOST).and_then(|h| h.to_str().ok()).map(normalize_host).unwrap_or_default();
    if let Some((project, environment)) = proxy.registry.project_of(&host)
        && proxy.firewalls.active(&project)
    {
        let location = proxy.geo.lookup(peer.ip().to_canonical());
        let judged = proxy.firewalls.judge(&project, &firewall_request(&FirewallInput {
            method: req.method().as_str(),
            host: &host,
            uri: req.uri(),
            headers: req.headers(),
            peer,
            protocol: protocol_name(req.version()),
            scheme: "http",
            environment: &environment,
            region: &proxy.registry.here(),
            location: &location,
            target_path: None,
            rate_limit_api: None,
            challenge_passed: false,
            ja4: None,
        }), false);
        match judged.decision {
            Decision::Deny { .. } => {
                info!(%host, client = %peer.ip().to_canonical(), project, rule = judged.rule.as_deref(), held = judged.held, "firewall: refused on port 80");
                return denied_by_firewall();
            }
            Decision::RateLimited { retry_after, .. } => return rate_limited(retry_after),
            _ => {}
        }
    }
    redirect(&req)
}

/// What the firewall needs of a request, from either listener.
struct FirewallInput<'a> {
    method: &'a str,
    host: &'a str,
    uri: &'a Uri,
    headers: &'a HeaderMap,
    peer: SocketAddr,
    protocol: &'static str,
    scheme: &'static str,
    environment: &'a str,
    region: &'a str,
    location: &'a Location,
    target_path: Option<&'a str>,
    rate_limit_api: Option<ApiCheck<'a>>,
    challenge_passed: bool,
    ja4: Option<&'a str>,
}

fn firewall_request<'a>(input: &FirewallInput<'a>) -> forkrail_common::firewall::Request<'a> {
    forkrail_common::firewall::Request {
        method: input.method,
        host: input.host,
        raw_path: input.uri.path(),
        target_path: input.target_path,
        query: input.uri.query(),
        headers: input.headers,
        ip: input.peer.ip().to_canonical(),
        protocol: input.protocol,
        scheme: input.scheme,
        environment: input.environment,
        region: input.region,
        continent: input.location.continent.as_deref(),
        country: input.location.country.as_deref(),
        country_region: input.location.subdivision.as_deref(),
        city: input.location.city.as_deref(),
        asn: input.location.asn,
        ja4: input.ja4,
        rate_limit_api: input.rate_limit_api,
        challenge_passed: input.challenge_passed,
    }
}

fn protocol_name(version: Version) -> &'static str {
    match version {
        Version::HTTP_2 => "HTTP/2.0",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/1.1",
    }
}

/// Judge a request by its project's firewall (and the install's): the router's answer if the
/// firewall answers it, with what it did in the access log either way.
#[allow(clippy::too_many_arguments)]
fn screen(
    proxy: &Proxy,
    project: &str,
    environment: &str,
    head: &UpstreamHead,
    peer: SocketAddr,
    entry: Entry,
    target_path: Option<&str>,
    log: &mut AccessLog,
) -> Option<Response<ResponseBody>> {
    let protocol = match log.proto {
        "h2" => "HTTP/2.0",
        "h1.0" => "HTTP/1.0",
        _ => "HTTP/1.1",
    };
    let region = proxy.registry.here();
    let agent = head.headers.get(hyper::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    let now = unix_now();
    let passed = proxy.challenger.passed(&head.headers, peer.ip(), agent, &log.host, now);
    // Attack mode challenges every visitor without a pass, before any rule; verified crawlers and
    // the system bypasses pass.
    if !passed && proxy.shield.attack_mode(project) && !proxy.shield.exempt(peer.ip(), &log.host, project) && !proxy.bots.verified(peer.ip()) {
        log.firewall = Some("challenge");
        log.firewall_rule = Some("attack_mode".to_owned());
        return Some(proxy.challenger.page(peer.ip(), agent, &log.host, now));
    }
    let request = firewall_request(&FirewallInput {
        method: head.method.as_str(),
        host: &log.host,
        uri: &head.uri,
        headers: &head.headers,
        peer,
        protocol,
        scheme: if entry == Entry::Https { "https" } else { "http" },
        environment,
        region: &region,
        location: &log.location,
        target_path,
        rate_limit_api: None,
        challenge_passed: passed,
        ja4: log.ja4.as_deref(),
    });
    let Judged { decision, rule, held } = proxy.firewalls.judge(project, &request, true);
    if decision == Decision::Allow {
        return None;
    }
    log.firewall = Some(if held { "held" } else { decision.name() });
    log.firewall_rule = rule;
    match decision {
        Decision::Deny { .. } => Some(denied_by_firewall()),
        Decision::RateLimited { retry_after, .. } => Some(rate_limited(retry_after)),
        Decision::Challenge { .. } => Some(proxy.challenger.page(peer.ip(), agent, &log.host, now)),
        Decision::Redirect { location, status } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::TEMPORARY_REDIRECT);
            let mut response = redirect_response(status, &location, log);
            response.headers_mut().insert(MITIGATED_HEADER, HeaderValue::from_static("redirect"));
            Some(response)
        }
        Decision::Allow | Decision::Log | Decision::Bypass => None,
    }
}

/// Says on a response that the firewall made it, and how (`deny`, `rate_limit`, `redirect`).
const MITIGATED_HEADER: HeaderName = HeaderName::from_static("x-forkrail-mitigated");

/// Check a browser's answer to the challenge (`seed=…&nonce=…`, at most 1 KB): 204 with the pass,
/// or 403.
async fn solve_challenge(proxy: &Proxy, head: &UpstreamHead, body: Incoming, peer: SocketAddr, entry: Entry, log: &AccessLog) -> (Response<ResponseBody>, &'static str) {
    let form = match http_body_util::Limited::new(body, 1024).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return (coded(StatusCode::BAD_REQUEST, "BAD_REQUEST", "That isn't an answer to the challenge."), "challenge_failed"),
    };
    let agent = head.headers.get(hyper::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
    match proxy.challenger.solve(&String::from_utf8_lossy(&form), peer.ip(), agent, &log.host, unix_now(), entry == Entry::Https) {
        Ok(cookie) => (crate::challenge::solved(cookie), "challenge_solved"),
        Err(why) => {
            info!(host = %log.host, client = %peer.ip().to_canonical(), reason = ?why, "challenge answer refused");
            let mut response = coded(StatusCode::FORBIDDEN, "CHALLENGE_FAILED", "The answer to the challenge didn't hold. Reload the page to try again.");
            response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            (response, "challenge_failed")
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The TLS client's JA4 fingerprint, put on each request of its connection.
#[derive(Clone)]
pub struct Ja4(pub Arc<str>);

/// The host a request names, normalised: HTTP/2's `:authority`, else `Host`.
fn request_host<B>(req: &Request<B>) -> String {
    req.uri().host().or_else(|| req.headers().get(HOST).and_then(|h| h.to_str().ok())).map(normalize_host).unwrap_or_default()
}

/// The system mitigations' answer to an address they block: 429 until the block ends, and the
/// connection closed after it (HTTP/1).
fn refused_by_shield(blocked: Duration) -> Response<ResponseBody> {
    let mut response = coded(StatusCode::TOO_MANY_REQUESTS, "TOO_MANY_REQUESTS", "Too many requests from this address. Try again later.");
    let headers = response.headers_mut();
    headers.insert(MITIGATED_HEADER, HeaderValue::from_static("system"));
    headers.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
    headers.insert(RETRY_AFTER, HeaderValue::from(blocked.as_secs().max(1)));
    response
}

/// The firewall's 429, with when to try again.
fn rate_limited(retry_after: Duration) -> Response<ResponseBody> {
    let mut response = coded(StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED", "Too many requests. Try again later.");
    let headers = response.headers_mut();
    headers.insert(MITIGATED_HEADER, HeaderValue::from_static("rate_limit"));
    headers.insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    let seconds = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
    headers.insert(RETRY_AFTER, HeaderValue::from(seconds.max(1)));
    response
}

/// Where an app asks the firewall about its rate limits, under any path prefix: ours, and the one
/// Vercel's `@vercel/firewall` package calls (so an app moved from Vercel keeps working).
const RATE_LIMIT_API: [&str; 2] = ["/.well-known/forkrail/rate-limit-api/", "/.well-known/vercel/rate-limit-api/"];
/// The client's key the app sends with the question.
const RATE_LIMIT_KEY: [&str; 2] = ["x-forkrail-rate-limit-key", "x-vercel-rate-limit-key"];

/// The rate-limit id a request asks about, if it's such a question.
fn rate_limit_api_id(path: &str) -> Option<String> {
    let rest = RATE_LIMIT_API.iter().find_map(|prefix| path.find(prefix).map(|at| &path[at + prefix.len()..]))?;
    Some(forkrail_common::rules::convert::percent_decode(rest))
}

/// Answer an app's rate-limit question: 204 within the limit, 429 over it, 403 when a rule
/// denies, 404 when no rule names the id (as `@vercel/firewall` expects).
#[allow(clippy::too_many_arguments)]
fn rate_limit_api(proxy: &Proxy, target: &Target, head: &UpstreamHead, peer: SocketAddr, entry: Entry, id: &str, log: &mut AccessLog) -> Response<ResponseBody> {
    let answer = |status: StatusCode| {
        let mut response = error(status, "");
        response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
        response
    };
    if id.is_empty() || !proxy.firewalls.knows_api(&target.project, id) {
        return answer(StatusCode::NOT_FOUND);
    }
    let Some(key) = RATE_LIMIT_KEY.iter().find_map(|name| head.headers.get(*name)).and_then(|v| v.to_str().ok()).filter(|k| !k.is_empty() && k.len() <= 1024) else {
        return coded(StatusCode::BAD_REQUEST, "BAD_REQUEST", "A rate-limit question needs the client's key in x-forkrail-rate-limit-key.");
    };
    let region = proxy.registry.here();
    let request = firewall_request(&FirewallInput {
        method: head.method.as_str(),
        host: &log.host,
        uri: &head.uri,
        headers: &head.headers,
        peer,
        protocol: if log.proto == "h2" { "HTTP/2.0" } else { "HTTP/1.1" },
        scheme: if entry == Entry::Https { "https" } else { "http" },
        environment: &target.environment,
        region: &region,
        location: &log.location,
        target_path: None,
        rate_limit_api: Some(ApiCheck { id, key }),
        challenge_passed: false,
        ja4: log.ja4.as_deref(),
    });
    let Judged { decision, rule, .. } = proxy.firewalls.judge(&target.project, &request, true);
    if decision != Decision::Allow {
        log.firewall = Some(decision.name());
        log.firewall_rule = rule;
    }
    match decision {
        Decision::RateLimited { .. } => answer(StatusCode::TOO_MANY_REQUESTS),
        Decision::Deny { .. } => answer(StatusCode::FORBIDDEN),
        _ => answer(StatusCode::NO_CONTENT),
    }
}

/// The firewall's 403.
fn denied_by_firewall() -> Response<ResponseBody> {
    let mut response = coded(StatusCode::FORBIDDEN, "FORBIDDEN", "The firewall refused this request.");
    response.headers_mut().insert(MITIGATED_HEADER, HeaderValue::from_static("deny"));
    response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

/// The path the deployment serves a request from, for rules on `target_path`: its static file's,
/// after the routing rules; `None` when a function or anything else answers (the request's path).
async fn target_path(proxy: &Proxy, target: &Target, head: &UpstreamHead, host: &str) -> Option<String> {
    let statics = proxy.statics.as_ref()?;
    let manifest = match &target.static_files {
        Some(id) => statics.manifest(id).await,
        None => None,
    };
    let has_function = target.pool.is_some();
    let project_rules = target.project_routing.routing.as_deref();
    let engine = match target.rules.as_deref() {
        None if project_rules.is_none() && has_function => PLAIN_FUNCTION.clone(),
        None if project_rules.is_none() => PLAIN_STATIC.clone(),
        deployment => statics.program(deployment, project_rules, has_function).await.ok()?,
    };
    let files: &(dyn Files + Sync) = match manifest.as_deref() {
        Some(manifest) => manifest,
        None => &NoFiles,
    };
    let with_images = ImageFiles(files);
    let files: &(dyn Files + Sync) = if target.images.is_some() { &with_images } else { files };
    let request = rules::Request { method: head.method.as_str(), path: head.uri.path(), query: head.uri.query(), host, headers: &head.headers };
    match engine.route(&request, files).outcome {
        Outcome::Static { file } => Some(format!("/{}", file.trim_start_matches('/'))),
        _ => None,
    }
}

/// Port 80: every request goes to the same URL over HTTPS, with 308 so the method and body stay.
/// A request without a usable hostname (a bare IP address, say) gets 400, since the certificate
/// covers names only. The health path is answered here, on any host.
pub fn redirect<B>(req: &Request<B>) -> Response<ResponseBody> {
    // Global Accelerator's health checks: plain HTTP, the IP address as the host.
    if req.uri().path() == HEALTH_PATH && matches!(*req.method(), Method::GET | Method::HEAD) {
        return health();
    }
    let host = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .or_else(|| req.uri().authority().map(|a| a.to_string()))
        .map(|h| normalize_host(&h))
        .unwrap_or_default();
    let named = !host.is_empty()
        && host.parse::<std::net::IpAddr>().is_err()
        && host.split('.').all(|label| !label.is_empty() && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    let path_and_query = req.uri().path_and_query().map_or("/", |pq| pq.as_str());
    match HeaderValue::from_str(&format!("https://{host}{path_and_query}")) {
        Ok(location) if named => {
            let mut response = error(StatusCode::PERMANENT_REDIRECT, "");
            response.headers_mut().insert(LOCATION, location);
            response
        }
        _ => error(StatusCode::BAD_REQUEST, "use https:// and a hostname"),
    }
}

/// Deployment protection for a request to a protected host: `Ok` to go on, or the router's own
/// answer with its outcome: sent to the dashboard for access, refused, or redirected to keep a
/// cookie (a share link, a bypass secret). Also answers `/_forkrail/auth`, where the dashboard
/// sends a browser back with its token.
fn guard(gate: &Guard, head: &UpstreamHead, peer: SocketAddr, log: &mut AccessLog) -> Result<(), (Response<ResponseBody>, &'static str)> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let path = head.uri.path();
    if path == protection::AUTH_PATH {
        log.rule = Some("protection".to_owned());
        return Err(match gate.auth(&log.host, head.uri.query(), &head.headers, now) {
            Ok((cookie, to)) => {
                let mut response = redirect_response(StatusCode::FOUND, &to, log);
                response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response.headers_mut().append(SET_COOKIE, cookie);
                (response, "protection_granted")
            }
            Err(reason) => {
                info!(request = %log.request_id, host = %log.host, reason, "protection: a token was refused");
                (denied(None, "This access link has expired or isn't for this deployment."), "protected")
            }
        });
    }
    let ask = protection::Ask {
        host: &log.host,
        method: &head.method,
        path,
        query: head.uri.query(),
        headers: &head.headers,
        client: peer.ip().to_canonical(),
        now,
    };
    match gate.check(&ask) {
        Access::Granted => Ok(()),
        Access::Redirect { cookie, location } => {
            log.rule = Some("protection".to_owned());
            let mut response = redirect_response(StatusCode::TEMPORARY_REDIRECT, &location, log);
            response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response.headers_mut().append(SET_COOKIE, cookie);
            Err((response, "protection_granted"))
        }
        Access::NotFound => {
            log.rule = Some("protection".to_owned());
            let mut response = not_found_response(&head.headers, &log.request_id);
            response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Err((response, "protected"))
        }
        Access::Denied => {
            log.rule = Some("protection".to_owned());
            let browser = matches!(head.method, Method::GET | Method::HEAD)
                && head.headers.get(hyper::header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html"));
            // A browser begins a sign-in: its nonce cookie, and the dashboard's page with the nonce's hash.
            let begun = browser.then(|| gate.begin(&head.headers));
            let login = begun.as_ref().and_then(|(_, nh)| gate.login_url(&log.host, head.uri.path_and_query().map_or("/", |pq| pq.as_str()), nh));
            let mut response = denied(login.as_deref(), "Authentication is required to open this deployment.");
            if let (Some(_), Some((cookie, _))) = (&login, begun) {
                response.headers_mut().append(SET_COOKIE, cookie);
            }
            Err((response, "protected"))
        }
    }
}

/// The answer to a request protection keeps out: to the dashboard for access (a browser, when the
/// install has one), else 401 with a JSON error.
fn denied(login: Option<&str>, message: &str) -> Response<ResponseBody> {
    let mut response = match login.and_then(|l| HeaderValue::from_str(l).ok()) {
        Some(location) => {
            let mut response = error(StatusCode::FOUND, "Redirecting...\n");
            response.headers_mut().insert(LOCATION, location);
            response
        }
        None => {
            let body = serde_json::json!({"error": {"code": "DEPLOYMENT_PROTECTED", "message": message}}).to_string();
            let mut response = Response::new(Full::new(Bytes::from(body)).map_err(|never| match never {}).boxed());
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            response
        }
    };
    response.headers_mut().insert(ERROR_HEADER, HeaderValue::from_static("DEPLOYMENT_PROTECTED"));
    response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// What the routing rules change about the function's request and answer: the request's query
/// string, and the answer's status and headers.
#[derive(Default)]
struct Effects {
    /// The query string the request gets instead of the client's (encoded).
    query: Option<String>,
    status: Option<StatusCode>,
    headers: Vec<(HeaderName, HeaderValue)>,
    /// The routes' transforms: the request's are applied before it goes on, the answer's here.
    transforms: Vec<rules::Transform>,
    /// The function path the routes reached, which a deployment of several functions in one
    /// dispatches on: sent to it as `x-forkrail-function`.
    function: Option<String>,
    /// That function's own maxDuration, when it has one under the deployment's.
    max_duration: Option<Duration>,
    /// The prerender that function is (Vercel's ISR), and the captures of the route that sent the
    /// request there, for its `x-now-route-matches`.
    prerender: Option<Arc<rules::Prerender>>,
    captures: Option<Vec<(String, String)>>,
    /// How the answer is kept by the response cache, once it's decided (`cached::before`).
    cache: Option<cached::Fill>,
    /// Headers that go beside the answer's own of the same name (a middleware's `set-cookie`).
    added: Vec<(HeaderName, HeaderValue)>,
    /// A Next.js deployment's request (`nextjs.rs`): its config, for the cache and the headers.
    next: Option<Arc<nextjs::Ctx>>,
    /// A request another router sent to run here (`relay.rs`): the worker's answer goes back as
    /// it came, with what was measured, and that router does the rest. With `run_alone`, it gets
    /// a worker to itself.
    for_router: bool,
    run_alone: bool,
}

/// A copy for another request the router makes on the same routing (Image Optimization fetching
/// its source from a function): the response cache's fill stays with the request it was made for.
impl Clone for Effects {
    fn clone(&self) -> Self {
        Effects {
            query: self.query.clone(),
            status: self.status,
            headers: self.headers.clone(),
            transforms: self.transforms.clone(),
            function: self.function.clone(),
            max_duration: self.max_duration,
            prerender: self.prerender.clone(),
            captures: self.captures.clone(),
            cache: None,
            added: self.added.clone(),
            next: self.next.clone(),
            for_router: self.for_router,
            run_alone: self.run_alone,
        }
    }
}

impl Effects {
    /// The rules' status, and their headers where the answer hasn't set its own: on Vercel a
    /// header the function sets wins over the same one from `vercel.json`.
    fn apply(&self, status: &mut StatusCode, headers: &mut HeaderMap) {
        if let Some(s) = self.status {
            *status = s;
        }
        for (name, value) in &self.headers {
            if !headers.contains_key(name) {
                headers.insert(name.clone(), value.clone());
            }
        }
        for (name, value) in &self.added {
            headers.append(name.clone(), value.clone());
        }
        rules::transform::apply_headers(&self.transforms, TransformKind::ResponseHeaders, headers);
    }

    /// Change the request as the transforms say: its headers, query and path.
    fn transform_request(&mut self, head: &mut UpstreamHead) {
        if self.transforms.is_empty() {
            return;
        }
        rules::transform::apply_headers(&self.transforms, TransformKind::RequestHeaders, &mut head.headers);
        let raw = self.query.clone().or_else(|| head.uri.query().map(str::to_owned)).unwrap_or_default();
        if let Some(query) = rules::transform::apply_query(&self.transforms, &raw) {
            self.query = Some(query);
        }
        if let Some(path) = rules::transform::request_path(&self.transforms) {
            let query = self.query.clone().or_else(|| head.uri.query().map(str::to_owned));
            let pq = match query.as_deref() {
                Some(q) if !q.is_empty() => format!("{path}?{q}"),
                _ => path.to_owned(),
            };
            let mut parts = head.uri.clone().into_parts();
            if let Ok(pq) = pq.parse() {
                parts.path_and_query = Some(pq);
                if let Ok(uri) = Uri::from_parts(parts) {
                    head.uri = uri;
                    self.query = None;
                }
            }
        }
    }

    /// For a site elsewhere's answer, where Vercel lets the rules' headers win.
    fn apply_over(&self, status: &mut StatusCode, headers: &mut HeaderMap) {
        if let Some(s) = self.status {
            *status = s;
        }
        for (name, value) in &self.headers {
            headers.insert(name.clone(), value.clone());
        }
        for (name, value) in &self.added {
            headers.append(name.clone(), value.clone());
        }
        rules::transform::apply_headers(&self.transforms, TransformKind::ResponseHeaders, headers);
    }
}

/// How routing left a request.
enum Routed {
    /// Answered, and its log line written.
    Done(Response<ResponseBody>),
    /// For the function.
    Function(Effects),
    /// For a URL elsewhere.
    External { url: String, effects: Effects },
    /// A Routing Middleware runs first (`middleware.rs`).
    Middleware(Box<middleware::Pending>),
}

/// Run the deployment's routing rules (or the plain ones: its files, then its function) and
/// answer what they decide the router answers itself.
async fn route(proxy: &Arc<Proxy>, target: &Target, head: &UpstreamHead, guarded: bool, log: &mut AccessLog) -> Routed {
    let has_function = target.pool.is_some();
    // With FORKRAIL_STATIC=0 there are no files or rules to read: the function has everything.
    let Some(statics) = proxy.statics.as_ref() else {
        return if has_function {
            Routed::Function(Effects::default())
        } else {
            Routed::Done(log_now(log, coded(StatusCode::NOT_FOUND, "NOT_FOUND", "The page could not be found."), "not_found"))
        };
    };
    let manifest = match &target.static_files {
        Some(id) => statics.manifest(id).await,
        None => None,
    };
    let project_rules = target.project_routing.routing.as_deref();
    let engine = match target.rules.as_deref() {
        None if project_rules.is_none() && has_function => PLAIN_FUNCTION.clone(),
        None if project_rules.is_none() => PLAIN_STATIC.clone(),
        deployment => match statics.program(deployment, project_rules, has_function).await {
            Ok(engine) => engine,
            // Never the function without them: they may hold the middleware and the rules that keep
            // visitors off its routes. Tried again within seconds (`statics.rs`).
            Err(Unreadable::Deployment) => {
                let response = coded(StatusCode::BAD_GATEWAY, "ROUTER_RULES_UNAVAILABLE", "The deployment's routing rules couldn't be read.");
                return Routed::Done(log_now(log, response, "rules_unavailable"));
            }
        },
    };
    // Bulk redirects come before every other rule: the project's, then the deployment's.
    if !guarded {
        let tables = [(target.project_routing.redirects.as_deref(), "project bulk redirects"), (engine.bulk(), "bulk redirects")];
        for (id, name) in tables {
            let Some(id) = id else { continue };
            let Some(table) = statics.redirects(id).await else { continue };
            if let Some(found) = table.lookup(&log.host, head.uri.path()) {
                log.rule = Some(name.to_owned());
                let status = StatusCode::from_u16(found.status).unwrap_or(StatusCode::TEMPORARY_REDIRECT);
                let location = found.location(head.uri.query());
                return Routed::Done(log_now(log, redirect_response(status, &location, log), "redirect"));
            }
        }
    }
    let files: &(dyn Files + Sync) = match manifest.as_deref() {
        Some(manifest) => manifest,
        None => &NoFiles,
    };
    let with_images = ImageFiles(files);
    let routed_files: &(dyn Files + Sync) = if target.images.is_some() { &with_images } else { files };
    // Its own copy of the host: a `mitigate` answer writes the log.
    let host = log.host.clone();
    let request = rules::Request { method: head.method.as_str(), path: head.uri.path(), query: head.uri.query(), host: &host, headers: &head.headers };
    let mut step = engine.step(&request, routed_files);
    let decision = loop {
        match step {
            rules::Step::Decided(decision) => break decision,
            rules::Step::Middleware { function, paused, .. } => {
                return Routed::Middleware(Box::new(middleware::Pending { engine: engine.clone(), manifest, function, paused }));
            }
            rules::Step::Mitigate { action, route, paused } => match mitigated(proxy, head, log, action, engine.rule_name(route)) {
                Some(response) => return Routed::Done(response),
                None => step = engine.resume(&request, routed_files, paused, rules::Resume::Next),
            },
        }
    };
    settle(proxy, target, head, log, &engine, manifest.as_ref(), statics, decision).await
}

/// A route's `mitigate` (Vercel's), as the firewall answers: `deny` with its 403, `challenge`
/// with its challenge page unless the visitor has passed it. None when they have: routing goes on.
fn mitigated(proxy: &Proxy, head: &UpstreamHead, log: &mut AccessLog, action: rules::MitigateAction, rule: String) -> Option<Response<ResponseBody>> {
    let response = match action {
        rules::MitigateAction::Deny => {
            log.firewall = Some("deny");
            denied_by_firewall()
        }
        rules::MitigateAction::Challenge => {
            let agent = head.headers.get(hyper::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
            let ip: std::net::IpAddr = log.client.parse().unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
            let now = unix_now();
            if proxy.challenger.passed(&head.headers, ip, agent, &log.host, now) {
                return None;
            }
            log.firewall = Some("challenge");
            proxy.challenger.page(ip, agent, &log.host, now)
        }
    };
    log.firewall_rule = Some(rule);
    Some(log_now(log, response, "firewall"))
}

/// What a routing decision comes to: an answer the router makes itself, or the function's or a URL
/// elsewhere's to get.
#[allow(clippy::too_many_arguments)]
async fn settle(
    proxy: &Arc<Proxy>,
    target: &Target,
    head: &UpstreamHead,
    log: &mut AccessLog,
    engine: &Arc<Engine>,
    manifest: Option<&Arc<forkrail_common::statics::Manifest>>,
    statics: &Statics,
    decision: rules::Decision,
) -> Routed {
    let has_function = target.pool.is_some();
    let files: &(dyn Files + Sync) = match manifest {
        Some(manifest) => manifest.as_ref(),
        None => &NoFiles,
    };
    log.rule = decision.route.map(|i| engine.rule_name(i));
    let status = decision.status.and_then(|s| StatusCode::from_u16(s).ok());
    let headers: Vec<(HeaderName, HeaderValue)> = decision
        .headers
        .iter()
        .filter_map(|(k, v)| Some((HeaderName::from_bytes(k.as_bytes()).ok()?, HeaderValue::from_str(v).ok()?)))
        .collect();
    let transforms = decision.transforms;
    let function = decision.function;
    let captures = decision.captures;
    let looked_up = decision.looked_up;
    let max_duration = function.as_deref().and_then(|f| engine.max_duration(f)).map(Duration::from_secs);
    let respond = |log: &mut AccessLog, response: Response<ResponseBody>, outcome: &str| Routed::Done(log_now(log, respond_headers(response, &headers, &transforms), outcome));
    // A Next.js deployment (`nextjs.rs`): a hashed asset it hasn't got is a 404 without a worker,
    // and `/_next/image` under its base path is the image function's, both unless its middleware
    // runs on the request (then Next answers, after it).
    let next_site = match &target.next {
        Some(next) => proxy.next_sites.get(proxy.statics.as_deref(), &next.config).await,
        None => None,
    };
    if let (Some(site), Outcome::Function { .. }) = (&next_site, &decision.outcome) {
        if target.images.is_some() && !site.config.base_path.is_empty() && nextjs::image_is_routers(site, head, &log.host) {
            let (response, outcome) = image(proxy, target, head, engine, manifest, files, log).await;
            return respond(log, response, outcome);
        }
        if let Some(response) = nextjs::static_missing(site, head, &log.host) {
            log.function = None;
            return respond(log, response, "not_found");
        }
    }
    let image_routers = next_site.as_ref().is_none_or(|site| !nextjs::middleware_takes(site, head, &log.host));
    match decision.outcome {
        Outcome::Function { query } if has_function => {
            let prerender = function.as_deref().and_then(|f| engine.prerender(f)).cloned();
            Routed::Function(Effects { query, status, headers, transforms, function, max_duration, prerender, captures, ..Effects::default() })
        }
        Outcome::Function { .. } => respond(log, coded(StatusCode::NOT_FOUND, "NOT_FOUND", "The page could not be found."), "not_found"),
        Outcome::External { url } => Routed::External { url, effects: Effects { query: None, status, headers, transforms, ..Effects::default() } },
        Outcome::Redirect { location } => {
            let status = status.unwrap_or(StatusCode::PERMANENT_REDIRECT);
            let response = redirect_response(status, &location, log);
            let outcome = if response.status() == status { "redirect" } else { "redirect_failed" };
            respond(log, response, outcome)
        }
        // The optimizer's path is a file of the deployment's, as on Vercel: the routes before the
        // filesystem run first, and their headers go on its answers.
        Outcome::Static { file } if target.images.is_some() && forkrail_common::images::PATHS.contains(&file.as_str()) && image_routers => {
            let (response, outcome) = image(proxy, target, head, engine, manifest, files, log).await;
            respond(log, response, outcome)
        }
        // As on Vercel, OPTIONS is answered here when a file or the miss would answer.
        Outcome::Static { .. } | Outcome::NotFound if head.method == Method::OPTIONS => {
            respond(log, empty(StatusCode::NO_CONTENT), "static")
        }
        Outcome::Static { file } => {
            let found = manifest.and_then(|m| m.files.get(&file)).cloned();
            match found {
                // A file answers GET and HEAD, whether or not there's a function (Vercel's 405 has
                // no body). A page served with a status (a 404 page) answers any method.
                Some(_) if status.is_none() && !matches!(head.method, Method::GET | Method::HEAD) => {
                    let mut response = empty(StatusCode::METHOD_NOT_ALLOWED);
                    response.headers_mut().insert(hyper::header::ALLOW, HeaderValue::from_static("GET, HEAD, OPTIONS"));
                    respond(log, response, "static")
                }
                Some(found) => match statics.respond(&found, &head.method, &head.headers, status, crate::statics::Serve::File, |len| proxy.room(len)).await {
                    // Vercel's 304 for a file carries its caching headers alone, none of the routes'.
                    Some(served) if served.not_modified => {
                        log.function = None;
                        Routed::Done(log_now(log, served.response, "static"))
                    }
                    Some(served) => {
                        log.function = None;
                        log.encoding = served.encoding.map(str::to_owned);
                        log.bytes_out = served.bytes;
                        let mut response = respond_headers(served.response, &headers, &transforms);
                        // Vercel names every file it serves after the path the filesystem was
                        // asked for, unless the routes named it.
                        let name = looked_up.as_deref().unwrap_or(head.uri.path());
                        file_headers(response.headers_mut(), name, next_site.as_deref(), head, &log.host);
                        Routed::Done(log_now(log, response, "static"))
                    }
                    None if has_function => Routed::Function(Effects { headers, transforms: transforms.clone(), ..Effects::default() }),
                    None => respond(log, coded(StatusCode::BAD_GATEWAY, "ROUTER_STATIC_UNAVAILABLE", "The file couldn't be read."), "static_failed"),
                },
                None if has_function => Routed::Function(Effects::default()),
                None => respond(log, coded(StatusCode::NOT_FOUND, "NOT_FOUND", "The page could not be found."), "not_found"),
            }
        }
        Outcome::Error => {
            let status = status.unwrap_or(StatusCode::NOT_FOUND);
            let code = status.canonical_reason().unwrap_or("ERROR").to_ascii_uppercase().replace(' ', "_");
            respond(log, coded(status, &code, &format!("{}: {code}", status.as_u16())), "routed")
        }
        Outcome::NotFound => respond(log, coded(StatusCode::NOT_FOUND, "NOT_FOUND", "The page could not be found."), "not_found"),
    }
}

/// A deployment's files, with the optimizer's paths among them when it has image settings.
struct ImageFiles<'a>(&'a (dyn Files + Sync));

impl Files for ImageFiles<'_> {
    fn has(&self, path: &str) -> bool {
        forkrail_common::images::PATHS.contains(&path) || self.0.has(path)
    }
}

/// What an optimized image is made from, decided by routing its `url`.
enum ImageSource {
    /// One of the deployment's static files, with the `Cache-Control` it's served with; `fetched`
    /// when `url` named it by a path written another way, which Vercel treats as a fetch.
    Static { file: forkrail_common::statics::StaticFile, cache_control: String, fetched: bool },
    /// What the deployment's function answers for it.
    Function { pool: Arc<Pool>, head: UpstreamHead, effects: Effects, seed: LogSeed },
    /// A URL elsewhere: allowed by the settings, or where the deployment's own rewrite sends it.
    Url(String),
}

/// Everything needed to make an image again later, owned, so a stale one can be made in the
/// background.
struct ImagePlan {
    key: String,
    source: ImageSource,
    width: u32,
    quality: u32,
    format: Option<String>,
    svg: bool,
    minimum_cache_ttl: u64,
    /// The loop guard's mark of the request that asked (`loops.rs`).
    loop_mark: Option<String>,
    /// Whose image it is, for the optimizer's spend.
    owner: crate::spend::Owner,
}

/// What a source request's access log line starts from: the client's request's.
#[derive(Clone)]
struct LogSeed {
    entry: Entry,
    host: String,
    deployment: Option<String>,
    project: String,
    region: String,
    client: String,
    location: Location,
    agent: Option<String>,
}

/// `/_vercel/image` (see `images.rs`): checked as Vercel checks it, served from the cache, made by
/// the optimizer on a miss. Returns the answer and its access log outcome: `image_hit`,
/// `image_miss`, `image_stale`, `image_refused` (Vercel's 4xx), `image_failed`.
async fn image(
    proxy: &Arc<Proxy>,
    target: &Target,
    head: &UpstreamHead,
    engine: &Arc<Engine>,
    manifest: Option<&Arc<forkrail_common::statics::Manifest>>,
    files: &(dyn Files + Sync),
    log: &mut AccessLog,
) -> (Response<ResponseBody>, &'static str) {
    use crate::images::{self, Failure};
    log.function = None;
    let (Some(optimizer), Some(id)) = (proxy.images.clone(), target.images.as_deref()) else {
        return (coded(StatusCode::BAD_GATEWAY, "ROUTER_IMAGES_UNAVAILABLE", "Image optimization isn't set up on this router."), "image_failed");
    };
    let Some(config) = optimizer.config(id).await else {
        return (coded(StatusCode::BAD_GATEWAY, "ROUTER_IMAGES_UNAVAILABLE", "The deployment's image settings couldn't be read."), "image_failed");
    };
    match head.method {
        Method::GET | Method::HEAD => {}
        // Vercel answers OPTIONS as it does a source that isn't an image.
        Method::OPTIONS => return (images::failed(&Failure::NotAnImage, &config, "image", &[]), "image_refused"),
        _ => return (images::method_not_allowed(), "image_refused"),
    }
    let Ok(request) = config.request(head.uri.query()) else {
        return (images::invalid(), "image_refused");
    };
    let format = config.format(head.headers.get(hyper::header::ACCEPT).and_then(|v| v.to_str().ok())).map(str::to_owned);
    let stem = request.source.stem();
    let refuse = |failure: Failure, source_headers: &[(HeaderName, HeaderValue)]| {
        // A Next.js deployment's source that isn't there is Vercel's 400 for a request it can't
        // optimize (seen 2026-11-17: `/_next/image?url=/nope.bin` on a Pages Router app), not 404.
        if failure == Failure::BadUpstream && target.next.is_some() {
            let mut response = images::invalid();
            if let Ok(csp) = HeaderValue::from_str(&config.config.content_security_policy) {
                response.headers_mut().insert(HeaderName::from_static("content-security-policy"), csp);
            }
            return (response, "image_refused");
        }
        let outcome = if matches!(failure, Failure::Other { status, .. } if status >= 500) { "image_failed" } else { "image_refused" };
        (images::failed(&failure, &config, &stem, source_headers), outcome)
    };
    let project = target.project.as_str();
    let deployment = target.deployment.as_deref().unwrap_or_default();
    let fn_scope = format!("{project} {deployment}");
    let mut source_headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
    let (key, source) = match &request.source {
        forkrail_common::images::Source::Remote { url } => {
            (images::result_key(project, project, url, request.width, request.quality, format.as_deref()), ImageSource::Url(url.clone()))
        }
        local @ forkrail_common::images::Source::Local { path, query, exact } => {
            let path_and_query = local.path_and_query().unwrap_or_default();
            // Routed and fetched with none of the visitor's headers (`image_source_headers`).
            let anonymous = image_source_headers(&head.headers);
            let rq = rules::Request { method: "GET", path, query: query.as_deref(), host: &log.host, headers: &anonymous };
            let decision = engine.route(&rq, files);
            source_headers = decision
                .headers
                .iter()
                .filter_map(|(k, v)| Some((HeaderName::from_bytes(k.as_bytes()).ok()?, HeaderValue::from_str(v).ok()?)))
                .collect();
            match decision.outcome {
                Outcome::Static { file } => {
                    let Some(found) = manifest.and_then(|m| m.files.get(&file)).cloned() else {
                        return refuse(Failure::BadUpstream, &source_headers);
                    };
                    let kind = found.content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
                    if !kind.starts_with("image/") || (kind == "image/svg+xml" && !config.config.dangerously_allow_svg) {
                        return refuse(Failure::NotAnImage, &source_headers);
                    }
                    let cache_control = decision.headers.iter().find(|(k, _)| k == "cache-control").map_or_else(|| found.cache_control.clone(), |(_, v)| v.clone());
                    if !forkrail_common::images::OPTIMIZED.contains(&kind.as_str()) {
                        // Served as it is (an SVG, a GIF, an icon), as Vercel serves what it doesn't optimize.
                        let Some(statics) = proxy.statics.as_ref() else {
                            return refuse(Failure::BadUpstream, &source_headers);
                        };
                        let Some(bytes) = statics.file_bytes(&found).await else {
                            return refuse(Failure::other(StatusCode::BAD_GATEWAY, "ROUTER_STATIC_UNAVAILABLE", "The file couldn't be read."), &[]);
                        };
                        let own = forkrail_common::images::max_age(Some(&cache_control));
                        let (max_age, revalidate) = if *exact { (own, true) } else { (own.max(config.config.minimum_cache_ttl), false) };
                        let immutable = *exact && forkrail_common::images::immutable(Some(&cache_control));
                        let as_is = images::Optimized { bytes, content_type: kind, max_age, revalidate, immutable, expires: 0, created: images::now() };
                        let (response, sent) = images::respond(&as_is, "HIT", &config, &stem, &head.method, &head.headers);
                        log.bytes_out = sent;
                        return (response, "image_hit");
                    }
                    // Named by its content, for every deployment of the project; one fetched by a
                    // path written another way is answered differently, so it's named apart.
                    let key = match exact {
                        true => images::result_key(project, project, &found.sha256, request.width, request.quality, format.as_deref()),
                        false => images::result_key(project, &fn_scope, &path_and_query, request.width, request.quality, format.as_deref()),
                    };
                    (key, ImageSource::Static { file: found, cache_control, fetched: !*exact })
                }
                Outcome::Function { query } if target.pool.is_some() => {
                    let pool = target.pool.clone().expect("checked");
                    let function = decision.function;
                    let max_duration = function.as_deref().and_then(|f| engine.max_duration(f)).map(Duration::from_secs);
                    // A Next.js deployment's worker may answer with one of its static files.
                    let next = match &target.next {
                        Some(next) => Some(Arc::new(nextjs::ctx(proxy, next, target.static_files.clone()).await)),
                        None => None,
                    };
                    let effects = Effects { query, status: None, headers: source_headers.clone(), transforms: decision.transforms, function, max_duration, next, ..Default::default() };
                    let mut src = head.clone();
                    src.method = Method::GET;
                    src.uri = format!("http://{}{path_and_query}", head.host).parse().unwrap_or_else(|_| head.uri.clone());
                    src.headers = anonymous;
                    let seed = LogSeed {
                        entry: log.entry,
                        host: log.host.clone(),
                        deployment: log.deployment.clone(),
                        project: log.project.clone(),
                        region: log.region.clone(),
                        client: log.client.clone(),
                        location: log.location.clone(),
                        agent: log.agent.clone(),
                    };
                    let key = images::result_key(project, &fn_scope, &path_and_query, request.width, request.quality, format.as_deref());
                    (key, ImageSource::Function { pool, head: src, effects, seed })
                }
                // A rewrite elsewhere: fetched by the optimizer, named per deployment like a function's.
                Outcome::External { url } => {
                    (images::result_key(project, &fn_scope, &url, request.width, request.quality, format.as_deref()), ImageSource::Url(url))
                }
                Outcome::Redirect { .. } => return refuse(Failure::NotAnImage, &source_headers),
                _ => return refuse(Failure::BadUpstream, &source_headers),
            }
        }
    };
    let plan = Arc::new(ImagePlan {
        key: key.clone(),
        source,
        width: request.width,
        quality: request.quality,
        format,
        svg: config.config.dangerously_allow_svg,
        minimum_cache_ttl: config.config.minimum_cache_ttl,
        loop_mark: head.headers.get(crate::loops::HEADER).and_then(|v| v.to_str().ok()).map(str::to_owned),
        owner: crate::spend::Owner::new(&target.project, &target.deployment.clone().unwrap_or_default(), crate::spend::ledger().region()),
    });
    let (made, state) = match optimizer.cached(&key).await {
        Some(found) if found.fresh(images::now()) => (Ok(found), "HIT"),
        Some(stale) => {
            // Served as it is once more, and made again behind it, as Vercel serves STALE.
            let (proxy, optimizer, plan) = (proxy.clone(), optimizer.clone(), plan.clone());
            tokio::spawn(async move {
                let maker = optimizer.clone();
                let _ = optimizer.once(&plan.key, || make_image(proxy, maker, plan.clone())).await;
            });
            (Ok(stale), "STALE")
        }
        None => (optimizer.once(&key, || make_image(proxy.clone(), optimizer.clone(), plan.clone())).await, "MISS"),
    };
    match made {
        Ok(found) => {
            let (mut response, sent) = images::respond(&found, state, &config, &stem, &head.method, &head.headers);
            // A Next.js deployment's image from elsewhere, as Vercel's `/_next/image` answers it
            // (seen 2026-11-17 on saleor): `must-revalidate`, and no CORS header.
            if target.next.is_some() && matches!(request.source, forkrail_common::images::Source::Remote { .. }) {
                let headers = response.headers_mut();
                headers.remove("access-control-allow-origin");
                if let Some(cc) = headers.get(hyper::header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).filter(|c| !c.contains("must-revalidate"))
                    && let Ok(value) = HeaderValue::from_str(&format!("{cc}, must-revalidate"))
                {
                    headers.insert(hyper::header::CACHE_CONTROL, value);
                }
            }
            log.bytes_out = sent;
            (response, match state {
                "HIT" => "image_hit",
                "STALE" => "image_stale",
                _ => "image_miss",
            })
        }
        Err(failure) => refuse(failure, &source_headers),
    }
}

/// Have the optimizer make a planned image: from a static file by its key, from what the function
/// answers (fetched here, through a worker like any request, with its own access log line), or
/// from a URL the optimizer fetches.
async fn make_image(proxy: Arc<Proxy>, optimizer: Arc<crate::images::Images>, plan: Arc<ImagePlan>) -> Result<Arc<crate::images::Optimized>, crate::images::Failure> {
    use crate::images::{Failure, Job, JobSource, MAX_INLINE_SOURCE};
    let (source, content_type, cache_control, fetched) = match &plan.source {
        ImageSource::Static { file, cache_control, fetched } => {
            (JobSource::Key(file.key(forkrail_common::statics::Encoding::Identity)), Some(file.content_type.clone()), Some(cache_control.clone()), *fetched)
        }
        ImageSource::Url(url) => (JobSource::Url(url.clone()), None, None, true),
        ImageSource::Function { pool, head, effects, seed } => {
            let (status, headers, bytes) = fetch_source(&proxy, pool.clone(), head.clone(), effects.clone(), seed).await?;
            // A redirect isn't followed (Vercel answers its own 400 for one).
            if status.is_redirection() {
                return Err(Failure::NotAnImage);
            }
            if !status.is_success() {
                return Err(Failure::BadUpstream);
            }
            let kind = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
            if !kind.starts_with("image/") || (kind == "image/svg+xml" && !plan.svg) {
                return Err(Failure::NotAnImage);
            }
            let Some(bytes) = bytes else {
                return Err(Failure::other(
                    StatusCode::BAD_GATEWAY,
                    "ROUTER_IMAGE_SOURCE_TOO_LARGE",
                    &format!("A function's image over {} MB can't be optimized.", MAX_INLINE_SOURCE >> 20),
                ));
            };
            let cache_control = headers.get(hyper::header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).map(str::to_owned);
            (JobSource::Body(bytes), Some(kind), cache_control, true)
        }
    };
    optimizer
        .optimize(Job {
            source,
            content_type,
            cache_control,
            width: plan.width,
            quality: plan.quality,
            format: plan.format.clone(),
            svg: plan.svg,
            minimum_cache_ttl: plan.minimum_cache_ttl,
            immutable: !fetched,
            fetched,
            store: plan.key.clone(),
            loop_mark: match &plan.source {
                ImageSource::Url(url) if url.parse::<Uri>().ok().and_then(|u| u.host().map(crate::loops::is_own)).unwrap_or(false) => plan.loop_mark.clone(),
                _ => None,
            },
            owner: plan.owner.clone(),
        })
        .await
}

/// The headers a function gets when its answer is an image's source: the router's own that are the
/// same for every visitor (the host and scheme asked for), none of the visitor's. The optimized
/// image is kept for everyone, so nothing one visitor sent (cookies, `Authorization`, their address
/// or location) may shape it: Next.js's optimizer sends no headers since CVE-2025-57752, and
/// neither does Vercel's. The routes choose the source with these headers too.
fn image_source_headers(visitor: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    // The loop guard's mark is the router's, not the visitor's: a source that calls back for an
    // image of its own goes on down the same chain (`loops.rs`).
    for name in ["x-forwarded-host", "x-forwarded-proto", crate::loops::HEADER] {
        if let Some(value) = visitor.get(name) {
            out.insert(HeaderName::from_static(name), value.clone());
        }
    }
    out
}

/// A GET of the deployment's own path for an image, through a worker, as a request of its own:
/// its status, headers and body (`None` past `MAX_INLINE_SOURCE`).
async fn fetch_source(
    proxy: &Arc<Proxy>,
    pool: Arc<Pool>,
    mut head: UpstreamHead,
    mut effects: Effects,
    seed: &LogSeed,
) -> Result<(StatusCode, HeaderMap, Option<Bytes>), crate::images::Failure> {
    effects.transform_request(&mut head);
    head.headers.remove(forkrail_common::FUNCTION_HEADER);
    if let Some(value) = effects.function.as_deref().and_then(|f| HeaderValue::from_str(f).ok()) {
        head.headers.insert(forkrail_common::FUNCTION_HEADER, value);
    }
    if let Some(query) = &effects.query {
        head.uri = with_query(&head.uri, query);
    }
    let mut log = AccessLog::spent();
    log.written = false;
    log.request_id = forkrail_common::quick_id();
    head.headers.insert(REQUEST_ID, HeaderValue::from_str(&log.request_id).expect("hex is a valid header"));
    log.entry = seed.entry;
    log.host = seed.host.clone();
    log.deployment = seed.deployment.clone();
    log.project = seed.project.clone();
    log.region = seed.region.clone();
    log.client = seed.client.clone();
    log.location = seed.location.clone();
    log.agent = seed.agent.clone();
    log.proto = "internal";
    log.method = Method::GET;
    log.path = mask_path(head.uri.path());
    log.query = head.uri.query().filter(|q| !q.is_empty()).map(|q| truncate(&mask_query(q), MAX_LOGGED_QUERY));
    log.rule = Some("image source".to_owned());
    log.function = Some(FunctionLog::of(pool.config()));
    log.bytes_in = Some(0);
    pool.logs().begin(&log.request_id);
    log.app_logs = Some(pool.logs().clone());
    let (tx, rx) = oneshot::channel();
    let body = UpstreamBody::Buffered { bytes: Bytes::new(), _room: None };
    tokio::spawn(exchange(head, body, pool, Version::HTTP_11, proxy.clone(), log, effects, tx));
    let failed = || crate::images::Failure::other(StatusCode::BAD_GATEWAY, "ROUTER_IMAGE_SOURCE_FAILED", "The image's source couldn't be read.");
    let response = rx.await.map_err(|_| failed())?;
    let (parts, body) = response.into_parts();
    let limit = crate::images::MAX_INLINE_SOURCE;
    let declared = parts.headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > limit) {
        return Ok((parts.status, parts.headers, None));
    }
    // Read to the end whatever the status, so the source's own line is complete.
    let _room = proxy.room(declared.unwrap_or(limit)).await;
    let mut body = body;
    let mut read = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { return Err(failed()) };
        if let Ok(data) = frame.into_data() {
            if read.len() + data.len() > limit {
                return Ok((parts.status, parts.headers, None));
            }
            read.extend_from_slice(&data);
        }
    }
    Ok((parts.status, parts.headers, Some(read.freeze())))
}

/// A redirect to `location`, as Vercel answers one; a 500 if `location` can't be a header.
fn redirect_response(status: StatusCode, location: &str, log: &AccessLog) -> Response<ResponseBody> {
    let Ok(value) = HeaderValue::from_str(location) else {
        warn!(request = %log.request_id, location, "a redirect's location isn't a valid header");
        return coded(StatusCode::INTERNAL_SERVER_ERROR, "ROUTER_INVALID_REDIRECT", "The redirect's destination isn't valid.");
    };
    let mut response = error(status, "Redirecting...\n");
    response.headers_mut().insert(LOCATION, value);
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=0, must-revalidate"));
    // As Vercel does, for clients that predate 308.
    if status == StatusCode::PERMANENT_REDIRECT
        && let Ok(refresh) = HeaderValue::from_str(&format!("0;url={location}"))
    {
        response.headers_mut().insert(HeaderName::from_static("refresh"), refresh);
    }
    response
}

/// A response the router made, with the rules' headers set over its own.
fn respond_headers(mut response: Response<ResponseBody>, headers: &[(HeaderName, HeaderValue)], transforms: &[rules::Transform]) -> Response<ResponseBody> {
    for (name, value) in headers {
        response.headers_mut().insert(name.clone(), value.clone());
    }
    rules::transform::apply_headers(transforms, TransformKind::ResponseHeaders, response.headers_mut());
    response
}

/// Write a response the router made itself, keeping `log` for nobody after it (`finish_now` takes
/// the log by value; here it's borrowed, so a spent copy stays behind, marked written).
fn log_now(log: &mut AccessLog, response: Response<ResponseBody>, outcome: &str) -> Response<ResponseBody> {
    let taken = std::mem::replace(log, AccessLog::spent());
    taken.finish_now(response, outcome)
}

/// A URL with its query string replaced by `query` (encoded).
fn with_url_query(url: &str, query: &str) -> String {
    let (base, hash) = match url.find('#') {
        Some(at) => (&url[..at], &url[at..]),
        None => (url, ""),
    };
    let base = base.split('?').next().unwrap_or(base);
    if query.is_empty() { format!("{base}{hash}") } else { format!("{base}?{query}{hash}") }
}

/// `uri` with `query` (encoded) as its query string.
fn with_query(uri: &Uri, query: &str) -> Uri {
    let path_and_query = if query.is_empty() { uri.path().to_owned() } else { format!("{}?{query}", uri.path()) };
    let mut parts = uri.clone().into_parts();
    match path_and_query.parse() {
        Ok(pq) => {
            parts.path_and_query = Some(pq);
            Uri::from_parts(parts).unwrap_or_else(|_| uri.clone())
        }
        Err(_) => uri.clone(),
    }
}

fn external_client() -> Result<External, BoxError> {
    let mut http = HttpConnector::new_with_resolver(PublicResolver);
    http.enforce_http(false);
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_provider_and_platform_verifier(rustls::crypto::ring::default_provider())?
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Ok(Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build(connector))
}

/// A rewrite elsewhere reaches the public internet only, as on Vercel: never this box (the
/// tester's port, the instance metadata service whose credentials are the router's role), the
/// VPC, or any other private network. A name is resolved here and only its public addresses are
/// dialed, so it can't answer differently in between; an address written in the URL itself never
/// reaches a resolver and is checked in `external`.
#[derive(Clone, Copy, Debug)]
pub struct PublicResolver;

impl tower_service::Service<hyper_util::client::legacy::connect::dns::Name> for PublicResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = std::io::Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: hyper_util::client::legacy::connect::dns::Name) -> Self::Future {
        Box::pin(async move {
            let public: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0)).await?.filter(|a| is_public(a.ip())).collect();
            if public.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("{name} has no public address")));
            }
            Ok(public.into_iter())
        })
    }
}

/// Whether `ip` is on the public internet: not this machine, a private or shared network
/// (10/8, 172.16/12, 192.168/16, 100.64/10, fc00::/7), link-local (169.254/16 holds the instance
/// metadata service and the VPC's DNS; fe80::/10), multicast or reserved, nor one of those inside
/// an IPv6 address (mapped, NAT64, 6to4).
pub fn is_public(ip: std::net::IpAddr) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    let v4 = |v4: Ipv4Addr| {
        let [a, b, c, _] = v4.octets();
        !(a == 0
            || a == 10
            || a == 127
            || (a == 169 && b == 254)
            || (a == 172 && (16..=31).contains(&b))
            || (a == 192 && b == 168)
            || (a == 100 && (64..=127).contains(&b))
            || (a == 192 && b == 0 && c == 0)
            || (a == 198 && (b == 18 || b == 19))
            || a >= 224)
    };
    match ip {
        IpAddr::V4(ip) => v4(ip),
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return v4(mapped);
            }
            let s = ip.segments();
            let embedded = |hi: u16, lo: u16| v4(Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8));
            if s[0] == 0x64 && s[1] == 0xff9b {
                return embedded(s[6], s[7]);
            }
            if s[0] == 0x2002 {
                return embedded(s[1], s[2]);
            }
            !(s[..6].iter().all(|&x| x == 0)
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xff00) == 0xff00
                || (s[0] == 0x2001 && s[1] < 0x0200)
                || (s[0] == 0x2001 && s[1] == 0x0db8))
        }
    }
}

/// A rewrite to a URL elsewhere: the request goes there as the client sent it (with the router's
/// `x-forwarded-*` headers), and its answer comes back as it arrives.
async fn external(proxy: &Proxy, head: UpstreamHead, body: ProxyBody, url: &str, effects: Effects, client_version: Version, mut log: AccessLog) -> Response<ResponseBody> {
    log.function = None;
    let failed = |log: AccessLog, status: StatusCode, message: &str| {
        let mut response = coded(status, "ROUTER_EXTERNAL_TARGET_ERROR", message);
        for (name, value) in &effects.headers {
            response.headers_mut().insert(name.clone(), value.clone());
        }
        log.finish_now(response, "external_failed")
    };
    let (Some(client), Ok(uri)) = (proxy.external.as_ref(), url.parse::<Uri>()) else {
        return failed(log, StatusCode::BAD_GATEWAY, "The rewrite's destination couldn't be reached.");
    };
    // An address in the URL is dialed as written, without the resolver's check.
    let literal = uri.host().and_then(|h| h.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().ok());
    if literal.is_some_and(|ip| !is_public(ip)) {
        warn!(request = %log.request_id, url, "a rewrite to an address that isn't public was refused");
        return failed(log, StatusCode::BAD_GATEWAY, "The rewrite's destination isn't on the public internet.");
    }
    let own = uri.host().is_some_and(crate::loops::is_own);
    let mut request = Request::new(body);
    *request.method_mut() = head.method.clone();
    *request.uri_mut() = uri;
    *request.version_mut() = Version::HTTP_11;
    *request.headers_mut() = head.headers.clone();
    // A rewrite to one of this install's own hosts comes back here as the chain's next call
    // (`loops.rs`); a site elsewhere never sees the mark, nor the vouch for the visitor's address,
    // which would let it pass for that visitor here for minutes (`visitor.rs`).
    if !own {
        request.headers_mut().remove(crate::loops::HEADER);
        request.headers_mut().remove(crate::visitor::HEADER);
    }
    let started = Instant::now();
    let response = match tokio::time::timeout(EXTERNAL_TIMEOUT, client.request(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            warn!(request = %log.request_id, url, error = %e, "a rewrite's destination failed");
            return failed(log, StatusCode::BAD_GATEWAY, "The rewrite's destination couldn't be reached.");
        }
        Err(_) => return failed(log, StatusCode::GATEWAY_TIMEOUT, "The rewrite's destination took too long to answer."),
    };
    let (mut parts, upstream) = response.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    // An origin elsewhere may tag its answers too; Vercel reads the tags and doesn't pass them on.
    parts.headers.remove(crate::cache::CACHE_TAG);
    parts.version = client_version;
    effects.apply_over(&mut parts.status, &mut parts.headers);
    log.tag(&mut parts.headers);
    log.status = parts.status.as_u16();
    log.encoding = header_text(&parts.headers, CONTENT_ENCODING);
    log.ttfb = Some(started.elapsed());
    log.expected_out = parts.headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok());
    Response::from_parts(parts, ExternalBody { upstream, log: Some(log) }.boxed())
}

/// The answer from a URL elsewhere, passed on as it arrives. Writes the access log line when it
/// ends.
struct ExternalBody {
    upstream: Incoming,
    log: Option<AccessLog>,
}

impl Body for ExternalBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        match ready!(Pin::new(&mut this.upstream).poll_frame(cx)) {
            Some(Ok(frame)) => {
                if let (Some(log), Some(data)) = (this.log.as_mut(), frame.data_ref()) {
                    log.bytes_out += data.len() as u64;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Some(Err(e)) => {
                if let Some(log) = this.log.take() {
                    log.write("external_failed");
                }
                Poll::Ready(Some(Err(e.into())))
            }
            None => {
                if let Some(log) = this.log.take() {
                    log.write("external");
                }
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.upstream.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.upstream.size_hint()
    }
}

impl Drop for ExternalBody {
    fn drop(&mut self) {
        if let Some(log) = self.log.take() {
            let complete = log.expected_out == Some(log.bytes_out);
            log.write(if complete { "external" } else { "client_gone" });
        }
    }
}

/// Remove one of the worker's numeric headers and parse it.
fn take_kb(headers: &mut HeaderMap, name: &str) -> Option<u64> {
    headers.remove(name)?.to_str().ok()?.parse().ok()
}

/// A paused project's answer (spend protection, `budgets.rs`), never cached.
fn paused_response(request: &HeaderMap, request_id: &str) -> Response<ResponseBody> {
    let mut response = error_page(request, request_id, StatusCode::SERVICE_UNAVAILABLE, "DEPLOYMENT_PAUSED", "This deployment is paused.", Icon::Paused);
    response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// No deployment answers for the host.
fn not_found_response(request: &HeaderMap, request_id: &str) -> Response<ResponseBody> {
    error_page(request, request_id, StatusCode::NOT_FOUND, "DEPLOYMENT_NOT_FOUND", "The deployment could not be found.", Icon::Unknown)
}

/// The mark above an error page's words.
enum Icon {
    Paused,
    Unknown,
}

/// An error as `coded` says it, or for a browser as a small page of its own (`error.html`:
/// centered, no name or logo, light and dark, the request's id to quote).
fn error_page(request: &HeaderMap, request_id: &str, status: StatusCode, code: &str, message: &str, icon: Icon) -> Response<ResponseBody> {
    const PAGE: &str = include_str!("error.html");
    let mut response = coded(status, code, message);
    let browser = request.get(hyper::header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html"));
    if browser {
        // Only the router's own words go in, and its hex id: nothing to escape.
        let id: String = request_id.chars().filter(char::is_ascii_alphanumeric).collect();
        let icon = match icon {
            Icon::Paused => "<i></i><i></i>",
            Icon::Unknown => "<b>?</b>",
        };
        let page = PAGE
            .replace("@STATUS@", status.as_str())
            .replace("@TITLE@", message.trim_end_matches('.'))
            .replace("@MESSAGE@", message)
            .replace("@CODE@", code)
            .replace("@ICON@", icon)
            .replace("@ID@", &id);
        *response.body_mut() = Full::new(Bytes::from(page)).map_err(|never| match never {}).boxed();
        response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    }
    response
}

fn service_unavailable(retry_after: Duration) -> Response<ResponseBody> {
    let mut response = error(StatusCode::SERVICE_UNAVAILABLE, "no worker available");
    response.headers_mut().insert(RETRY_AFTER, HeaderValue::from(retry_after.as_secs().max(1)));
    response
}

fn timed_out() -> Response<ResponseBody> {
    coded(StatusCode::GATEWAY_TIMEOUT, "FUNCTION_INVOCATION_TIMEOUT", "The function took longer than its maxDuration.")
}

/// A response with no body, as Vercel's 204 to OPTIONS and 405 are (revalidated, as its others).
fn empty(status: StatusCode) -> Response<ResponseBody> {
    let mut response = error(status, "");
    response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=0, must-revalidate"));
    response
}

fn error(status: StatusCode, message: &'static str) -> Response<ResponseBody> {
    let body = Full::new(Bytes::from_static(message.as_bytes())).map_err(|never| match never {}).boxed();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
}

/// An error with a code in the body and in `x-forkrail-error`, as Vercel names its errors.
fn coded(status: StatusCode, code: &str, message: &str) -> Response<ResponseBody> {
    let body = Full::new(Bytes::from(format!("{message}\n\n{code}\n"))).map_err(|never| match never {}).boxed();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    if let Ok(code) = HeaderValue::from_str(code) {
        response.headers_mut().insert(ERROR_HEADER, code);
    }
    response
}

/// Where the tester's port takes a schedule's run, as the queue would give it (tests).
const SCHEDULE_RUN_PATH: &str = "/_forkrail/schedule-run";

/// `POST /_forkrail/schedule-run` with a message's JSON: 202 and the run, or why it didn't start.
async fn schedule_run(req: Request<Incoming>, proxy: &Proxy) -> Response<ResponseBody> {
    use crate::schedules::Skipped;
    let json = |status: StatusCode, value: serde_json::Value| {
        let mut response = Response::new(Full::new(Bytes::from(value.to_string())).map_err(|never| match never {}).boxed());
        *response.status_mut() = status;
        response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        response
    };
    let Some(runs) = &proxy.schedules else { return json(StatusCode::NOT_FOUND, serde_json::json!({"error": "schedules are off"})) };
    if req.method() != Method::POST {
        return json(StatusCode::METHOD_NOT_ALLOWED, serde_json::json!({"error": "POST a message"}));
    }
    let body = match http_body_util::Limited::new(req.into_body(), 64 << 10).collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => return json(StatusCode::BAD_REQUEST, serde_json::json!({"error": "unreadable body"})),
    };
    match runs.take_message(&String::from_utf8_lossy(&body), None) {
        Ok(run) => json(StatusCode::ACCEPTED, serde_json::json!({"key": run.key, "run": run.run, "delay_ms": run.delay_ms, "manual": run.manual})),
        Err(Skipped::Unreadable(why)) => json(StatusCode::BAD_REQUEST, serde_json::json!({"error": why})),
        Err(Skipped::Repeat) => json(StatusCode::CONFLICT, serde_json::json!({"skipped": "repeat"})),
        Err(Skipped::Expired) => json(StatusCode::OK, serde_json::json!({"skipped": "expired"})),
        Err(Skipped::Gone) => json(StatusCode::NOT_FOUND, serde_json::json!({"skipped": "gone"})),
    }
}

fn health() -> Response<ResponseBody> {
    let mut response = error(StatusCode::OK, "ok\n");
    response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// A query string for the access log: values of parameters whose names look secret (`api_key`,
/// `token`, `X-Amz-Signature`) become `REDACTED`. Everything else is kept as sent, still encoded.
/// A URL as it's logged: without a user or password, without its fragment, and with
/// secret-looking query parameters masked.
pub(crate) fn mask_url(url: &str) -> String {
    let url = url.split_once('#').map_or(url, |(u, _)| u);
    let url = match url.find("://") {
        Some(i) => {
            let rest = &url[i + 3..];
            let authority = &rest[..rest.find(['/', '?']).unwrap_or(rest.len())];
            match authority.rfind('@') {
                Some(at) => format!("{}{}", &url[..i + 3], &rest[at + 1..]),
                None => url.to_owned(),
            }
        }
        None => url.to_owned(),
    };
    let url = match url.find(DEPLOY_HOOKS_PATH) {
        Some(i) => format!("{}{}", &url[..i], mask_path(&url[i..])),
        None => url,
    };
    match url.split_once('?') {
        Some((path, query)) => format!("{path}?{}", mask_query(query)),
        None => url,
    }
}

/// Where the dashboard answers deploy hooks: `/api/deploy-hooks/<project>/<secret>`.
const DEPLOY_HOOKS_PATH: &str = "/api/deploy-hooks/";

/// A path for the access log: a deploy hook's secret, which is in the path of its URL, becomes
/// `REDACTED`, as a secret-looking query parameter's value does. Anything after it goes too.
pub(crate) fn mask_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(DEPLOY_HOOKS_PATH)
        && let Some((project, secret)) = rest.split_once('/')
        && !secret.is_empty()
        && !secret.starts_with('?')
    {
        let query = secret.find('?').map_or("", |i| &secret[i..]);
        return format!("{DEPLOY_HOOKS_PATH}{project}/REDACTED{query}");
    }
    path.to_owned()
}

pub(crate) fn mask_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _)) if secret_param(name) => format!("{name}=REDACTED"),
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn secret_param(name: &str) -> bool {
    let name: String = name.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    SECRET_PARAMS.iter().any(|s| name.ends_with(s)) || SECRET_NAMES.contains(&name.as_str())
}

/// At most `max` bytes, cut at a character boundary.
/// A header's value for the access log, cut to `MAX_LOGGED_ENCODING`.
fn header_text(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers.get(name).map(|v| truncate(&String::from_utf8_lossy(v.as_bytes()), MAX_LOGGED_ENCODING))
}

fn truncate(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// What a page's `@vercel/analytics` (or `@forkrail/analytics`) and `@vercel/speed-insights`
/// scripts ask the platform for (`insights.rs`). Answered for every deployment, since a project
/// that ships them had them on at Vercel, where an unanswered path would reach the app as a 404 on
/// every page and a console error. With Web Analytics on for the project, the script counts and a
/// beacon writes its own line in place of the request's; otherwise the script is an empty one and
/// beacons get 204, as nothing of Speed Insights is collected either.
#[allow(clippy::too_many_arguments)]
async fn insights(
    proxy: &Proxy,
    target: &Target,
    head: &UpstreamHead,
    body: Incoming,
    peer: SocketAddr,
    ask: crate::insights::Ask,
    tracked_for: Option<std::net::IpAddr>,
    mut log: AccessLog,
) -> Response<ResponseBody> {
    use crate::insights::{self as analytics, Agent, Ask, Beacon, Place};
    let on = target.project_routing.analytics_on();
    match ask {
        Ask::Script | Ask::SpeedScript => {
            let counts = ask == Ask::Script && on;
            let gzip = counts && head.headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("gzip"));
            let body = match (counts, gzip) {
                (true, true) => analytics::script_gzipped(),
                (true, false) => Bytes::from_static(analytics::SCRIPT.as_bytes()),
                (false, _) => Bytes::from_static(analytics::EMPTY_SCRIPT.as_bytes()),
            };
            let mut response = Response::new(Full::new(body).map_err(|never| match never {}).boxed());
            response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/javascript; charset=utf-8"));
            if counts {
                response.headers_mut().insert(hyper::header::VARY, HeaderValue::from_static("accept-encoding"));
            }
            if gzip {
                response.headers_mut().insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
                log.encoding = Some("gzip".to_owned());
            }
            // The empty one briefly, so switching analytics on reaches pages within a minute.
            let cache = if counts { "public, max-age=3600" } else { "public, max-age=60" };
            response.headers_mut().insert(hyper::header::CACHE_CONTROL, HeaderValue::from_static(cache));
            return log.finish_now(response, "insights");
        }
        Ask::NotFound => return log.finish_now(empty(StatusCode::NOT_FOUND), "insights"),
        Ask::Other => return log.finish_now(empty(StatusCode::NO_CONTENT), "insights"),
        Ask::View | Ask::Event => {}
    }
    if !on {
        return log.finish_now(empty(StatusCode::NO_CONTENT), "insights_off");
    }
    let dropped = |log: AccessLog| log.finish_now(empty(StatusCode::NO_CONTENT), "insights_dropped");
    // Another site's page can make a visitor's browser post here; only the project's own count.
    if let Some(origin) = head.headers.get(hyper::header::ORIGIN).and_then(|v| v.to_str().ok())
        && !same_project(proxy, &target.project, origin.split_once("://").map_or(origin, |(_, host)| host))
    {
        return dropped(log);
    }
    let agent = head.headers.get(hyper::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    if analytics::is_bot(&agent) || (tracked_for.is_none() && analytics::is_library(&agent)) {
        return dropped(log);
    }
    let bytes = match http_body_util::Limited::new(body, analytics::MAX_BODY).collect().await {
        Ok(b) => b.to_bytes(),
        Err(_) => return dropped(log),
    };
    log.bytes_in = Some(bytes.len() as u64);
    let Some(mut beacon) = Beacon::read(ask, &bytes) else { return dropped(log) };
    // The page's host when it's the project's (a function's `track` posts to its deployment's URL
    // for a page on a domain), else the host it was sent to.
    if !same_project(proxy, &target.project, &beacon.host) {
        beacon.host = log.host.clone();
    }
    let ip = tracked_for.unwrap_or_else(|| peer.ip());
    let country = match tracked_for {
        Some(ip) => proxy.geo.lookup(ip.to_canonical()).country,
        None => log.location.country.clone(),
    };
    let visitor = proxy.salts.visitor(crate::insights::day_of(unix_now()), &target.project, ip, &agent);
    let environment = match target.environment.as_str() {
        "" => proxy.registry.project_of(&log.host).map(|(_, e)| e).unwrap_or_default(),
        e => e.to_owned(),
    };
    let response = empty(StatusCode::NO_CONTENT);
    analytics::write(
        &beacon,
        visitor,
        Agent::of(&agent),
        &Place {
            project: &target.project,
            deployment: target.deployment.as_deref(),
            environment: &environment,
            request_host: &log.host,
            entry: log.entry.name(),
            region: &target.region,
            country: country.as_deref(),
            status: response.status().as_u16(),
            bytes_in: bytes.len() as u64,
            bytes_out: 0,
            total_ms: log.started.elapsed().as_millis() as u64,
        },
    );
    log.finish_unlogged(response)
}

/// Whether `host` (a host, maybe with a port) is one of `project`'s.
fn same_project(proxy: &Proxy, project: &str, host: &str) -> bool {
    let host = normalize_host(host.split('/').next().unwrap_or_default());
    proxy.registry.project_of(&host).is_some_and(|(p, _)| p == project)
}

/// The visitor a function's server `track` is for: the address in `x-vercel-ip` when the request
/// says it's from `track` (`x-va-server`). Taken as it is, as a page's own beacon is: analytics
/// counts what it's sent.
fn server_tracked_for<B>(req: &Request<B>) -> Option<std::net::IpAddr> {
    use crate::insights::{SERVER_ADDRESS_HEADER, SERVER_HEADER};
    req.headers().get(SERVER_HEADER)?;
    let forwarded = req.headers().get(SERVER_ADDRESS_HEADER)?.to_str().ok()?;
    forwarded.split(',').next()?.trim().parse().ok()
}

/// The one `x-middleware-*` a Next.js deployment's visitors may send (`nextjs::request_headers`).
pub(crate) fn nextjs_prefetch() -> HeaderName {
    HeaderName::from_static(forkrail_common::next::CLIENT_MAY_SEND[0])
}

/// A request header only the platform may set, so a client's is taken off before anything reads
/// it: Vercel tells apps to trust `x-vercel-forwarded-for` for the visitor's address (a client's
/// own would let it choose its address for the app's rate limits and allow lists), and
/// middleware's protocol headers steer routing (Next.js's CVE-2025-29927 was a client's
/// `x-middleware-subrequest`). Kept: what the router itself reads from visitors further on (the
/// protection bypass and its cookie request, the rate-limit API's key).
fn platform_only(name: &str) -> bool {
    const READ_BY_THE_ROUTER: [&str; 3] = ["x-vercel-protection-bypass", "x-vercel-set-bypass-cookie", "x-vercel-rate-limit-key"];
    (name.starts_with("x-vercel-") && !READ_BY_THE_ROUTER.contains(&name))
        || name.starts_with("x-middleware-")
        // A schedule's run: its cron expression and the CloudEvent fields only Vercel's scheduler
        // sets (other CloudEvents keep the standard `ce-` ones).
        || name.starts_with("ce-vss")
        || matches!(name, "x-now-route-matches" | "x-matched-path" | "x-forkrail-cron-schedule")
}

/// Whether the request's host (`:authority`, else `Host`) names a port other than 443.
fn foreign_port<B>(req: &Request<B>) -> bool {
    let host = match req.uri().authority() {
        Some(authority) => authority.as_str().rsplit('@').next().unwrap_or_default().to_owned(),
        None => req.headers().get(HOST).and_then(|h| h.to_str().ok()).unwrap_or_default().trim().to_owned(),
    };
    let after_name = match host.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or("", |(_, rest)| rest),
        None => host.rsplit_once(':').map_or("", |(name, _)| &host[name.len()..]),
    };
    after_name.strip_prefix(':').is_some_and(|port| port != "443")
}

/// `path` with its percent-encoded unreserved characters (letters, digits, `-._~`) decoded, if it
/// has any; the rest stays encoded as sent.
fn unreserved_decoded(path: &str) -> Option<String> {
    if !path.contains('%') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = path.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
            && (b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        {
            out.push(b as char);
            changed = true;
            i += 3;
            continue;
        }
        // Whole characters at a time: the path is ASCII as `http` takes it, but stays a str.
        let ch = path[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    changed.then_some(out)
}

/// Whether a path has a `.` or `..` segment, written plainly or with `%2e` for a dot.
fn dot_segment(path: &str) -> bool {
    path.split('/').any(|segment| {
        let segment = segment.to_ascii_lowercase();
        let segment = if segment.contains('%') { segment.replace("%2e", ".") } else { segment };
        segment == "." || segment == ".."
    })
}

/// A client's header that an app reads as one the router sets: CGI, WSGI and Django's META make
/// `_` and `-` the same (`x_forwarded_for` and `x-forwarded-for` are both `HTTP_X_FORWARDED_FOR`,
/// joined), so a client's `x_real_ip` would stand beside the router's own. Other underscored
/// names pass, as apps' own may use them.
fn aliases_platform(name: &str) -> bool {
    if !name.contains('_') {
        return false;
    }
    let dashed = name.replace('_', "-");
    platform_only(&dashed) || dashed.starts_with("x-forwarded-") || dashed.starts_with("x-forkrail-") || matches!(dashed.as_str(), "x-real-ip" | "forwarded" | "x-deployment-id")
}

/// Vercel types a function's answer that has a length but no type as plain text (seen 2026-11-16:
/// a 200 with `content-length` and no `content-type` arrives as `text/plain; charset=utf-8`; a
/// streamed one, with no length, and an empty one stay untyped; the status doesn't matter, so
/// SvelteKit's 403 for a cross-site form post is typed too).
fn default_content_type(status: StatusCode, headers: &mut HeaderMap) {
    let sized = headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok()).is_some_and(|n| n > 0);
    if !headers.contains_key(CONTENT_TYPE) && sized && !matches!(status.as_u16(), 204 | 304) {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    }
}

/// What every file the router serves gets on top of its own headers and the routes': a text type
/// the rules gave it a charset, as Vercel serves it; `content-disposition` named after `name` (the
/// path the filesystem was asked for) unless the routes named it; and a Next.js deployment's
/// config `headers()` for the request's own path, as Vercel serves its files (a `cache-control`
/// the app sets for one, say).
fn file_headers(headers: &mut HeaderMap, name: &str, next_site: Option<&forkrail_common::next::Site>, head: &UpstreamHead, host: &str) {
    if let Some(kind) = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok())
        && kind.starts_with("text/")
        && !kind.contains("charset")
        && let Ok(value) = HeaderValue::from_str(&format!("{kind}; charset=utf-8"))
    {
        headers.insert(CONTENT_TYPE, value);
    }
    headers.entry(hyper::header::CONTENT_DISPOSITION).or_insert_with(|| inline_disposition(name));
    if let Some(site) = next_site {
        let req = rules::Request { method: head.method.as_str(), path: head.uri.path(), query: head.uri.query(), host, headers: &head.headers };
        for (name, value) in site.config_headers(&req).unwrap_or_default() {
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
                headers.insert(name, value);
            }
        }
    }
}

/// Vercel's `content-disposition` on a file it serves (seen on every deployment): `inline`, named
/// after the last segment of the path its filesystem was asked for, after the routes' rewrites
/// (`Decision::looked_up`): `/docs` serves `docs/index.html` as "docs", `/photo.png/` is
/// "photo.png", a rewrite of `/static` to `/static/index.html` "index.html", a `cleanUrls` site's
/// 404 page "404"; `/` has no name. A name that can't sit in quotes as it is goes without one.
fn inline_disposition(path: &str) -> HeaderValue {
    let last = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if last.is_empty() || !last.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\') {
        return HeaderValue::from_static("inline");
    }
    HeaderValue::from_str(&format!("inline; filename=\"{last}\"")).unwrap_or_else(|_| HeaderValue::from_static("inline"))
}

#[cfg(test)]
mod paused_page_tests {
    use super::*;

    #[tokio::test]
    async fn a_browser_gets_the_page_anything_else_the_text() {
        let mut browser = HeaderMap::new();
        browser.insert(hyper::header::ACCEPT, HeaderValue::from_static("text/html,application/xhtml+xml,*/*;q=0.8"));
        let page = paused_response(&browser, "ab12cd34");
        assert_eq!(page.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(page.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
        assert_eq!(page.headers()[hyper::header::CACHE_CONTROL], "no-store");
        assert_eq!(page.headers()[ERROR_HEADER], "DEPLOYMENT_PAUSED");
        let html = String::from_utf8(page.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
        assert!(html.contains("This deployment is paused.") && html.contains("DEPLOYMENT_PAUSED · ab12cd34"));
        assert!(html.len() < 2500, "{} bytes: small", html.len());
        let missing = not_found_response(&browser, "ab12cd34");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let html = String::from_utf8(missing.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
        assert!(html.contains("<title>404: The deployment could not be found</title>") && html.contains("DEPLOYMENT_NOT_FOUND · ab12cd34") && html.contains("<b>?</b>"));
        assert!(!html.as_bytes().windows(2).any(|w| w[0] == b'@' && w[1].is_ascii_uppercase()), "every blank filled");
        let text = paused_response(&HeaderMap::new(), "ab12cd34");
        assert_eq!(text.headers()[CONTENT_TYPE], "text/plain; charset=utf-8");
        let body = String::from_utf8(text.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
        assert_eq!(body, "This deployment is paused.\n\nDEPLOYMENT_PAUSED\n");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_backslash_reaches_the_router_as_written() {
        // hyper and `http` take it, so the router has to refuse it itself (`handle`).
        let uri: Uri = "/\\other.example/admin".parse().unwrap();
        assert!(uri.path().contains('\\'));
    }

    #[test]
    fn dot_segments_are_refused_however_written() {
        // hyper and `http` keep them as sent (`curl --path-as-is`); an app resolving them would
        // serve another path than the one the firewall and the routes judged.
        let uri: Uri = "/login/../admin".parse().unwrap();
        assert_eq!(uri.path(), "/login/../admin");
        for dotted in ["/login/../admin", "/./admin", "/%2e/admin", "/a/%2E%2e/admin", "/a/.%2e", "/a/..", "/."] {
            assert!(dot_segment(dotted), "{dotted}");
        }
        for plain in ["/", "/a.b/c", "/.well-known/x", "/a/...", "/a/..b", "/%2e%2e%2e", "/a/%2ex", "/v1.2/..x"] {
            assert!(!dot_segment(plain), "{plain}");
        }
        // On 443, only 443 (or no port) in the host.
        let with_host = |host: &str| Request::builder().uri("/").header(HOST, host).body(()).unwrap();
        for ours in ["shop.example.com", "shop.example.com:443", "[2001:db8::1]", "[2001:db8::1]:443", "SHOP.example.com."] {
            assert!(!foreign_port(&with_host(ours)), "{ours}");
        }
        for other in ["shop.example.com:1337", "shop.example.com:", "[2001:db8::1]:8443", "shop.example.com:80"] {
            assert!(foreign_port(&with_host(other)), "{other}");
        }
        assert!(foreign_port(&Request::builder().uri("https://shop.example.com:8443/").body(()).unwrap()));
        // Encoded unreserved characters are read as themselves; the others stay as sent.
        assert_eq!(unreserved_decoded("/api/%61dmin").as_deref(), Some("/api/admin"));
        assert_eq!(unreserved_decoded("/%2Egit/x%7E%2d").as_deref(), Some("/.git/x~-"));
        assert_eq!(unreserved_decoded("/a%2Fb/%20c%"), None);
        assert_eq!(unreserved_decoded("/plain"), None);
        assert_eq!(unreserved_decoded("/a%2fb/%41%zz").as_deref(), Some("/a%2fb/A%zz"));
        // A schedule's own headers, forged by a visitor, are taken off like Vercel's.
        assert!(platform_only("x-forkrail-cron-schedule") && platform_only("ce-vssscheduleid") && !platform_only("ce-type"));
        assert!(aliases_platform("x_forkrail_cron_schedule"));
    }

    #[test]
    fn underscored_names_of_the_routers_headers_are_taken_off() {
        for name in ["x_forwarded_for", "x_real_ip", "x_vercel_forwarded_for", "x_vercel_ip_country", "x_forkrail_request_id", "x_forkrail_loop", "x-forwarded_host", "x_middleware_subrequest"] {
            assert!(super::aliases_platform(name), "{name}");
        }
        for name in ["x-forwarded-for", "api_key", "x_custom_thing", "x_vercel_protection_bypass"] {
            assert!(!super::aliases_platform(name), "{name}");
        }
    }

    #[test]
    fn rewrites_elsewhere_reach_public_addresses_only() {
        let public = |ip: &str| super::is_public(ip.parse().unwrap());
        for ip in ["1.1.1.1", "52.94.236.248", "2606:4700:4700::1111", "::ffff:8.8.8.8", "64:ff9b::808:808", "2002:808:808::1"] {
            assert!(public(ip), "{ip} is public");
        }
        for ip in [
            "169.254.169.254", "169.254.169.253", "127.0.0.1", "0.0.0.0", "10.0.1.5", "172.31.255.1", "192.168.1.1", "100.100.1.1",
            "192.0.0.192", "198.18.0.1", "224.0.0.1", "255.255.255.255", "::", "::1", "fd00:ec2::254", "fe80::1", "ff02::1",
            "::ffff:169.254.169.254", "::ffff:127.0.0.1", "::127.0.0.1", "64:ff9b::a9fe:a9fe", "2002:a9fe:a9fe::1", "2001:db8::1",
        ] {
            assert!(!public(ip), "{ip} isn't public");
        }
    }

    #[tokio::test]
    async fn resolver_keeps_only_public_addresses() {
        use tower_service::Service as _;
        let name = |n: &str| n.parse::<hyper_util::client::legacy::connect::dns::Name>().unwrap();
        let refused = super::PublicResolver.call(name("localhost")).await.unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::PermissionDenied);
        // An address written as a name (as getaddrinfo reads `2852039166`) is checked like any other.
        assert!(super::PublicResolver.call(name("2852039166")).await.is_err(), "169.254.169.254 written as a number");
    }

    #[test]
    fn header_fields_as_big_as_vercel_takes() {
        let ask = |fields: &[(&str, String)], target: &str| {
            let mut req = Request::builder().uri(target);
            for (k, v) in fields {
                req = req.header(*k, v.as_str());
            }
            oversized_head(&req.body(()).unwrap()).map(|r| r.status().as_u16())
        };
        let host = ("host", "shop.example".to_owned());
        // One field: 32 KiB with its name, `: ` and CRLF (Vercel: "X-Big: " + 32,759 bytes passed, 32,760 refused).
        assert_eq!(ask(&[host.clone(), ("x-big", "a".repeat(32_759))], "/"), None);
        assert_eq!(ask(&[host.clone(), ("x-big", "a".repeat(32_760))], "/"), Some(494));
        // All of them, with the request line.
        let fields = |n: usize| (0..n / 30_000).map(|i| (["x-a", "x-b", "x-c", "x-d", "x-e"][i], "a".repeat(30_000))).collect::<Vec<_>>();
        let mut many = fields(120_000);
        many.push(host.clone());
        assert_eq!(ask(&many, "/"), None);
        many.push(("x-rest", "a".repeat(7_500)));
        assert_eq!(ask(&many, "/"), Some(431));
        // A long URL counts (on Vercel a 2 KB query made a request 1 KB under refused).
        many.pop();
        many.push(("x-rest", "a".repeat(5_000)));
        assert_eq!(ask(&many, "/"), None);
        assert_eq!(ask(&many, &format!("/?q={}", "b".repeat(3_000))), Some(431));
    }

    #[test]
    fn the_forwarded_port_is_the_one_the_visitor_asked_on() {
        assert_eq!(forwarded_port("shop.example", "https"), "443");
        assert_eq!(forwarded_port("shop.example", "http"), "80");
        assert_eq!(forwarded_port("shop.example:8443", "https"), "8443");
        assert_eq!(forwarded_port("127.0.0.1:18080", "http"), "18080");
        assert_eq!(forwarded_port("[::1]:8080", "http"), "8080");
        // Not a port: the scheme's.
        assert_eq!(forwarded_port("[::1]", "https"), "443");
        assert_eq!(forwarded_port("shop.example:", "https"), "443");
        assert_eq!(forwarded_port("shop.example:99999999", "http"), "80");
    }

    #[test]
    fn an_image_source_gets_none_of_the_visitors_headers() {
        let mut visitor = HeaderMap::new();
        for (k, v) in [
            ("cookie", "sid=alice"),
            ("authorization", "Bearer alice"),
            ("x-forwarded-for", "203.0.113.9"),
            ("x-real-ip", "203.0.113.9"),
            ("x-vercel-forwarded-for", "203.0.113.9"),
            ("x-vercel-ip-country", "BD"),
            ("user-agent", "alice's browser"),
            ("accept-language", "bn"),
            ("x-forwarded-host", "shop.example"),
            ("x-forwarded-proto", "https"),
            ("x-forkrail-loop", "ab12.1.cd"),
        ] {
            visitor.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        let sent = image_source_headers(&visitor);
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert_eq!(&sent["x-forkrail-loop"], &HeaderValue::from_static("ab12.1.cd"), "the router's own mark goes on");
        assert_eq!((&sent["x-forwarded-host"], &sent["x-forwarded-proto"]), (&HeaderValue::from_static("shop.example"), &HeaderValue::from_static("https")));
    }

    #[test]
    fn platform_headers_come_from_the_router_alone() {
        for spoofed in ["x-vercel-forwarded-for", "x-vercel-id", "x-vercel-ip-country", "x-vercel-deployment-url", "x-middleware-subrequest", "x-middleware-rewrite", "x-now-route-matches", "x-matched-path"] {
            assert!(platform_only(spoofed), "{spoofed}");
        }
        for kept in ["x-vercel-protection-bypass", "x-vercel-set-bypass-cookie", "x-vercel-rate-limit-key", "x-forwarded-proto", "x-custom", "authorization"] {
            assert!(!platform_only(kept), "{kept}");
        }
    }

    #[test]
    fn untyped_answers_with_a_length_are_plain_text_as_on_vercel() {
        let typed = |status: u16, headers: &[(&str, &str)]| {
            let mut map = HeaderMap::new();
            for (k, v) in headers {
                map.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
            }
            default_content_type(StatusCode::from_u16(status).unwrap(), &mut map);
            map.get(CONTENT_TYPE).map(|v| v.to_str().unwrap().to_owned())
        };
        assert_eq!(typed(200, &[("content-length", "20")]).as_deref(), Some("text/plain; charset=utf-8"));
        assert_eq!(typed(403, &[("content-length", "46")]).as_deref(), Some("text/plain; charset=utf-8"));
        assert_eq!(typed(200, &[]), None, "streamed: no length, no type added");
        assert_eq!(typed(200, &[("content-length", "2"), ("content-type", "application/json")]).as_deref(), Some("application/json"));
        assert_eq!(typed(204, &[("content-length", "0")]), None);
        assert_eq!(typed(200, &[("content-length", "0")]), None, "an empty answer stays untyped");
    }

    /// The three limits measured on Vercel (`PAYLOAD_LIMIT`), to the byte: curl's own headers
    /// (over HTTP/2 the host is the URI's), then with a 1,000-byte header, then a 1,000-character query.
    #[test]
    fn function_payload_limit_is_vercels() {
        let host = "forkrail-check-fw-sveltekit.vercel.app";
        let curl = |length: u64, extra: &[(&'static str, String)]| {
            let mut h = HeaderMap::new();
            h.insert("user-agent", HeaderValue::from_static("curl/8.7.1"));
            h.insert("accept", HeaderValue::from_static("*/*"));
            h.insert("content-type", HeaderValue::from_static("application/octet-stream"));
            h.insert("content-length", HeaderValue::from(length));
            for (k, v) in extra {
                h.insert(*k, HeaderValue::from_str(v).unwrap());
            }
            h
        };
        let largest = |path: &str, extra: &[(&'static str, String)], runs: u64| {
            // The length header is part of the request, so it's the one each body is sent with.
            let refused = runs + 1;
            assert!(runs <= max_body(payload_head(path, host, &curl(runs, extra))), "{path}: {runs} runs on Vercel");
            assert!(refused > max_body(payload_head(path, host, &curl(refused, extra))), "{path}: {refused} is refused on Vercel");
        };
        largest("/api/env", &[], 4_493_999);
        largest("/api/env", &[("x-pad", "a".repeat(1000))], 4_493_241);
        largest(&format!("/api/env?q={}", "a".repeat(1000)), &[], 4_493_247);
        // A host header (HTTP/1.1) counts once.
        let mut with_host = curl(4_493_999, &[]);
        with_host.insert(HOST, HeaderValue::from_static("forkrail-check-fw-sveltekit.vercel.app"));
        assert_eq!(payload_head("/api/env", host, &with_host), payload_head("/api/env", host, &curl(4_493_999, &[])));
        // Vercel's documented 4.5 MB is about where a plain request ends up.
        assert!((4_490_000..4_500_000).contains(&max_body(payload_head("/", "a.example.com", &HeaderMap::new()))));
    }

    #[test]
    fn files_are_named_inline_as_vercel_names_them() {
        let name = |p: &str| inline_disposition(p).to_str().unwrap().to_owned();
        assert_eq!(name("/"), "inline");
        assert_eq!(name("/docs"), "inline; filename=\"docs\"");
        assert_eq!(name("/assets/site.css"), "inline; filename=\"site.css\"");
        assert_eq!(name("/photo.png/"), "inline; filename=\"photo.png\"");
        assert_eq!(name("/a\"b"), "inline");
    }

    use super::*;
    use http_body_util::Empty;
    use hyper_util::rt::{TokioExecutor, TokioIo};

    /// A tunnel to a fake worker whose every request runs `service`.
    async fn tunnel<F, Fut>(service: F) -> (hyper::client::conn::http2::SendRequest<Empty<Bytes>>, tokio::task::JoinHandle<()>)
    where
        F: Fn(Request<Incoming>) -> Fut + Send + Clone + 'static,
        Fut: Future<Output = Result<Response<Full<Bytes>>, std::io::Error>> + Send + 'static,
    {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(server_io), hyper::service::service_fn(service))
                .await;
        });
        let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(client_io))
            .await
            .unwrap();
        tokio::spawn(conn);
        (sender, server)
    }

    fn get() -> Request<Empty<Bytes>> {
        Request::get("http://worker/").body(Empty::new()).unwrap()
    }

    #[tokio::test]
    async fn a_reset_stream_is_not_a_dead_tunnel() {
        // What a runtime does when its service fails one request: hyper resets that stream.
        let (mut sender, _server) = tunnel(|_| async { Err(std::io::Error::other("connection to the app failed")) }).await;
        let e = sender.send_request(get()).await.unwrap_err();
        assert_eq!(stream_reset(&e), Some(h2::Reason::INTERNAL_ERROR));
        assert!(!sender.is_closed());
        assert!(error_chain(&e).contains("stream error received"), "{}", error_chain(&e));
    }

    #[tokio::test]
    async fn a_dropped_tunnel_is_not_a_reset_stream() {
        // A worker killed with a request in flight: the connection goes, with no reset frame.
        let (mut sender, server) = tunnel(|_| std::future::pending()).await;
        let request = tokio::spawn(async move { sender.send_request(get()).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.abort();
        let e = request.await.unwrap().unwrap_err();
        assert_eq!(stream_reset(&e), None, "{}", error_chain(&e));
    }

    /// A worker whose app answers every request alike: `status`, `headers` and `body`.
    async fn answering(status: u16, headers: &'static [(&'static str, &'static str)], body: &'static [u8]) -> hyper::client::conn::http2::SendRequest<ProxyBody> {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |_| async move {
                let mut response = Response::new(Full::new(Bytes::from_static(body)));
                *response.status_mut() = StatusCode::from_u16(status).unwrap();
                for (name, value) in headers {
                    response.headers_mut().append(*name, HeaderValue::from_static(value));
                }
                Ok::<_, Infallible>(response)
            });
            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new()).serve_connection(TokioIo::new(server_io), service).await;
        });
        let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new()).handshake(TokioIo::new(client_io)).await.unwrap();
        tokio::spawn(conn);
        sender
    }

    /// A Next.js deployment's static files in a folder laid out as the store is: `/another.txt`
    /// and `/café.txt`, under manifest `FILES`.
    const FILES: &str = "5a1e5a1e5a1e5a1e5a1e5a1e5a1e5a1e5a1e5a1e5a1e5a1e5a1e5a1e5a1e5a1e";
    fn next_files() -> std::path::PathBuf {
        use forkrail_common::statics::{Encoding, Manifest, StaticFile, manifest_key};
        let dir = std::env::temp_dir().join(format!("forkrail-worker-file-{}-{}", std::process::id(), forkrail_common::quick_id()));
        let mut manifest = Manifest::default();
        for (path, body, sha) in [("/another.txt", "some text\n", "ab".repeat(32)), ("/café.txt", "crème\n", "cd".repeat(32))] {
            let file = StaticFile { sha256: sha, size: body.len() as u64, content_type: "text/plain".into(), cache_control: "public, max-age=0, must-revalidate".into(), br: None, gz: None };
            let at = dir.join(file.key(Encoding::Identity));
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, body).unwrap();
            manifest.files.insert(path.to_owned(), file);
        }
        let at = dir.join(manifest_key(FILES));
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, serde_json::to_vec(&manifest).unwrap()).unwrap();
        dir
    }

    /// A GET through `exchange`, as `handle` sends it, to a worker whose app answers as `answering`
    /// does; for a Next.js deployment with `next_files`' files when `next`. The router's answer,
    /// and how many workers are still leased once it's read.
    async fn through_worker(
        files: &std::path::Path,
        next: bool,
        path: &str,
        request: &[(&'static str, &'static str)],
        worker: (u16, &'static [(&'static str, &'static str)], &'static [u8]),
    ) -> (u16, HeaderMap, Bytes, usize) {
        let (spawner, spawned) = crate::pool::tests::hanging_spawner();
        let pool = Pool::new(crate::pool::tests::single(), spawner);
        tokio::spawn(pool.clone().reap());
        let backend = crate::registry::Backend::Local(std::path::PathBuf::new());
        let registry = Registry::new(None, backend, crate::pool::tests::single(), Some(pool.clone()), None);
        let statics = Arc::new(Statics::new(crate::statics::Source::Dir(files.to_path_buf()), 1 << 20));
        let proxy = Arc::new(Proxy::new(registry, 1 << 20, 64 << 20, Geo::open(std::path::PathBuf::from("/nonexistent")), Some(statics)));
        let mut headers = HeaderMap::new();
        for (name, value) in request {
            headers.insert(*name, HeaderValue::from_static(value));
        }
        let head = UpstreamHead { host: "app.test".into(), method: Method::GET, uri: format!("http://app.test{path}").parse().unwrap(), headers, payload: 0 };
        let mut log = AccessLog::spent();
        log.written = false;
        log.request_id = forkrail_common::quick_id();
        let ctx = nextjs::Ctx { shape: forkrail_common::routes::NextShape::Handlers, site: None, handler: false, file: false, files: Some(FILES.into()) };
        let effects = Effects { next: next.then(|| Arc::new(ctx)), ..Effects::default() };
        let (tx, rx) = oneshot::channel();
        let body = UpstreamBody::Buffered { bytes: Bytes::new(), _room: None };
        tokio::spawn(exchange(head, body, pool.clone(), Version::HTTP_11, proxy, log, effects, tx));
        let preface = loop {
            if let Some(p) = crate::pool::tests::spawned(&pool).pop() {
                break p;
            }
            crate::pool::tests::settle().await;
        };
        pool.claim(&preface.worker_id, &preface.token).unwrap();
        let (status, answer, body) = worker;
        pool.register(&preface, answering(status, answer, body).await, None);
        let (parts, body) = rx.await.unwrap().into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        crate::pool::tests::settle().await;
        let leased = crate::pool::tests::counts(&pool).2;
        std::fs::remove_dir_all(spawned).unwrap();
        (parts.status.as_u16(), parts.headers, body, leased)
    }

    #[tokio::test]
    async fn a_next_workers_word_for_a_static_file_is_answered_with_the_file() {
        let files = next_files();
        // What the H server answers for `/docs/another.txt` under a rewrite of `/docs/:path*` to
        // `/:path*`, with a cookie its middleware set on the way.
        const NAMES: &[(&str, &str)] =
            &[("x-forkrail-static", "/another.txt"), ("set-cookie", "seen=1"), ("set-cookie", "theme=dark"), ("x-from-routing", "yes"), ("content-type", "application/octet-stream")];
        let (status, h, body, leased) = through_worker(&files, true, "/docs/another.txt", &[], (200, NAMES, b"")).await;
        assert_eq!((status, &body[..], leased), (200, &b"some text\n"[..], 0), "the file, and the worker let go");
        let text = |name: &str| h.get_all(name).iter().map(|v| v.to_str().unwrap().to_owned()).collect::<Vec<_>>();
        // The file's own headers, as a request for `/another.txt` gets them.
        assert_eq!(text("content-type"), ["text/plain; charset=utf-8"]);
        assert_eq!(text("cache-control"), ["public, max-age=0, must-revalidate"]);
        assert_eq!(text("content-disposition"), ["inline; filename=\"another.txt\""]);
        assert_eq!(text("x-vercel-cache"), ["MISS"]);
        assert_eq!(text("access-control-allow-origin"), ["*"]);
        // What routing set on the way, where the file has none of its own; never the word itself.
        assert_eq!(text("set-cookie"), ["seen=1", "theme=dark"]);
        assert_eq!(text("x-from-routing"), ["yes"]);
        assert!(!h.contains_key("x-forkrail-static"));

        // A byte range and a conditional request, as for the file's own path.
        let (status, _, body, _) = through_worker(&files, true, "/docs/another.txt", &[("range", "bytes=0-3")], (200, NAMES, b"")).await;
        assert_eq!((status, &body[..]), (206, &b"some"[..]));
        let etag = format!("\"{}\"", "ab".repeat(16));
        let etag: &'static str = Box::leak(etag.into_boxed_str());
        let (status, h, _, _) = through_worker(&files, true, "/docs/another.txt", &[("if-none-match", etag)], (200, NAMES, b"")).await;
        assert_eq!((status, h.contains_key("set-cookie")), (304, false));

        // Named percent-encoded, as the header carries any name.
        let (status, _, body, _) = through_worker(&files, true, "/fr/café.txt", &[], (200, &[("x-forkrail-static", "/caf%C3%A9.txt")], b"")).await;
        assert_eq!((status, &body[..]), (200, "crème\n".as_bytes()));

        // A file the deployment hasn't got: a 404, never the worker again.
        let (status, h, _, leased) = through_worker(&files, true, "/docs/nope.txt", &[], (200, &[("x-forkrail-static", "/nope.txt")], b"")).await;
        assert_eq!((status, h.get("x-forkrail-error").and_then(|v| v.to_str().ok()), leased), (404, Some("NOT_FOUND"), 0));
        std::fs::remove_dir_all(files).unwrap();
    }

    #[tokio::test]
    async fn the_word_is_only_a_next_workers_and_never_reaches_a_visitor() {
        let files = next_files();
        // Another deployment's app may send the header: its answer goes out as it is, without it.
        let (status, h, body, _) = through_worker(&files, false, "/x", &[], (200, &[("x-forkrail-static", "/another.txt"), ("x-own", "1")], b"app's own")).await;
        assert_eq!((status, &body[..]), (200, &b"app's own"[..]));
        assert!(!h.contains_key("x-forkrail-static") && h.contains_key("x-own"));
        std::fs::remove_dir_all(files).unwrap();
    }

    /// A fake worker's app: it says when it has a request, waits to be released, then answers.
    #[derive(Default)]
    struct App {
        requests: AtomicU64,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
        /// It answered, or its handler was dropped before it could.
        answered: std::sync::atomic::AtomicBool,
        cancelled: std::sync::atomic::AtomicBool,
    }

    /// A worker's tunnel, with `app` behind it.
    async fn worker(app: Arc<App>) -> hyper::client::conn::http2::SendRequest<ProxyBody> {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |_| {
                let app = app.clone();
                async move {
                    struct Running(Arc<App>);
                    impl Drop for Running {
                        fn drop(&mut self) {
                            if !self.0.answered.load(Ordering::SeqCst) {
                                self.0.cancelled.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                    let running = Running(app.clone());
                    app.requests.fetch_add(1, Ordering::SeqCst);
                    app.started.notify_one();
                    app.release.notified().await;
                    app.answered.store(true, Ordering::SeqCst);
                    drop(running);
                    let mut response = Response::new(Full::new(Bytes::from_static(b"answer")));
                    response.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from(6));
                    Ok::<_, Infallible>(response)
                }
            });
            let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(server_io), service)
                .await;
        });
        let (sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(client_io))
            .await
            .unwrap();
        tokio::spawn(conn);
        sender
    }

    /// A router with one pool, of one request per worker, retired as soon as it's free.
    fn router() -> (Arc<Proxy>, Arc<Pool>, std::path::PathBuf) {
        let (spawner, dir) = crate::pool::tests::hanging_spawner();
        let pool = Pool::new(crate::pool::tests::single(), spawner);
        tokio::spawn(pool.clone().reap());
        let backend = crate::registry::Backend::Local(std::path::PathBuf::new());
        let registry = Registry::new(None, backend, crate::pool::tests::single(), Some(pool.clone()), None);
        (Arc::new(Proxy::new(registry, 1 << 20, 64 << 20, Geo::open(std::path::PathBuf::from("/nonexistent")), None)), pool, dir)
    }

    /// A client's HTTP/1.1 request through `handle`, as the tester's port serves it. Returns the
    /// request, the client's connection and the router's side of it.
    async fn client_request(
        proxy: Arc<Proxy>,
    ) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let server = tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| handle(req, proxy.clone(), peer, Entry::Tester));
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(server_io), service).await;
        });
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(client_io)).await.unwrap();
        let conn = tokio::spawn(async move {
            let _ = conn.await;
        });
        let request = tokio::spawn(async move {
            let _ = sender.send_request(Request::get("/slow").body(Empty::<Bytes>::new()).unwrap()).await;
        });
        (request, conn, server)
    }

    /// The pool's one starting worker, connected to `app`.
    async fn connect(pool: &Pool, app: Arc<App>) {
        let preface = loop {
            if let Some(p) = crate::pool::tests::spawned(pool).pop() {
                break p;
            }
            crate::pool::tests::settle().await;
        };
        pool.claim(&preface.worker_id, &preface.token).unwrap();
        pool.register(&preface, worker(app).await, None);
    }

    /// The client hangs up, and the router's side of the connection ends with it.
    async fn hang_up(request: tokio::task::JoinHandle<()>, conn: tokio::task::JoinHandle<()>, server: tokio::task::JoinHandle<()>) {
        request.abort();
        conn.abort();
        tokio::time::timeout(Duration::from_secs(1), server).await.expect("hyper closes the connection").unwrap();
        crate::pool::tests::settle().await;
    }

    #[tokio::test]
    async fn a_request_its_client_left_holds_its_worker_until_the_app_answers() {
        let (proxy, pool, dir) = router();
        let (request, conn, server) = client_request(proxy).await;
        let app = Arc::new(App::default());
        connect(&pool, app.clone()).await;
        app.started.notified().await;

        // The client leaves while the app works on its request. The app isn't cancelled, and the
        // worker stays leased, so it isn't retired (and its environment frozen) under the app.
        hang_up(request, conn, server).await;
        assert!(!app.cancelled.load(Ordering::SeqCst), "the app's request was cancelled");
        assert_eq!(crate::pool::tests::counts(&pool).2, 1, "the worker was released");

        // Once the app has answered, the worker is free, and retires.
        app.release.notify_one();
        crate::pool::tests::settle().await;
        assert!(app.answered.load(Ordering::SeqCst));
        assert_eq!(crate::pool::tests::counts(&pool).2, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_request_its_client_left_while_it_waited_for_a_worker_never_reaches_one() {
        let (proxy, pool, dir) = router();
        let (request, conn, server) = client_request(proxy).await;
        while crate::pool::tests::counts(&pool).3 == 0 {
            crate::pool::tests::settle().await;
        }
        hang_up(request, conn, server).await;
        assert_eq!(crate::pool::tests::counts(&pool).3, 0, "still waiting for a worker");

        // The worker started for it connects after the client has gone: it gets nothing.
        let app = Arc::new(App::default());
        connect(&pool, app.clone()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(app.requests.load(Ordering::SeqCst), 0);
        assert_eq!(crate::pool::tests::counts(&pool).2, 0, "the worker retires");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A west router with no version of `deployment` in its region and an east router that runs
    /// `dpl`, linked as in production (`relay.rs`). Each test names its own regions, as the links
    /// are the process's.
    async fn two_routers(west_region: &str, east_region: &str, deployment: &str) -> (Arc<Proxy>, Arc<Pool>, Arc<Proxy>, std::path::PathBuf) {
        two_routers_running(west_region, east_region, deployment, None).await
    }

    /// The same, with the east router answering other routers' requests by `runs` instead of
    /// running them.
    async fn two_routers_running(
        west_region: &str,
        east_region: &str,
        deployment: &str,
        runs: Option<crate::cache::peers::Runs>,
    ) -> (Arc<Proxy>, Arc<Pool>, Arc<Proxy>, std::path::PathBuf) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (spawner, dir) = crate::pool::tests::hanging_spawner();
        let geo = || Geo::open(std::path::PathBuf::from("/nonexistent"));
        let east_registry = Registry::new(None, crate::registry::Backend::Local(dir.join("hang.sh")), crate::pool::tests::single(), None, None);
        let mut routes = forkrail_common::routes::Routes::default();
        routes.deployments.insert("dpl".into(), serde_json::from_value(serde_json::json!({"project": "api", "function": "local", "qualifier": "1"})).unwrap());
        east_registry.apply(routes);
        let east = Arc::new(Proxy::new(east_registry, 1 << 20, 64 << 20, geo(), None));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peers: std::collections::BTreeMap<String, String> =
            [(east_region.to_owned(), listener.local_addr().unwrap().to_string()), (west_region.to_owned(), "127.0.0.1:9".to_owned())].into();
        let key = [9u8; 32];
        let east_link = Arc::new(crate::cache::Link::new(east_region.into(), east_region.into(), peers.clone(), &key).unwrap());
        let east_cache = Arc::new(crate::cache::ResponseCache::new(1 << 20, 1 << 20, None, Some(east_link.clone()), true));
        let renders: crate::cache::peers::Renders = Arc::new(|_| Box::pin(async { crate::cache::peers::Answer::Unknown }));
        tokio::spawn(east_link.serve(listener, east_cache, renders, runs.unwrap_or_else(|| peer_runs(east.clone()))));

        let west_pool = Pool::new(crate::pool::PoolConfig { name: deployment.into(), relay: Some(crate::relay::slot(east_region)), ..crate::pool::tests::single() }, spawner);
        tokio::spawn(west_pool.clone().reap());
        let west_registry = Registry::new(None, crate::registry::Backend::Local(std::path::PathBuf::new()), crate::pool::tests::single(), Some(west_pool.clone()), None);
        let west = Arc::new(Proxy::new(west_registry, 1 << 20, 64 << 20, geo(), None));
        let routers = crate::routers::Routers::merge(west_region, None, &peers, &[]);
        let (_, routers) = tokio::sync::watch::channel(Arc::new(routers));
        crate::relay::start(Arc::new(crate::cache::Link::new(west_region.into(), east_region.into(), peers, &key).unwrap()), routers);
        for _ in 0..400 {
            if west_pool.relayed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(west_pool.relayed(), "the link is up");
        (west, west_pool, east, dir)
    }

    /// A client's request through `proxy`'s tester port, and what it got.
    async fn send(proxy: Arc<Proxy>, request: Request<Full<Bytes>>) -> tokio::task::JoinHandle<(StatusCode, HeaderMap, Bytes)> {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| handle(req, proxy.clone(), peer, Entry::Tester));
            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(server_io), service).await;
        });
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(client_io)).await.unwrap();
        tokio::spawn(async move {
            let _ = conn.await;
        });
        tokio::spawn(async move {
            let (parts, body) = sender.send_request(request).await.unwrap().into_parts();
            (parts.status, parts.headers, body.collect().await.unwrap().to_bytes())
        })
    }

    #[tokio::test]
    async fn a_request_for_a_deployment_run_in_another_region_runs_on_that_router() {
        let (west, west_pool, east, dir) = two_routers("xa-west-1", "xa-east-1", "dpl").await;
        let request = Request::post("/search").header(HOST, "api.test").body(Full::new(Bytes::from_static(b"q=1"))).unwrap();
        let answer = send(west, request).await;
        // The east router starts a worker for it, beside it; the west router starts none.
        let east_pool = east.registry.deployment_target("api.test", "dpl").unwrap().pool.unwrap();
        let app = Arc::new(App::default());
        connect(&east_pool, app.clone()).await;
        app.started.notified().await;
        app.release.notify_one();
        let (status, headers, body) = answer.await.unwrap();
        assert_eq!((status, body.as_ref()), (StatusCode::OK, &b"answer"[..]));
        assert!(headers.contains_key(REQUEST_ID));
        assert!(headers.keys().all(|k| !k.as_str().starts_with("x-forkrail-run") && k != "x-forkrail-ran" && k != crate::cache::peers::AUTH_HEADER), "the link's own headers stay between the routers: {headers:?}");
        assert_eq!(app.requests.load(Ordering::SeqCst), 1);
        assert_eq!(crate::pool::tests::counts(&west_pool), (0, 0, 0, 0), "no worker started from the west");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_deployment_the_other_router_doesnt_run_starts_its_worker_here() {
        let (west, west_pool, _east, dir) = two_routers("xb-west-1", "xb-east-1", "gone").await;
        let request = Request::post("/").header(HOST, "api.test").body(Full::new(Bytes::from_static(b"once"))).unwrap();
        let answer = send(west, request).await;
        // The east router hasn't got `gone`, so it refuses; no worker saw it, so even a POST runs here.
        let app = Arc::new(App::default());
        connect(&west_pool, app.clone()).await;
        app.started.notified().await;
        app.release.notify_one();
        let (status, _, body) = answer.await.unwrap();
        assert_eq!((status, body.as_ref()), (StatusCode::OK, &b"answer"[..]));
        assert_eq!(app.requests.load(Ordering::SeqCst), 1);
        assert!(west_pool.relayed(), "the link stays up for the next request");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Released one region at a time: the west router's new, the east one's not yet, and answers a
    /// request it doesn't know with a 404 of its own. It runs here, and the link waits.
    #[tokio::test]
    async fn a_router_that_doesnt_run_requests_yet_is_let_be() {
        let old: crate::cache::peers::Runs = Arc::new(|_| Box::pin(async { error(StatusCode::NOT_FOUND, "") }));
        let (west, west_pool, _east, dir) = two_routers_running("xc-west-1", "xc-east-1", "dpl", Some(old)).await;
        let answer = send(west, Request::get("/").header(HOST, "api.test").body(Full::new(Bytes::new())).unwrap()).await;
        let app = Arc::new(App::default());
        connect(&west_pool, app.clone()).await;
        app.started.notified().await;
        app.release.notify_one();
        let (status, _, body) = answer.await.unwrap();
        assert_eq!((status, body.as_ref()), (StatusCode::OK, &b"answer"[..]), "not the old router's 404");
        assert!(!west_pool.relayed(), "the link is let go");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!west_pool.relayed(), "and not opened again at once");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn access_log_masks_deploy_hook_secrets() {
        assert_eq!(mask_path("/api/deploy-hooks/shop/0123456789abcdef"), "/api/deploy-hooks/shop/REDACTED");
        assert_eq!(mask_path("/api/deploy-hooks/shop/0123456789abcdef/more"), "/api/deploy-hooks/shop/REDACTED");
        assert_eq!(mask_path("/api/deploy-hooks/shop"), "/api/deploy-hooks/shop");
        assert_eq!(mask_path("/api/deploy-hooks/shop/"), "/api/deploy-hooks/shop/");
        assert_eq!(mask_path("/api/v1/projects/shop/deploy-hooks"), "/api/v1/projects/shop/deploy-hooks");
        assert_eq!(mask_path("/search"), "/search");
        assert_eq!(
            mask_url("https://app.example.com/api/deploy-hooks/shop/0123456789abcdef?buildCache=false&token=t"),
            "https://app.example.com/api/deploy-hooks/shop/REDACTED?buildCache=false&token=REDACTED"
        );
    }

    #[test]
    fn access_log_masks_secret_query_values() {
        assert_eq!(mask_query("location=Austin%2C%20TX&page=2"), "location=Austin%2C%20TX&page=2");
        assert_eq!(mask_query("api_key=abc&city=x&Token=t&X-Amz-Signature=s"), "api_key=REDACTED&city=x&Token=REDACTED&X-Amz-Signature=REDACTED");
        assert_eq!(mask_query("flag&key"), "flag&key");
        assert_eq!(mask_query("keyword=x&author=y&accessToken=z&X-Amz-Credential=c"), "keyword=x&author=y&accessToken=REDACTED&X-Amz-Credential=REDACTED");
        // An OAuth callback: the code signs in whoever holds it with their own verifier.
        assert_eq!(mask_query("code=4f1c2b&state=n0nce.preview.example"), "code=REDACTED&state=n0nce.preview.example");
        assert_eq!(mask_query("oauth_verifier=v&Code=c&zipcode=02139&otp=123456"), "oauth_verifier=REDACTED&Code=REDACTED&zipcode=02139&otp=REDACTED");
        assert_eq!(mask_query("_vercel_share=jwt&SAMLResponse=PHNhbWw%2B"), "_vercel_share=REDACTED&SAMLResponse=REDACTED");
        assert_eq!(mask_url("https://dash.example/auth/github/callback?code=abc&state=s#x"), "https://dash.example/auth/github/callback?code=REDACTED&state=s");
        assert_eq!(truncate("héllo", 2), "h");
        assert_eq!(truncate("abc", 10), "abc");
    }

    fn location(host: Option<&str>, uri: &str) -> (u16, Option<String>) {
        let mut req = Request::builder().uri(uri);
        if let Some(host) = host {
            req = req.header(HOST, host);
        }
        let response = redirect(&req.body(()).unwrap());
        let location = response.headers().get(LOCATION).map(|v| v.to_str().unwrap().to_owned());
        (response.status().as_u16(), location)
    }

    #[test]
    fn redirects_keep_path_and_query() {
        assert_eq!(
            location(Some("Atlas.forkrail.com:80"), "/search?q=Austin%2C%20TX&page=2"),
            (308, Some("https://atlas.forkrail.com/search?q=Austin%2C%20TX&page=2".into()))
        );
        assert_eq!(location(Some("pyapp.forkrail.com"), "/"), (308, Some("https://pyapp.forkrail.com/".into())));
        assert_eq!(location(None, "http://pyapp.forkrail.com/x?y=1"), (308, Some("https://pyapp.forkrail.com/x?y=1".into())));
        assert_eq!(location(Some("35.172.209.138"), "/"), (400, None));
        assert_eq!(location(Some("bad host"), "/"), (400, None));
        assert_eq!(location(None, "/"), (400, None));
        // Health checks get an answer, not a redirect, on any host.
        assert_eq!(location(Some("35.172.209.138"), "/_forkrail/health"), (200, None));
        assert_eq!(location(Some("pyapp.forkrail.com"), "/_forkrail/health"), (200, None));
        assert_eq!(location(Some("pyapp.forkrail.com"), "/_forkrail/health?x=1"), (200, None));
    }
}
