//! Listener that Lambda workers dial into: each invocation's tunnel, and the connections its app
//! opens to private addresses through the router (egress).
//!
//! Egress: a deployment names targets it may reach (`egress` in its table entry), and each router
//! maps names to addresses its own network reaches (`FORKRAIL_EGRESS_TARGETS`, such as
//! `analytics=172.31.9.153:8443`). The runtime dials in as a live worker of that deployment, with
//! its token, and names the target; the router connects and passes bytes both ways, untouched
//! (TLS to the target stays end to end). The connection ends when the worker goes.
//!
//! Purges: a function's `invalidateByTag` and the rest (`@vercel/functions`) reach the runtime,
//! which dials in as its live worker with its token: the purge is of that worker's project and
//! environment, and no other (`cache::ResponseCache::purge_from_function`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forkrail_common::purge::{MAX_PURGE_LINE, PurgeReply, RuntimePurge};
use forkrail_common::{EgressHello, Hello, Preface, PurgeHello, ack_preface, read_hello, read_json_line};
use tokio::io::AsyncWriteExt;
use hyper::client::conn::http2;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::BoxError;
use crate::cache::ResponseCache;
use crate::registry::Registry;
use crate::wire;

/// TLS plus preface must finish within this, or the connection is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections still before their preface, at most: the port is open to the internet, and
/// each one holds a file descriptor the public listeners need too. Past it, new ones are closed.
const HANDSHAKES: usize = 8192;
/// After an accept fails (out of descriptors, say), the next is tried this much later, not at once.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
/// Pings detect a worker that vanished without closing the connection.
const KEEP_ALIVE: Duration = Duration::from_secs(10);
/// An egress target must accept the connection within this.
const EGRESS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A purge's line must come within this, and be done within `PURGE_TIMEOUT` (home may be asked).
const PURGE_READ_TIMEOUT: Duration = Duration::from_secs(5);
const PURGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Egress target names and the addresses this router reaches them at.
pub type EgressTargets = Arc<HashMap<String, String>>;

/// `FORKRAIL_EGRESS_TARGETS`: comma-separated `name=host:port`.
pub fn parse_egress(spec: &str) -> Result<HashMap<String, String>, String> {
    let mut targets = HashMap::new();
    for item in spec.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let Some((name, addr)) = item.split_once('=') else { return Err(format!("{item:?}: expected name=host:port")) };
        let (name, addr) = (name.trim(), addr.trim());
        if !forkrail_common::routes::valid_egress_name(name) {
            return Err(format!("{name:?}: use 1-32 of a-z, 0-9 and -"));
        }
        if addr.rsplit_once(':').is_none_or(|(host, port)| host.is_empty() || port.parse::<u16>().is_err()) {
            return Err(format!("{addr:?}: expected host:port"));
        }
        targets.insert(name.to_owned(), addr.to_owned());
    }
    Ok(targets)
}

pub async fn serve(listener: TcpListener, tls: TlsAcceptor, registry: Arc<Registry>, egress: EgressTargets, cache: Option<Arc<ResponseCache>>) {
    let handshakes = Arc::new(tokio::sync::Semaphore::new(HANDSHAKES));
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!(error = %e, "tunnel accept failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        let Ok(handshake) = handshakes.clone().try_acquire_owned() else {
            debug!(%peer, "tunnel connection closed: too many handshakes under way");
            continue;
        };
        let (tls, registry, egress, cache) = (tls.clone(), registry.clone(), egress.clone(), cache.clone());
        tokio::spawn(async move {
            if let Err(e) = handle(tcp, tls, handshake, &registry, &egress, cache.as_deref()).await {
                warn!(%peer, error = %e, "tunnel connection failed");
            }
        });
    }
}

async fn handle(tcp: TcpStream, tls: TlsAcceptor, handshake: tokio::sync::OwnedSemaphorePermit, registry: &Registry, egress: &EgressTargets, cache: Option<&ResponseCache>) -> Result<(), BoxError> {
    let accepted_at = Instant::now();
    tcp.set_nodelay(true)?;
    let (stream, hello) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut stream = tls.accept(tcp).await?;
        let hello = read_hello(&mut stream).await?;
        Ok::<_, std::io::Error>((stream, hello))
    })
    .await??;
    drop(handshake);
    match hello {
        Hello::Worker(preface) => worker(stream, preface, accepted_at, registry).await,
        Hello::Egress(hello) => pipe(stream, hello, registry, egress).await,
        Hello::Purge(hello) => purge(stream, hello, registry, cache).await,
        // Next.js's cache from a worker whose tunnel is gone (`cache/next.rs`).
        Hello::Cache(hello) => match crate::cache::next::get() {
            Some(next) => next.dialed(stream, &hello.worker_id, &hello.token).await,
            None => Err(format!("cache connection from {}: this router keeps no cache", hello.worker_id).into()),
        },
    }
}

/// A purge from a worker's app: of its project and environment, answered with one line.
async fn purge(mut stream: TunnelStream, hello: PurgeHello, registry: &Registry, cache: Option<&ResponseCache>) -> Result<(), BoxError> {
    let PurgeHello { worker_id, token } = hello;
    let Some((project, environment)) = registry.worker_scope(&worker_id, &token) else {
        return Err(format!("purge from {worker_id}: unknown worker or bad token").into());
    };
    ack_preface(&mut stream).await?;
    let started = Instant::now();
    let line = tokio::time::timeout(PURGE_READ_TIMEOUT, read_json_line(&mut stream, MAX_PURGE_LINE)).await.map_err(|_| "purge: no request in time")??;
    let reply = match (serde_json::from_slice::<RuntimePurge>(&line), cache) {
        (Err(e), _) => PurgeReply { error: Some(format!("not a purge: {e}")), ..PurgeReply::default() },
        // No response cache on this router: nothing is kept, so nothing to purge.
        (Ok(_), None) => PurgeReply { ok: true, error: None },
        (Ok(asked), Some(cache)) => {
            let (op, tags) = (asked.op, asked.tags.len());
            match tokio::time::timeout(PURGE_TIMEOUT, cache.purge_from_function(&project, &environment, asked)).await {
                Ok(Ok(marks)) => {
                    info!(worker = %worker_id, %project, %environment, op = op.as_str(), tags, marks, ms = started.elapsed().as_millis() as u64, "purge");
                    PurgeReply { ok: true, error: None }
                }
                Ok(Err(e)) => PurgeReply { error: Some(e), ..PurgeReply::default() },
                Err(_) => PurgeReply { error: Some("the purge took too long".into()), ..PurgeReply::default() },
            }
        }
    };
    let mut out = serde_json::to_vec(&reply)?;
    out.push(b'\n');
    stream.write_all(&out).await?;
    stream.shutdown().await.ok();
    Ok(())
}

type TunnelStream = tokio_rustls::server::TlsStream<TcpStream>;

async fn worker(mut stream: TunnelStream, preface: Preface, accepted_at: Instant, registry: &Registry) -> Result<(), BoxError> {
    let worker_id = preface.worker_id.clone();
    // The worker id names the deployment, so the right pool checks the token.
    let Some((pool, since_spawn)) = registry.claim(&worker_id, &preface.token) else {
        return Err(format!("unknown worker {worker_id} or bad token").into());
    };
    ack_preface(&mut stream).await?;

    // hyper takes the stream and closes it; this handle is for the kernel's byte counts then.
    let socket = wire::hold(stream.get_ref().0);
    // And this one to end the connection at once when the project is paused: with other handles
    // open, dropping hyper's would send nothing.
    let kill = wire::hold(stream.get_ref().0);
    // Roles flip here: the worker dialed us, but we are the HTTP/2 client.
    let (sender, conn) = http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .keep_alive_interval(KEEP_ALIVE)
        .max_header_list_size(forkrail_common::TUNNEL_MAX_HEADER_LIST)
        .handshake(TokioIo::new(stream))
        .await?;
    // Weak, because holding the worker would keep its tunnel open after it's retired.
    let registered = pool.register(&preface, sender, socket);
    let stop = registered.stop.clone();
    let worker = Arc::downgrade(&registered);
    drop(registered);
    info!(
        worker = %worker_id,
        spawn_ms = since_spawn.as_millis() as u64,
        handshake_ms = accepted_at.elapsed().as_millis() as u64,
        limit_mb = preface.limit_kb / 1024,
        rss_mb = preface.rss_kb / 1024,
        "worker connected"
    );

    tokio::select! {
        ended = conn => if let Err(e) = ended {
            warn!(worker = %worker_id, error = %e, "tunnel error");
        },
        // Its project is paused (`budgets.rs`): the connection goes now, in flight or not, so the
        // worker's runtime returns its invocation.
        () = stop.notified() => {
            if let Some(kill) = kill {
                let _ = std::net::TcpStream::from(kill).shutdown(std::net::Shutdown::Both);
            }
            info!(worker = %worker_id, "tunnel closed: the project is paused");
        }
    }
    pool.tunnel_closed(&worker_id);
    // The router only closes tunnels with nothing in flight, so requests still
    // in flight mean the worker died.
    if let Some(worker) = worker.upgrade().filter(|w| w.inflight() > 0) {
        pool.worker_died(&worker);
    }
    pool.remove(&worker_id);
    info!(worker = %worker_id, "worker disconnected");
    Ok(())
}

/// An egress connection: check it, connect to the target, and pass bytes both ways until either
/// side closes or the worker goes.
async fn pipe(mut stream: TunnelStream, hello: EgressHello, registry: &Registry, egress: &EgressTargets) -> Result<(), BoxError> {
    let EgressHello { worker_id, token, target } = hello;
    let Some(addr) = egress.get(&target) else {
        return Err(format!("egress from {worker_id}: no target {target:?} on this router").into());
    };
    let Some((pool, mut gone)) = registry.egress(&worker_id, &token, &target) else {
        return Err(format!("egress from {worker_id} to {target}: unknown worker, bad token, or not allowed").into());
    };
    let started = Instant::now();
    let mut upstream = tokio::time::timeout(EGRESS_CONNECT_TIMEOUT, TcpStream::connect(addr.as_str()))
        .await
        .map_err(|_| format!("egress to {target} ({addr}): connect timed out"))?
        .map_err(|e| format!("egress to {target} ({addr}): {e}"))?;
    upstream.set_nodelay(true)?;
    ack_preface(&mut stream).await?;
    let (ended, bytes) = tokio::select! {
        copied = tokio::io::copy_bidirectional(&mut stream, &mut upstream) => match copied {
            Ok(bytes) => ("closed", Some(bytes)),
            Err(e) => {
                debug!(worker = %worker_id, %target, error = %e, "egress connection failed");
                ("failed", None)
            }
        },
        _ = gone.changed() => ("worker_gone", None),
    };
    // This connection is on the same billed hop as the worker's tunnel.
    pool.egress_closed(&worker_id, wire::bytes(stream.get_ref().0));
    info!(
        worker = %worker_id,
        target = %target,
        ms = started.elapsed().as_millis() as u64,
        bytes_up = bytes.map(|b| b.0),
        bytes_down = bytes.map(|b| b.1),
        ended,
        "egress"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egress_targets() {
        let t = parse_egress("analytics=172.31.9.153:8443, db=db.internal:5432").unwrap();
        assert_eq!(t["analytics"], "172.31.9.153:8443");
        assert_eq!(t["db"], "db.internal:5432");
        assert!(parse_egress("").unwrap().is_empty());
        for bad in ["analytics", "Analytics=1.2.3.4:1", "a=1.2.3.4", "a=:80", "a=host:http"] {
            assert!(parse_egress(bad).is_err(), "{bad}");
        }
    }
}
