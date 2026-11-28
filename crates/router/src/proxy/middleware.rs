//! Routing Middleware (C16 F6): when the routes reach a route with `middlewarePath`, the router
//! calls that function (in the deployment's own Lambda, through its dispatcher, as any of its
//! functions) with the request, and reads its answer as Vercel's proxy does
//! (`forkrail_common::rules::middleware`): go on routing, go on with another path or to a URL
//! elsewhere, or answer with what it said. Routing then goes on from the route after the
//! middleware's, so `vercel.json` redirects and headers before it run first, and the filesystem and
//! rewrites after.
//!
//! The middleware gets the request as the function would (the client's path, method, headers and,
//! when the router has read it in full, its body), with `x-forkrail-middleware: 1` so the runtime
//! answers for it as Vercel's runtime answers for a middleware (one that returns nothing goes on).
//! It must start answering within its own maxDuration if the build gave it one, else
//! `MIDDLEWARE_TIMEOUT` (Vercel's limit for Edge middleware to begin answering), and never past the
//! deployment's maxDuration. Each call is logged in the request's access line (`middleware`, what
//! it said; `middleware_ms`, how long it took).

use super::*;
use forkrail_common::rules::middleware::{self as protocol, Destination, Verdict};
use forkrail_common::rules::{Paused, Resume, Step};

/// The header the router sends on a middleware's call; never from a client.
pub const CALL_HEADER: HeaderName = HeaderName::from_static("x-forkrail-middleware");
/// How long a middleware without a maxDuration of its own may take to start answering.
const MIDDLEWARE_TIMEOUT: Duration = Duration::from_secs(25);
/// Middleware calls for one request, at most: a program with several middleware routes runs each
/// once, and this stops one that would go round.
const MAX_CALLS: usize = 8;
/// The most of a `next` or `rewrite` answer's body the router reads before dropping it: the
/// protocol's answers are empty.
const MAX_IGNORED_BODY: usize = 64 << 10;
// The largest request body a middleware gets is a function's (`max_body`, its headers counting
// towards it): seen on forkrail-check-fw-vite-middleware, 2026-11-16, a POST of 4,493,720 bytes ran
// the middleware and one of 4,494,334 was refused with 413 `FUNCTION_PAYLOAD_TOO_LARGE`, as a
// function's are.

/// Routing stopped at a middleware's route.
pub struct Pending {
    pub engine: Arc<Engine>,
    pub manifest: Option<Arc<forkrail_common::statics::Manifest>>,
    /// The middleware's function path.
    pub function: String,
    pub paused: Paused,
}

/// How a request's middleware left it.
pub enum Ran {
    /// The middleware's answer, or the router's on its behalf: the log line is written.
    Answered(Response<ResponseBody>),
    /// Routing went on to this, with the request's body as the router read it.
    Routed(Routed, UpstreamBody),
}

/// A client's `x-forkrail-middleware` would read as the router's call. (Its `x-middleware-*`
/// headers are taken off with the platform's others when the request comes in, `to_upstream`.)
pub fn strip_client_headers(headers: &mut HeaderMap) {
    headers.remove(CALL_HEADER);
}

/// Take the protocol's headers off a function's answer to the client.
pub fn strip_protocol(headers: &mut HeaderMap) {
    for name in protocol_headers(headers) {
        headers.remove(name);
    }
}

/// Call the middleware `pending` names and route on as it says, through any others the routes
/// reach after it.
pub async fn run(proxy: &Arc<Proxy>, target: &Target, head: &mut UpstreamHead, incoming: Incoming, mut pending: Pending, client_version: Version, log: &mut AccessLog) -> Ran {
    let started = Instant::now();
    let Some(pool) = target.pool.clone() else {
        // A middleware route with no function to run it: a build forkrail would have refused.
        let response = coded(StatusCode::INTERNAL_SERVER_ERROR, "MIDDLEWARE_INVOCATION_FAILED", "A server error has occurred");
        log.middleware = Some("failed");
        return Ran::Answered(log_now(log, response, "middleware"));
    };
    let mut body = match read_all(incoming, &head.headers, proxy, max_body(head.payload), client_version).await {
        Ok(body) => body,
        Err(Unread::TooLarge) => {
            log.middleware = Some("too_large");
            let response = coded(StatusCode::PAYLOAD_TOO_LARGE, "FUNCTION_PAYLOAD_TOO_LARGE", "Request Entity Too Large");
            return Ran::Answered(log_now(log, response, "too_large"));
        }
        Err(Unread::Failed(e)) => {
            warn!(request = %log.request_id, error = %e, "reading request body failed");
            return Ran::Answered(log_now(log, error(StatusCode::BAD_REQUEST, "could not read request body"), "bad_request"));
        }
    };
    log.bytes_in = body.len();
    // The middleware's lines and calls count towards the request's, as the function's do.
    pool.logs().begin(&log.request_id);
    log.app_logs = Some(pool.logs().clone());
    let mut added: Vec<(String, String)> = Vec::new();
    for _ in 0..MAX_CALLS {
        let limit = pending.engine.max_duration(&pending.function).map_or(MIDDLEWARE_TIMEOUT, Duration::from_secs).min(pool.max_duration());
        let deadline = tokio::time::Instant::now() + limit;
        let (response, lease) = match call(&pool, head, &mut body, &pending.function, deadline, log).await {
            Ok(called) => called,
            Err(response) => {
                log.middleware_ms = Some(started.elapsed().as_millis() as u64);
                return Ran::Answered(log_now(log, response, "middleware"));
            }
        };
        let (mut parts, upstream) = response.into_parts();
        worker_measures(&mut parts.headers, &lease, &pool, log);
        let reading = protocol::read(parts.status.as_u16(), &parts.headers, head.host.split(':').next().unwrap_or(&head.host));
        log.middleware = Some(match &reading.verdict {
            Verdict::Next => "next",
            Verdict::Rewrite(Destination::Here { .. }) => "rewrite",
            Verdict::Rewrite(Destination::Elsewhere(_)) => "external",
            Verdict::Redirect { .. } => "redirect",
            Verdict::Answer => "answer",
        });
        log.middleware_ms = Some(started.elapsed().as_millis() as u64);
        let rule_headers = route_headers(&pending.paused);
        let rule_transforms = pending.paused.transforms().to_vec();
        match reading.verdict {
            Verdict::Answer => {
                // The middleware's own answer is the response, as a function's is (relay takes
                // the protocol's headers off), with the routes' headers so far where it set none.
                let response = Response::from_parts(parts, upstream);
                let effects = Effects { headers: rule_headers, transforms: rule_transforms, ..Effects::default() };
                let taken = std::mem::replace(log, AccessLog::spent());
                let deadline = tokio::time::Instant::now() + pool.max_duration();
                return Ran::Answered(relay(response, lease, &pool, deadline, client_version, proxy, &effects, taken).await);
            }
            Verdict::Redirect { status, location } => {
                drain(upstream).await;
                drop(lease);
                let status = StatusCode::from_u16(status).unwrap_or(StatusCode::TEMPORARY_REDIRECT);
                let mut response = redirect_response(status, &location, log);
                // What the middleware set besides (a cookie, say) goes with it.
                for (name, value) in &parts.headers {
                    let n = name.as_str();
                    if n == "location" || n.starts_with(protocol::PREFIX) || ["content-type", "content-length", "transfer-encoding", "date", "connection"].contains(&n) {
                        continue;
                    }
                    response.headers_mut().append(name.clone(), value.clone());
                }
                let response = respond_headers(response, &rule_headers, &rule_transforms);
                return Ran::Answered(log_now(log, response, "redirect"));
            }
            Verdict::Next | Verdict::Rewrite(_) => {
                drain(upstream).await;
                drop(lease);
            }
        }
        if let Some(given) = &reading.request_headers {
            protocol::override_request(&mut head.headers, given);
        }
        added.extend(reading.response_headers);
        let resume = match reading.verdict {
            Verdict::Rewrite(Destination::Elsewhere(url)) => {
                let mut effects = Effects { headers: rule_headers, transforms: rule_transforms, ..Effects::default() };
                add(&mut effects, &added);
                return Ran::Routed(Routed::External { url, effects }, body);
            }
            Verdict::Rewrite(Destination::Here { path, query }) => {
                // The rewrite's query replaces the client's; the path the function gets stays the
                // client's, as with any rewrite.
                head.uri = with_query(&head.uri, query.as_deref().unwrap_or(""));
                Resume::Rewrite { path }
            }
            _ => Resume::Next,
        };
        let step = {
            let plain: &(dyn Files + Sync) = match pending.manifest.as_deref() {
                Some(manifest) => manifest,
                None => &NoFiles,
            };
            // The optimizer's paths are files of the deployment's when it has image settings.
            let with_images = super::ImageFiles(plain);
            let files: &dyn Files = if target.images.is_some() { &with_images } else { plain };
            // Its own copy of the host: a `mitigate` answer writes the log.
            let host = log.host.clone();
            let request = rules::Request { method: head.method.as_str(), path: head.uri.path(), query: head.uri.query(), host: &host, headers: &head.headers };
            let mut step = pending.engine.resume(&request, files, pending.paused, resume);
            // A `mitigate` route after the middleware's: challenged or denied as before one.
            while let Step::Mitigate { action, route, paused } = step {
                match super::mitigated(proxy, head, log, action, pending.engine.rule_name(route)) {
                    Some(response) => return Ran::Answered(response),
                    None => step = pending.engine.resume(&request, files, paused, Resume::Next),
                }
            }
            step
        };
        match step {
            Step::Middleware { function, paused, .. } => {
                pending = Pending { engine: pending.engine, manifest: pending.manifest, function, paused };
            }
            Step::Mitigate { .. } => unreachable!("settled above"),
            Step::Decided(decision) => {
                let Some(statics) = proxy.statics.as_ref() else { unreachable!("routes ran, so the router has statics") };
                let routed = settle(proxy, target, head, log, &pending.engine, pending.manifest.as_ref(), statics, decision).await;
                let routed = match routed {
                    Routed::Done(response) => Routed::Done(onto(response, &added)),
                    Routed::Function(mut effects) => {
                        add(&mut effects, &added);
                        Routed::Function(effects)
                    }
                    Routed::External { url, mut effects } => {
                        add(&mut effects, &added);
                        Routed::External { url, effects }
                    }
                    Routed::Middleware(_) => unreachable!("settle has a decision"),
                };
                return Ran::Routed(routed, body);
            }
        }
    }
    warn!(request = %log.request_id, "routing reached more middleware calls than it allows");
    let response = coded(StatusCode::LOOP_DETECTED, "MIDDLEWARE_LOOP", "Routing ran the deployment's middleware too many times.");
    Ran::Answered(log_now(log, response, "middleware"))
}

/// The whole request body, for the middleware and then whatever routing reaches: a middleware gets
/// the body as the function would, so both need it, and the router can't send a stream twice.
/// Past `max`, or past what the router-wide buffer has room for now, it's `TooLarge`: answered as
/// Vercel answers a body over its limit, rather than giving the middleware less than was sent
/// (the rest read and thrown away first, `drain_refused`).
async fn read_all(mut body: Incoming, headers: &HeaderMap, proxy: &Proxy, max: u64, version: Version) -> Result<UpstreamBody, Unread> {
    if body.is_end_stream() {
        return Ok(UpstreamBody::Buffered { bytes: Bytes::new(), _room: None });
    }
    let length = headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if length.is_some_and(|n| n > max) {
        drain_refused(body, version, headers, 0).await;
        return Err(Unread::TooLarge);
    }
    let mut bytes = BytesMut::new();
    let mut room: Option<OwnedSemaphorePermit> = None;
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame.map_err(Unread::Failed)?.into_data() else { continue };
        let read = (bytes.len() + data.len()) as u64;
        if read > max {
            drop((bytes, room));
            drain_refused(body, version, headers, read).await;
            return Err(Unread::TooLarge);
        }
        let Ok(permit) = proxy.buffered.clone().try_acquire_many_owned(data.len().min(proxy.buffer_total) as u32) else {
            drop((bytes, room));
            drain_refused(body, version, headers, read).await;
            return Err(Unread::TooLarge);
        };
        match &mut room {
            Some(room) => room.merge(permit),
            None => room = Some(permit),
        }
        bytes.extend_from_slice(&data);
    }
    Ok(UpstreamBody::Buffered { bytes: bytes.freeze(), _room: room })
}

/// Send the middleware its call, on a worker of the deployment's pool. An attempt that never
/// reached a worker is tried again on another; anything else is the middleware's failure.
async fn call(pool: &Arc<Pool>, head: &UpstreamHead, body: &mut UpstreamBody, function: &str, deadline: tokio::time::Instant, log: &mut AccessLog) -> Result<(Response<Incoming>, Lease), Response<ResponseBody>> {
    let failed = |log: &mut AccessLog, code: &str| {
        log.middleware = Some("failed");
        coded(StatusCode::INTERNAL_SERVER_ERROR, code, "A server error has occurred")
    };
    let timed_out = |log: &mut AccessLog| {
        log.middleware = Some("timeout");
        coded(StatusCode::GATEWAY_TIMEOUT, "MIDDLEWARE_INVOCATION_TIMEOUT", "The middleware took too long to answer.")
    };
    let mut call_head = UpstreamHead { host: head.host.clone(), method: head.method.clone(), uri: head.uri.clone(), headers: head.headers.clone(), payload: head.payload };
    if let Ok(value) = HeaderValue::from_str(function) {
        call_head.headers.insert(forkrail_common::FUNCTION_HEADER, value);
    }
    call_head.headers.insert(CALL_HEADER, HeaderValue::from_static("1"));
    let mut tried: Vec<String> = Vec::new();
    for _ in 0..MAX_ATTEMPTS {
        log.attempts += 1;
        let queued = Instant::now();
        let acquired = tokio::time::timeout_at(deadline, pool.acquire(function, &tried, false)).await;
        log.queue += queued.elapsed();
        let lease = match acquired {
            Ok(Ok(lease)) => lease,
            Ok(Err(unavailable)) => {
                log.middleware = Some("unavailable");
                return Err(service_unavailable(unavailable.retry_after));
            }
            Err(_) => return Err(timed_out(log)),
        };
        log.worker = Some(lease.worker.id.clone());
        log.start = Some(lease.start);
        // Read in full (`read_all`), so the function gets it again after.
        let request = call_head.request(body.take().expect("a buffered body is always there"));
        let mut sender = lease.sender();
        let sent = match tokio::time::timeout_at(deadline, sender.try_send_request(request)).await {
            Ok(sent) => sent,
            Err(_) => return Err(timed_out(log)),
        };
        match sent {
            Ok(response) if response.headers().contains_key(RETRY_HEADER) => {
                // The app or its connection failed under it: the middleware's failure, as a
                // function's crash is (its request may have run partway).
                warn!(request = %log.request_id, worker = %lease.worker.id, "middleware's app failed");
                if response.headers().get(RETRY_HEADER).is_none_or(|v| v != RETRY_APP_CONNECTION) {
                    pool.remove(&lease.worker.id);
                }
                return Err(failed(log, "MIDDLEWARE_INVOCATION_FAILED"));
            }
            Ok(response) => return Ok((response, lease)),
            Err(mut e) => {
                if e.take_message().is_some() {
                    // Never reached the worker: another may take it.
                    tried.push(lease.worker.id.clone());
                    pool.remove(&lease.worker.id);
                    continue;
                }
                warn!(request = %log.request_id, worker = %lease.worker.id, error = %error_chain(e.error()), "middleware call failed");
                pool.worker_died(&lease.worker);
                return Err(failed(log, "MIDDLEWARE_INVOCATION_FAILED"));
            }
        }
    }
    Err(failed(log, "MIDDLEWARE_INVOCATION_FAILED"))
}

/// Read what's left of an answer whose body the protocol ignores, so the worker's stream ends
/// cleanly; past `MAX_IGNORED_BODY`, dropping it resets the stream instead.
async fn drain(mut body: Incoming) {
    let mut read = 0usize;
    while read <= MAX_IGNORED_BODY {
        match tokio::time::timeout(Duration::from_secs(1), body.frame()).await {
            Ok(Some(Ok(frame))) => read += frame.data_ref().map_or(0, Bytes::len),
            _ => return,
        }
    }
}

/// The protocol's headers on a middleware's answer, which never reach the client.
fn protocol_headers(headers: &HeaderMap) -> Vec<HeaderName> {
    headers.keys().filter(|k| k.as_str().starts_with(protocol::PREFIX)).cloned().collect()
}

/// The response headers the routes before the middleware set.
fn route_headers(paused: &Paused) -> Vec<(HeaderName, HeaderValue)> {
    paused.headers().iter().filter_map(|(k, v)| Some((HeaderName::from_bytes(k.as_bytes()).ok()?, HeaderValue::from_str(v).ok()?))).collect()
}

/// A middleware's headers for the final answer, as a route's headers go (a function's own of the
/// same name win), but for those that stand beside the answer's own (`set-cookie`).
fn add(effects: &mut Effects, added: &[(String, String)]) {
    for (name, value) in added {
        let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) else { continue };
        if protocol::adds_beside(name.as_str()) {
            effects.added.push((name, value));
        } else {
            effects.headers.retain(|(n, _)| *n != name);
            effects.headers.push((name, value));
        }
    }
}

/// A middleware's headers on an answer the router made itself (a file, a redirect, an error),
/// set over its own as the routes' are.
fn onto(mut response: Response<ResponseBody>, added: &[(String, String)]) -> Response<ResponseBody> {
    let mut first = std::collections::HashSet::new();
    for (name, value) in added {
        let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) else { continue };
        if protocol::adds_beside(name.as_str()) || !first.insert(name.clone()) {
            response.headers_mut().append(name, value);
        } else {
            response.headers_mut().insert(name, value);
        }
    }
    response
}
