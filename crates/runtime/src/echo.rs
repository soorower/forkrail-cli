//! Built-in test app, used when no Node app is configured. It tests the tunnel and the
//! router without Node in the way.
//!
//! - `GET /sleep?ms=200` waits, then answers, to simulate an I/O-bound request.
//! - `GET /alloc?mb=300&ms=1000` holds 300 MB of resident memory for a second, to test memory limits.
//! - `GET /crash?ms=500` waits, then kills the whole runtime, as an out-of-memory kill would.
//! - `GET /stream?chunks=10&ms=100&crash=0` sends a line every `ms`. With `crash=N`, it kills the
//!   runtime after N lines, to test failures mid-response.
//! - `GET /bytes?mb=5` answers with 5 MB, to test slow clients.
//! - Any other GET or HEAD answers with a JSON summary of who served it, including memory.
//! - Other methods stream the request body straight back.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::channel::Channel;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::CONTENT_TYPE;
use hyper::{Method, Request, Response};

use crate::{env_id, mem};

const MAX_SLEEP: Duration = Duration::from_secs(60);
const MAX_ALLOC_MB: u64 = 10_240;

/// What the JSON summary reports about the worker.
pub struct Context<'a> {
    pub worker_id: &'a str,
    pub request_no: u64,
    pub inflight: usize,
}

pub async fn handle(req: Request<Incoming>, ctx: &Context<'_>) -> Response<BoxBody<Bytes, hyper::Error>> {
    let query = req.uri().query();
    let wait = Duration::from_millis(query_num(query, "ms")).min(MAX_SLEEP);
    // Dropped after the response is built, so the summary shows memory while it's held.
    let mut _held = Vec::new();
    let mut body = None;
    match req.uri().path() {
        "/sleep" => tokio::time::sleep(wait).await,
        "/alloc" => {
            // A non-zero fill touches every page, so the memory is really resident.
            _held = vec![1u8; (query_num(query, "mb").min(MAX_ALLOC_MB) as usize) << 20];
            tokio::time::sleep(wait).await;
        }
        "/crash" => {
            tokio::time::sleep(wait).await;
            std::process::abort();
        }
        "/stream" => body = Some(stream(query_num(query, "chunks"), wait, query_num(query, "crash"))),
        "/bytes" => {
            let len = (query_num(query, "mb").min(MAX_ALLOC_MB) as usize) << 20;
            body = Some(Full::new(Bytes::from(vec![b'x'; len])).map_err(|never| match never {}).boxed());
        }
        _ => {}
    }
    let memory = mem::snapshot();

    let response = Response::builder();
    let response = if let Some(body) = body {
        response.header(CONTENT_TYPE, "text/plain").body(body)
    } else if matches!(*req.method(), Method::GET | Method::HEAD) {
        let summary = serde_json::json!({
            "worker": ctx.worker_id,
            "env": env_id(),
            "request_no": ctx.request_no,
            "inflight": ctx.inflight,
            "method": req.method().as_str(),
            "path": req.uri().path(),
            "request_id": req.headers().get("x-forkrail-request-id").and_then(|v| v.to_str().ok()),
            "rss_mb": memory.rss_kb / 1024,
            "mem_total_mb": memory.total_kb / 1024,
            "mem_available_mb": memory.available_kb / 1024,
            "cgroup_limit_mb": memory.cgroup_limit_kb / 1024,
            "cgroup_used_mb": memory.cgroup_used_kb / 1024,
            "configured_mb": memory.configured_kb / 1024,
        });
        let body = Full::new(Bytes::from(summary.to_string())).map_err(|never| match never {}).boxed();
        response.header(CONTENT_TYPE, "application/json").body(body)
    } else {
        let content_type = req.headers().get(CONTENT_TYPE).cloned();
        let response = match content_type {
            Some(value) => response.header(CONTENT_TYPE, value),
            None => response,
        };
        response.body(req.into_body().boxed())
    };
    response.expect("static headers are valid")
}

/// A line every `wait`. With `crash_after` above zero, the runtime dies after that many lines.
fn stream(lines: u64, wait: Duration, crash_after: u64) -> BoxBody<Bytes, hyper::Error> {
    let (mut tx, body) = Channel::<Bytes, hyper::Error>::new(1);
    tokio::spawn(async move {
        for line in 1..=lines {
            if crash_after > 0 && line > crash_after {
                std::process::abort();
            }
            if tx.send_data(Bytes::from(format!("line {line}\n"))).await.is_err() {
                return;
            }
            tokio::time::sleep(wait).await;
        }
    });
    body.boxed()
}

fn query_num(query: Option<&str>, key: &str) -> u64 {
    query
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}
