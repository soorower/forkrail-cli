use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use forkrail_common::{
    BUSY_HEADER, CPU_HEADER, EndReason, Inflight, InvokePayload, LOGS_PATH, LOGS_TOKEN_HEADER, PROFILE_HEADER,
    PROFILE_REQUEST_HEADER, Preface, RSS_HEADER, RSS_PEAK_HEADER, RSS_START_HEADER, TrackedBody, WorkerReport,
    send_preface,
};
use hyper::Response;
use hyper::header::{HeaderName, HeaderValue};
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use lambda_runtime::Error;
use tokio::net::TcpStream;
use tokio::sync::{Notify, watch};
use tracing::{info, warn};

use crate::app::UserApp;
use crate::{cpu, echo, egress, env_id, logs, mem, next_invocation, nextcache, tls};

/// Stop taking new work this long before Lambda's hard deadline.
const DEADLINE_MARGIN: Duration = Duration::from_secs(5);
/// After the app exits, how long its requests get to settle before the tunnel drops: each one
/// either answers that it should be retried elsewhere or is held to die with the tunnel.
const EXIT_SETTLE: Duration = Duration::from_millis(300);
/// Time to write out the last answers before dropping the tunnel.
const FLUSH: Duration = Duration::from_millis(20);

const WORKER_HEADER: HeaderName = HeaderName::from_static("x-forkrail-worker");
const SCHEDULE_HEADER: HeaderName = HeaderName::from_static(forkrail_common::schedules::SCHEDULE_HEADER);

/// `Authorization` for a schedule's run: `Bearer $CRON_SECRET` from this deployment's own
/// environment, so the secret sent and the one the app checks are always the same deployment's.
/// None without the variable, or with a value that can't be a header (the build refuses those).
fn cron_authorization() -> Option<HeaderValue> {
    static VALUE: std::sync::OnceLock<Option<HeaderValue>> = std::sync::OnceLock::new();
    VALUE
        .get_or_init(|| {
            let secret = crate::userenv::var(forkrail_common::schedules::SECRET_ENV).filter(|s| !s.is_empty())?;
            HeaderValue::from_str(&format!("Bearer {secret}")).ok()
        })
        .clone()
}
const ENV_HEADER: HeaderName = HeaderName::from_static("x-forkrail-env");

/// Compares secrets without stopping at the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What serves the requests.
pub enum App {
    Echo,
    User(Arc<UserApp>),
}

impl App {
    async fn exited(&self) {
        match self {
            App::Echo => std::future::pending().await,
            App::User(app) => app.exited().await,
        }
    }
}

/// Resolves once nothing has been in flight for `after`, and no `waitUntil` work is running: a
/// worker busy only with that isn't idle, and the router keeps it too.
async fn idle(inflight: &Inflight, after: Duration) {
    loop {
        inflight.wait_idle(after).await;
        if crate::app::background() == 0 {
            return;
        }
        crate::app::background_settled().await;
    }
}

/// One invocation: connect to the router and serve until it closes the tunnel.
pub async fn run(payload: InvokePayload, deadline: SystemTime, app: App) -> Result<WorkerReport, Error> {
    let env_invocations = next_invocation();
    let started = Instant::now();
    mem::reset_peak();

    // Before the tunnel brings the app requests: their calls back to the install carry the mark.
    if let Some(own) = &payload.own_hosts {
        crate::ownhosts::note(own);
    }
    let tcp = TcpStream::connect(&payload.router_addr).await?;
    tcp.set_nodelay(true)?;
    // For counting what the environment sends anywhere else (`net.rs`), when the router asked.
    let tunnel_socket = payload.report_net.then(|| std::os::fd::AsFd::as_fd(&tcp).try_clone_to_owned().ok()).flatten();
    let mut stream = tls::connect(&payload.cert_sha256, tcp).await?;
    let preface = Preface {
        worker_id: payload.worker_id.clone(),
        token: payload.token.clone(),
        limit_kb: mem::limit_kb(),
        rss_kb: mem::rss_kb(),
        // The echo app prints nothing.
        logs: matches!(app, App::User(_)),
        env_invocations: Some(env_invocations),
        // A Next.js app's caches are the router's (`nextcache.rs`).
        cache: nextcache::enabled(),
    };
    send_preface(&mut stream, &preface).await?;
    // The app's egress connections go out as this worker until the invocation ends.
    let _egress = egress::Active::begin(&payload);
    let connect_ms = started.elapsed().as_millis() as u64;
    info!(worker = %payload.worker_id, env = env_id(), env_invocations, connect_ms, "tunnel up");

    let app = Arc::new(app);
    let inflight = Arc::new(Inflight::new(Arc::new(Notify::new())));
    let served = Arc::new(AtomicU64::new(0));
    // Requests reset the peak counters, so the invocation's own peak is kept here.
    let peak = Arc::new(AtomicU64::new(0));
    let worker_header = HeaderValue::from_str(&payload.worker_id)?;
    let env_header = HeaderValue::from_static(env_id());
    // Set when the worker shuts down on its own, which ends the log stream, so the tunnel's
    // graceful shutdown doesn't wait for it.
    let (closing_tx, closing) = watch::channel(false);
    let token = Arc::new(payload.token.clone());
    // A router that names the deployment's maxDuration wants to hear of `waitUntil` work, and keeps
    // this worker while there is some.
    let tells_background = payload.max_duration_s.is_some();
    // A router that asked hears what the environment sends anywhere else (`net.rs`).
    let reports_net = tunnel_socket.is_some();
    let net_before = *crate::net::watch().borrow();
    let net_meter = tunnel_socket.map(|socket| tokio::spawn(crate::net::run(socket, closing.clone())));

    // Roles flip here: we dialed out, but we are the HTTP/2 server.
    let service = {
        let (app, inflight, served, peak) = (app.clone(), inflight.clone(), served.clone(), peak.clone());
        service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
            let (app, inflight, served, peak) = (app.clone(), inflight.clone(), served.clone(), peak.clone());
            let (worker_header, env_header) = (worker_header.clone(), env_header.clone());
            let (closing, token) = (closing.clone(), token.clone());
            async move {
                // The router's log stream, which isn't a request: not counted, not sent to the app.
                // Without the token it's an ordinary request, for the app.
                if req.uri().path() == LOGS_PATH
                    && req.headers().get(LOGS_TOKEN_HEADER).is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes()))
                {
                    let body = TrackedBody::new(logs::stream(closing, tells_background.then(crate::app::background_watch), reports_net.then(crate::net::watch)), None);
                    return Ok::<_, Error>(Response::new(body));
                }
                // The router's stream for Next.js's cache (`nextcache.rs`): not a request either.
                if req.uri().path() == forkrail_common::nextcache::PATH
                    && req.headers().get(forkrail_common::nextcache::TOKEN_HEADER).is_some_and(|t| constant_time_eq(t.as_bytes(), token.as_bytes()))
                {
                    let body = TrackedBody::new(nextcache::attach(req.into_body(), closing), None);
                    return Ok::<_, Error>(Response::new(body));
                }
                // Counts the request as in flight until its response body finishes.
                let guard = inflight.enter();
                let request_no = served.fetch_add(1, Ordering::Relaxed) + 1;
                // Alone, the peak from here on is this request's own.
                let alone = inflight.count() == 1;
                if alone {
                    peak.fetch_max(mem::rss_and_peak_kb().1, Ordering::Relaxed);
                    mem::reset_peak();
                }
                let rss_start = mem::rss_kb();
                // The router marks a schedule's run (and takes the mark off any visitor's request).
                if req.headers_mut().remove(SCHEDULE_HEADER).is_some() {
                    match cron_authorization() {
                        Some(value) => req.headers_mut().insert(hyper::header::AUTHORIZATION, value),
                        None => req.headers_mut().remove(hyper::header::AUTHORIZATION),
                    };
                }
                let profile = req.headers_mut().remove(PROFILE_REQUEST_HEADER).is_some() && alone;
                let busy_start = Instant::now();
                let cpu_start = if alone { cpu::cpu_us() } else { 0 };
                let sampler = profile.then(cpu::Sampler::start);
                let response = match &*app {
                    App::Echo => {
                        let worker_id = worker_header.to_str().unwrap_or_default();
                        let ctx = echo::Context { worker_id, request_no, inflight: inflight.count() };
                        echo::handle(req, &ctx).await
                    }
                    App::User(app) => app.handle(req).await?,
                };
                let (mut parts, body) = response.into_parts();
                let headers = &mut parts.headers;
                // The measurements below are the runtime's alone: an app's own never stand in for
                // them when the runtime has none to give (another request ran meanwhile).
                for name in [RSS_PEAK_HEADER, CPU_HEADER, BUSY_HEADER, PROFILE_HEADER] {
                    headers.remove(name);
                }
                headers.insert(WORKER_HEADER, worker_header);
                headers.insert(ENV_HEADER, env_header);
                // Lets the router learn what each route costs in memory.
                let (rss, rss_peak) = mem::rss_and_peak_kb();
                peak.fetch_max(rss_peak, Ordering::Relaxed);
                headers.insert(RSS_START_HEADER, HeaderValue::from(rss_start));
                headers.insert(RSS_HEADER, HeaderValue::from(rss));
                if alone {
                    headers.insert(RSS_PEAK_HEADER, HeaderValue::from(rss_peak.max(rss)));
                }
                // Nothing else arrived meanwhile, so the CPU the environment used is this request's.
                if alone && served.load(Ordering::Relaxed) == request_no {
                    headers.insert(CPU_HEADER, HeaderValue::from(cpu::cpu_us().saturating_sub(cpu_start)));
                    headers.insert(BUSY_HEADER, HeaderValue::from(busy_start.elapsed().as_micros() as u64));
                    if let Some(sampler) = sampler
                        && let Ok(value) = HeaderValue::from_str(&sampler.finish().await)
                    {
                        headers.insert(PROFILE_HEADER, value);
                    }
                }
                Ok::<_, Error>(Response::from_parts(parts, TrackedBody::new(body, Some(guard))))
            }
        })
    };
    let mut conn = Box::pin(
        http2::Builder::new(TokioExecutor::new())
            // Two more, for the log stream and the cache stream.
            .max_concurrent_streams(payload.max_inflight + 2)
            .max_header_list_size(forkrail_common::TUNNEL_MAX_HEADER_LIST)
            .serve_connection(TokioIo::new(stream), service),
    );

    let until_deadline = deadline
        .duration_since(SystemTime::now())
        .unwrap_or_default()
        .saturating_sub(DEADLINE_MARGIN);
    let end_reason = tokio::select! {
        result = conn.as_mut() => {
            if let Err(e) = result {
                warn!(error = %e, "tunnel ended with error");
            }
            EndReason::RouterClosed
        }
        _ = idle(&inflight, Duration::from_millis(payload.idle_ms)) => EndReason::Idle,
        _ = tokio::time::sleep(until_deadline) => EndReason::Deadline,
        _ = app.exited() => EndReason::AppExited,
    };
    let _ = closing_tx.send(true);
    match end_reason {
        EndReason::RouterClosed => {}
        EndReason::AppExited => {
            // GOAWAY stops new requests. Once every request in flight has either answered (to be
            // retried elsewhere) or is held, drop the tunnel: the router then handles the held
            // ones as having killed the worker.
            conn.as_mut().graceful_shutdown();
            if let App::User(app) = &*app {
                let settled = async {
                    while inflight.count() > app.held() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    tokio::time::sleep(FLUSH).await;
                };
                tokio::select! {
                    _ = conn.as_mut() => {}
                    _ = tokio::time::timeout(EXIT_SETTLE, settled) => {}
                }
            }
            drop(conn);
        }
        EndReason::Idle | EndReason::Deadline => {
            // Sends GOAWAY and lets in-flight streams finish.
            conn.as_mut().graceful_shutdown();
            if let Err(e) = conn.await {
                warn!(error = %e, "tunnel shutdown error");
            }
        }
    }

    // `waitUntil` work still running when the worker is done: the invocation stays open until it
    // settles, as Vercel keeps a function running for it. Returned, Lambda would freeze the
    // environment under the work until its next thaw. A router that knows of the work keeps the
    // worker while it runs, so this is what's left when it didn't (an older router, the worker's
    // age, a deployment that left the table) or gave up on it at maxDuration. Nothing new comes
    // either way. The work gets the deployment's maxDuration from here, where the router said it,
    // and never more than the invocation has left.
    if end_reason != EndReason::AppExited && crate::app::background() > 0 {
        let left = deadline.duration_since(SystemTime::now()).unwrap_or_default().saturating_sub(DEADLINE_MARGIN);
        let allowed = payload.max_duration_s.map_or(left, |s| left.min(Duration::from_secs(s)));
        let waited = Instant::now();
        let settled = tokio::time::timeout(allowed, crate::app::background_settled()).await.is_ok();
        info!(waited_ms = waited.elapsed().as_millis() as u64, settled, "waitUntil work kept the invocation open");
        if !settled {
            // For the app's own log, read by the next worker's stream: the work isn't stopped, and
            // goes on whenever the environment next runs.
            logs::push(
                "stderr",
                format!("waitUntil / after() work was still running {} s after its response: the function was suspended under it", allowed.as_secs()),
                None,
            );
        }
    }

    // The meter's last count, taken as the invocation ends, goes in the report.
    if let Some(meter) = net_meter {
        let _ = tokio::time::timeout(Duration::from_secs(1), meter).await;
    }
    let net_out = reports_net.then(|| crate::net::watch().borrow().saturating_sub(net_before));
    let memory = mem::snapshot();
    let report = WorkerReport {
        worker_id: payload.worker_id,
        env_id: env_id().to_owned(),
        env_invocations,
        requests: served.load(Ordering::Relaxed),
        connect_ms,
        active_ms: started.elapsed().as_millis() as u64,
        end_reason,
        app_start_ms: match &*app {
            App::Echo => 0,
            App::User(app) => app.take_start_ms(),
        },
        rss_peak_kb: memory.rss_peak_kb.max(peak.load(Ordering::Relaxed)),
        mem_total_kb: memory.total_kb,
        mem_available_kb: memory.available_kb,
        net_out,
    };
    info!(?report, "invocation done");
    Ok(report)
}
