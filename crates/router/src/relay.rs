//! Requests for a deployment with no version in this router's region go to the router of the
//! region that runs it, when that region has one, over the routers' link (`cache/peers.rs`); that
//! router runs them on its own workers. Vercel's shape: the edge that takes the request passes it
//! to the function's region over its own network.
//!
//! Why: a router that invokes a function in another region has its worker dial back across the
//! country. Measured 2026-11-22 (an API in us-east-1 only, called through us-west-1): a worker
//! connected in 194 ms (17 ms inside us-east-1), and since an idle worker is kept only as long as
//! that's cheaper than a new invocation (20 ms at 768 MB), 83% of requests waited for one: 256 ms
//! over what the function took, where the router in us-east-1 adds 2 to 19 ms. Over the link,
//! which stays open, a request costs one round trip between the regions (about 62 ms) plus that
//! router's own start, and both routers' requests share its warm workers.
//!
//! - **The asking router** keeps one HTTP/2 connection open to every other region's router it
//!   knows of (`start`, from `routers.rs`), opening and closing them as routers come and go. A
//!   pool whose deployment runs in a region with a router holds that region's link from when it's
//!   made (`PoolConfig::relay`, a `Slot`; a deployment in a region with no router has none, and
//!   its workers dial back from there) and leases it as if it were a worker (`Slot::worker`, one
//!   atomic load): `proxy::exchange` sends the request on it with
//!   the deployment's id (`mark`), and takes the answer as a worker's: what the worker measured,
//!   how it started and which worker it was come back with it (`ran`, read by `take_ran`).
//!   While the connection is down, or for a body too big to send again, the pool starts workers
//!   there itself, as before.
//! - **The router that runs it** (`proxy::peer_runs`) takes it on its own pool for that
//!   deployment, as a request of its own, and answers with the worker's answer as it came: the
//!   asking router writes the access log line, applies the routes' headers and keeps the answer
//!   in its cache. It refuses (`refused`) a deployment it doesn't run itself, so a request is
//!   passed on once at most, and any request while it drains; the asking router then runs it
//!   itself, which is safe since no worker saw it.
//!
//! Firewall, rate limits, bot checks, the cache, spend on transfer and the access log stay with
//! the router the visitor reached. The function's invocations, its app logs (by the request's id)
//! and its keep-warm are the running router's.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame, SizeHint};
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Request, Response, StatusCode};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::BoxError;
use crate::cache::Link;
use crate::pool::Worker;
use crate::proxy::{ProxyBody, ResponseBody};
use crate::routers::Routers;

/// On a request to another router: the deployment to run it on.
pub const RUN_HEADER: &str = "x-forkrail-run";
/// With it: the request must have a worker to itself (it was there when one died).
const ALONE_HEADER: &str = "x-forkrail-run-alone";
/// With it: the function's own maxDuration under the deployment's, in ms.
const MAX_MS_HEADER: &str = "x-forkrail-run-max-ms";
/// On the answer: `<start>;<queue ms>;<memory KiB or empty>;<worker>`, as the running router's
/// access log would have said.
const RAN_HEADER: &str = "x-forkrail-ran";
/// On the answer when it didn't run it, and why: `elsewhere` (not a version it runs), `unknown`
/// (not in its table, or no function), `draining` (it's stopping).
const REFUSED_HEADER: &str = "x-forkrail-run-refused";
/// On every answer to a request it was sent, refusals included: a router from before this, which
/// doesn't run requests for others, answers one with a 404 of its own and without it.
const ANSWER_HEADER: &str = "x-forkrail-run-answer";
/// The worker id a pool leases the link to `region` under.
const ID_PREFIX: &str = "relay:";
/// Back-off between attempts to open a link that's down.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// How often a link that's up is checked for a request having found it broken.
const CHECK_EVERY: Duration = Duration::from_millis(500);
/// A router that doesn't run requests for others (one not yet released, while the other region's
/// is) isn't asked again for this long.
const OLD_WAIT: Duration = Duration::from_secs(60);

/// Each region's link, as the pools of deployments that run there hold it (`slot`).
static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Slot>>>> = OnceLock::new();
/// Requests this router is running for others, which a router that stops lets finish.
static RUNNING: AtomicUsize = AtomicUsize::new(0);
/// Regions whose router answered as one that doesn't run requests for others, until when it
/// isn't asked again.
static OLD: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

/// What the asking router adds to the request on the link, and expects on the answer.
pub struct Proofs {
    pub ours: HeaderValue,
    pub theirs: String,
}

/// The link to one region's router, as the pool of a deployment that runs there holds it from
/// when it's made (`registry.rs`): a request reads it with one atomic load, and the link's keeper
/// fills it while the link is up and empties it when it's down. Nothing is looked up per request.
pub struct Slot {
    region: String,
    worker: ArcSwapOption<Worker>,
}

impl Slot {
    pub fn region(&self) -> &str {
        &self.region
    }

    /// The link, if it's up.
    pub fn worker(&self) -> Option<Arc<Worker>> {
        self.worker.load_full().filter(|w| w.usable())
    }

    /// Empty it, if it still holds `worker` (a newer keeper's link stays).
    fn clear(&self, worker: &Arc<Worker>) {
        self.worker.rcu(|current| match current {
            Some(w) if Arc::ptr_eq(w, worker) => None,
            other => other.clone(),
        });
    }
}

/// Region `region`'s slot: one per region, made the first time a pool asks (a handful at most),
/// and kept.
pub fn slot(region: &str) -> Arc<Slot> {
    let mut slots = SLOTS.get_or_init(Mutex::default).lock().unwrap();
    slots.entry(region.to_owned()).or_insert_with(|| Arc::new(Slot { region: region.to_owned(), worker: ArcSwapOption::empty() })).clone()
}

/// Keep a connection open to every other region's router, as `routers` knows them, for good: one
/// is opened for a router that's heard of, and closed for one that's gone or moved.
pub fn start(link: Arc<Link>, mut routers: watch::Receiver<Arc<Routers>>) {
    tokio::spawn(async move {
        let mut keepers: HashMap<String, (String, watch::Sender<bool>)> = HashMap::new();
        loop {
            let others = routers.borrow_and_update().others();
            keepers.retain(|region, (addr, stop)| {
                let stays = others.get(region) == Some(&*addr);
                if !stays {
                    let _ = stop.send(true);
                    info!(%region, %addr, "that router is gone or moved: its link closes");
                }
                stays
            });
            let mut opened = Vec::new();
            for (region, addr) in others {
                if let std::collections::hash_map::Entry::Vacant(entry) = keepers.entry(region.clone()) {
                    let (stop, stopped) = watch::channel(false);
                    tokio::spawn(keep(link.clone(), region.clone(), addr.clone(), slot(&region), stopped));
                    entry.insert((addr, stop));
                    opened.push(region);
                }
            }
            if !opened.is_empty() {
                let mut routers: Vec<&str> = keepers.keys().map(String::as_str).collect();
                routers.sort_unstable();
                info!(?routers, ?opened, "requests for deployments that run in another router's region go to that router");
            }
            if routers.changed().await.is_err() {
                // Nothing will change them again: the links stay as they are.
                std::future::pending::<()>().await;
            }
        }
    });
}

async fn keep(link: Arc<Link>, region: String, addr: String, slot: Arc<Slot>, mut stop: watch::Receiver<bool>) {
    let mut backoff = BACKOFF_MIN;
    let mut said_down = false;
    loop {
        let connected = tokio::select! {
            connected = link.connect_as::<ProxyBody>(&addr) => connected,
            _ = stop.changed() => return,
        };
        match connected {
            Ok((send, ours, theirs, mut closed)) => {
                let worker = Worker::relay(id(&region), send, Proofs { ours, theirs });
                slot.worker.store(Some(worker.clone()));
                info!(%region, "link up: requests for deployments run there go to its router");
                backoff = BACKOFF_MIN;
                said_down = false;
                // Until the connection ends, a request finds it can't be used (`Pool::worker_died`),
                // or the router is gone.
                let gone = loop {
                    tokio::select! {
                        _ = &mut closed => break false,
                        () = tokio::time::sleep(CHECK_EVERY) => if !worker.usable() { break false },
                        _ = stop.changed() => break true,
                    }
                };
                slot.clear(&worker);
                if gone {
                    return;
                }
                let old = OLD.get().and_then(|old| old.lock().unwrap().remove(&region));
                if let Some(until) = old {
                    warn!(%region, wait_s = OLD_WAIT.as_secs(), "the router there doesn't run requests for others yet: deployments run there start workers there from here");
                    tokio::select! {
                        () = tokio::time::sleep_until(until.into()) => continue,
                        _ = stop.changed() => return,
                    }
                }
                warn!(%region, "link down: requests for deployments run there start workers there from here until it's back");
            }
            Err(e) => {
                if !said_down {
                    warn!(%region, %addr, error = %e, "can't open the link: requests for deployments run there start workers there from here until it answers");
                }
                said_down = true;
            }
        }
        tokio::select! {
            () = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// The worker id the link to `region` is leased under.
pub fn id(region: &str) -> String {
    format!("{ID_PREFIX}{region}")
}

/// The router at the other end of link `id` doesn't run requests for others: the link closes,
/// and opens again after `OLD_WAIT`.
pub fn old(id: &str) {
    if let Some(region) = id.strip_prefix(ID_PREFIX) {
        OLD.get_or_init(Mutex::default).lock().unwrap().insert(region.to_owned(), Instant::now() + OLD_WAIT);
    }
}

/// Make `request` one for the router at the other end of the link: the deployment to run it on,
/// and what this router knows about it that the other's routing table doesn't say.
pub fn mark(request: &mut Request<ProxyBody>, proofs: &Proofs, deployment: &str, max_duration: Option<Duration>, alone: bool) {
    let headers = request.headers_mut();
    // The client's own, if it sent any of these, never get there.
    for name in [ALONE_HEADER, MAX_MS_HEADER, RAN_HEADER, REFUSED_HEADER, ANSWER_HEADER] {
        headers.remove(name);
    }
    headers.insert(crate::cache::peers::AUTH_HEADER, proofs.ours.clone());
    if let Ok(value) = HeaderValue::from_str(deployment) {
        headers.insert(RUN_HEADER, value);
    }
    if alone {
        headers.insert(ALONE_HEADER, HeaderValue::from_static("1"));
    }
    if let Some(max) = max_duration {
        headers.insert(MAX_MS_HEADER, HeaderValue::from(max.as_millis() as u64));
    }
}

/// Whether the answer came from the router the link was opened with.
pub fn proven(proofs: &Proofs, headers: &HeaderMap) -> bool {
    crate::cache::peers::proven(headers, &proofs.theirs)
}

/// Why the router at the other end didn't run it, if it didn't: what it said (`elsewhere`,
/// `unknown`, `draining`), or `old` for a router that doesn't run requests for others.
pub fn refused(headers: &HeaderMap) -> Option<String> {
    if !headers.contains_key(ANSWER_HEADER) {
        return Some("old".to_owned());
    }
    headers.get(REFUSED_HEADER).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}

/// Take off what's only for the asking router, once it's read: the link's proof, and the marks.
pub fn take_answer(headers: &mut HeaderMap) {
    headers.remove(crate::cache::peers::AUTH_HEADER);
    headers.remove(ANSWER_HEADER);
    headers.remove(REFUSED_HEADER);
}

/// What the request asked of the router that runs it: the deployment, whether it runs alone,
/// and its function's own maxDuration. The link's headers are taken off.
pub fn take_run(headers: &mut HeaderMap) -> (Option<String>, bool, Option<Duration>) {
    headers.remove(crate::cache::peers::AUTH_HEADER);
    let deployment = headers.remove(RUN_HEADER).and_then(|v| v.to_str().ok().map(str::to_owned)).filter(|d| !d.is_empty());
    let alone = headers.remove(ALONE_HEADER).is_some();
    let max = headers.remove(MAX_MS_HEADER).and_then(|v| v.to_str().ok()?.parse().ok()).map(Duration::from_millis);
    headers.remove(RAN_HEADER);
    headers.remove(REFUSED_HEADER);
    headers.remove(ANSWER_HEADER);
    (deployment, alone, max)
}

/// The answer that says it wasn't run here, and why.
pub fn refusal(why: &'static str) -> Response<ResponseBody> {
    let mut response = Response::new(http_body_util::Empty::<Bytes>::new().map_err(|never| match never {}).boxed());
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response.headers_mut().insert(REFUSED_HEADER, HeaderValue::from_static(why));
    response
}

/// Take off a worker's answer what only routers may say to each other: an app that answered
/// `x-forkrail-run-refused` would have the asking router drop its link to this region and run the
/// request again, though it ran here.
pub fn take_apps(headers: &mut HeaderMap) {
    for name in [RUN_HEADER, ALONE_HEADER, MAX_MS_HEADER, RAN_HEADER, REFUSED_HEADER, ANSWER_HEADER, crate::cache::peers::AUTH_HEADER] {
        headers.remove(name);
    }
}

/// What the running router says about how it ran it.
pub struct Ran {
    pub start: &'static str,
    pub queue: Duration,
    pub mem_kb: Option<u64>,
    pub worker: String,
}

/// On the running router's answer: how it ran it.
pub fn ran(headers: &mut HeaderMap, ran: &Ran) {
    let mem = ran.mem_kb.map(|kb| kb.to_string()).unwrap_or_default();
    if let Ok(value) = HeaderValue::from_str(&format!("{};{};{};{}", ran.start, ran.queue.as_millis(), mem, ran.worker)) {
        headers.insert(RAN_HEADER, value);
    }
}

/// On the asking router: how the other ran it, taken off the answer.
pub fn take_ran(headers: &mut HeaderMap) -> Option<Ran> {
    let value = headers.remove(RAN_HEADER)?;
    let mut parts = value.to_str().ok()?.splitn(4, ';');
    let start = match parts.next()? {
        "warm" => "warm",
        "thaw" => "thaw",
        "cold" => "cold",
        _ => "relay",
    };
    let queue = Duration::from_millis(parts.next()?.parse().ok()?);
    let mem_kb = parts.next()?.parse().ok();
    let worker = parts.next()?.to_owned();
    Some(Ran { start, queue, mem_kb, worker })
}

/// Requests this router is running for others now.
pub fn running() -> usize {
    RUNNING.load(Ordering::Relaxed)
}

/// An answer to another router, marked as this one's (`ANSWER_HEADER`) and counted in `running`
/// until its body has gone.
pub fn counted(mut response: Response<ResponseBody>) -> Response<ResponseBody> {
    response.headers_mut().insert(ANSWER_HEADER, HeaderValue::from_static("1"));
    RUNNING.fetch_add(1, Ordering::Relaxed);
    response.map(|inner| Counted { inner }.boxed())
}

struct Counted {
    inner: ResponseBody,
}

impl Drop for Counted {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Body for Counted {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
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
    use super::*;

    #[test]
    fn ran_round_trips() {
        let mut headers = HeaderMap::new();
        ran(&mut headers, &Ran { start: "thaw", queue: Duration::from_millis(17), mem_kb: Some(2048), worker: "abc.12ef".into() });
        let back = take_ran(&mut headers).unwrap();
        assert_eq!((back.start, back.queue, back.mem_kb, back.worker.as_str()), ("thaw", Duration::from_millis(17), Some(2048), "abc.12ef"));
        assert!(headers.is_empty(), "taken off");
        ran(&mut headers, &Ran { start: "warm", queue: Duration::ZERO, mem_kb: None, worker: "w;x".into() });
        let back = take_ran(&mut headers).unwrap();
        assert_eq!((back.start, back.mem_kb, back.worker.as_str()), ("warm", None, "w;x"));
        // Anything else is still a relayed request, whatever it says.
        headers.insert(RAN_HEADER, HeaderValue::from_static("odd;5;;w"));
        assert_eq!(take_ran(&mut headers).unwrap().start, "relay");
        headers.insert(RAN_HEADER, HeaderValue::from_static("garbled"));
        assert!(take_ran(&mut headers).is_none());
    }

    #[test]
    fn marked_and_taken() {
        let proofs = Proofs { ours: HeaderValue::from_static("ab12"), theirs: "cd34".into() };
        let mut request = Request::new(http_body_util::Empty::<Bytes>::new().map_err(|never| match never {}).boxed());
        // A client's own copies don't get through.
        request.headers_mut().insert(ALONE_HEADER, HeaderValue::from_static("1"));
        request.headers_mut().insert(RAN_HEADER, HeaderValue::from_static("warm;0;;x"));
        mark(&mut request, &proofs, "dpl1", Some(Duration::from_secs(60)), false);
        let headers = request.headers_mut();
        assert_eq!(headers[crate::cache::peers::AUTH_HEADER], "ab12");
        assert!(!headers.contains_key(RAN_HEADER));
        let (deployment, alone, max) = take_run(headers);
        assert_eq!((deployment.as_deref(), alone, max), (Some("dpl1"), false, Some(Duration::from_secs(60))));
        assert!(headers.is_empty(), "nothing of the link's reaches the worker: {headers:?}");
        mark(&mut request, &proofs, "dpl1", None, true);
        assert_eq!(take_run(request.headers_mut()), (Some("dpl1".to_owned()), true, None));
    }

    #[test]
    fn answers_proven_and_refused() {
        let proofs = Proofs { ours: HeaderValue::from_static("ab12"), theirs: "cd34".into() };
        let mut headers = HeaderMap::new();
        headers.insert(crate::cache::peers::AUTH_HEADER, HeaderValue::from_static("cd34"));
        assert!(proven(&proofs, &headers));
        headers.insert(crate::cache::peers::AUTH_HEADER, HeaderValue::from_static("cd35"));
        assert!(!proven(&proofs, &headers));
        assert!(!proven(&proofs, &HeaderMap::new()));
        let mut refusal = counted(refusal("draining"));
        refusal.headers_mut().insert(crate::cache::peers::AUTH_HEADER, HeaderValue::from_static("cd34"));
        assert_eq!(refused(refusal.headers()).as_deref(), Some("draining"));
        assert_eq!(running(), 1);
        take_answer(refusal.headers_mut());
        assert!(refusal.headers().is_empty());
        drop(refusal);
        assert_eq!(running(), 0);
        // An answer from a router that runs requests for others, and one from before that did.
        let ran = counted(Response::new(http_body_util::Empty::<Bytes>::new().map_err(|never| match never {}).boxed()));
        assert_eq!(refused(ran.headers()), None);
        assert_eq!(refused(&HeaderMap::new()).as_deref(), Some("old"));
    }
}
