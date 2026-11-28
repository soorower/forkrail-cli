//! Purges by tag from the app (`@vercel/functions`' `invalidateByTag`, `dangerouslyDeleteByTag`
//! and their by-source-image forms, which Astro's `cache.invalidate` calls), sent to the router as
//! this invocation's worker, so they purge the deployment's own project and environment only.
//!
//! The shim (`shim.mjs`) puts `purge` in Vercel's request context. Each call connects to a
//! loopback port the runtime listens on (`FORKRAIL_PURGE`, `127.0.0.1:<port>:<secret>`, which the
//! shim takes out of the app's environment), sends one line of JSON with the secret, and waits for
//! one back. The runtime dials the router's tunnel port as the live worker with its token
//! (`FORKRAIL-PURGE/1`), passes the purge on and the router's answer back. Between invocations
//! Lambda freezes the environment, so a call only runs while an invocation does.

use std::sync::OnceLock;
use std::time::Duration;

use forkrail_common::purge::{MAX_PURGE_LINE, PurgeReply, RuntimePurge};
use forkrail_common::{PurgeHello, read_json_line, send_purge};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::{egress, tls};

/// What the app is told: `FORKRAIL_PURGE` → `127.0.0.1:<port>:<secret>`.
static APP_ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();
static SECRET: OnceLock<String> = OnceLock::new();
/// A purge waits this long for the router (which may ask home, across regions).
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
struct Asked {
    secret: String,
    #[serde(flatten)]
    purge: RuntimePurge,
}

pub fn app_env() -> &'static [(String, String)] {
    APP_ENV.get().map_or(&[], Vec::as_slice)
}

/// Listen for the shim's purges. Called once, before the app starts.
pub fn start() {
    let listener = match std::net::TcpListener::bind(("127.0.0.1", 0)).and_then(|l| l.set_nonblocking(true).map(|()| l)).and_then(TcpListener::from_std) {
        Ok(listener) => listener,
        Err(e) => {
            warn!(error = %e, "purge listener failed: the app's purges do nothing");
            return;
        }
    };
    let Ok(addr) = listener.local_addr() else { return };
    let secret = forkrail_common::random_hex(16);
    let _ = SECRET.set(secret.clone());
    let _ = APP_ENV.set(vec![("FORKRAIL_PURGE".to_owned(), format!("{addr}:{secret}"))]);
    tokio::spawn(accept(listener));
}

async fn accept(listener: TcpListener) {
    loop {
        let (app, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!(error = %e, "purge accept failed");
                continue;
            }
        };
        tokio::spawn(async move {
            if let Err(e) = handle(app).await {
                warn!(error = %e, "purge connection failed");
            }
        });
    }
}

async fn handle(mut app: TcpStream) -> std::io::Result<()> {
    let line = read_json_line(&mut app, MAX_PURGE_LINE + 256).await?;
    let reply = match serde_json::from_slice::<Asked>(&line) {
        Ok(asked) if same(&asked.secret, SECRET.get().map_or("", String::as_str)) => {
            let (op, tags) = (asked.purge.op, asked.purge.tags.len());
            let started = std::time::Instant::now();
            match tokio::time::timeout(TIMEOUT, forward(&asked.purge)).await {
                Ok(Ok(reply)) => {
                    info!(op = op.as_str(), tags, ok = reply.ok, ms = started.elapsed().as_millis() as u64, "purge");
                    reply
                }
                Ok(Err(e)) => PurgeReply { error: Some(e), ..PurgeReply::default() },
                Err(_) => PurgeReply { error: Some("the router didn't answer the purge in time".into()), ..PurgeReply::default() },
            }
        }
        Ok(_) => PurgeReply { error: Some("not this environment's purge".into()), ..PurgeReply::default() },
        Err(e) => PurgeReply { error: Some(format!("not a purge: {e}")), ..PurgeReply::default() },
    };
    let mut out = serde_json::to_vec(&reply).expect("plain fields");
    out.push(b'\n');
    app.write_all(&out).await?;
    app.shutdown().await
}

/// The purge to the router, as the invocation's worker, and the router's answer.
async fn forward(purge: &RuntimePurge) -> Result<PurgeReply, String> {
    let session = egress::session().ok_or("no invocation is running")?;
    let tcp = TcpStream::connect(&session.router_addr).await.map_err(|e| format!("can't reach the router: {e}"))?;
    let _ = tcp.set_nodelay(true);
    let mut router = tls::connect(&session.cert_sha256, tcp).await.map_err(|e| format!("can't reach the router: {e}"))?;
    send_purge(&mut router, &PurgeHello { worker_id: session.worker_id, token: session.token }).await.map_err(|e| e.to_string())?;
    let mut line = serde_json::to_vec(purge).expect("plain fields");
    line.push(b'\n');
    router.write_all(&line).await.map_err(|e| e.to_string())?;
    router.flush().await.map_err(|e| e.to_string())?;
    let answer = read_json_line(&mut router, 64 << 10).await.map_err(|e| format!("the router's answer: {e}"))?;
    serde_json::from_slice(&answer).map_err(|e| format!("the router's answer: {e}"))
}

fn same(a: &str, b: &str) -> bool {
    !b.is_empty() && a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abd", "abc") && !same("ab", "abc") && !same("", ""));
    }

    #[test]
    fn the_shims_line_reads_as_a_purge() {
        let asked: Asked = serde_json::from_str(r#"{"secret":"s","op":"delete","tags":["a","b"],"deadline_s":60}"#).unwrap();
        assert_eq!(asked.secret, "s");
        assert_eq!(asked.purge.tags, ["a", "b"]);
        assert_eq!(asked.purge.deadline_s, Some(60));
        let asked: Asked = serde_json::from_str(r#"{"secret":"s","op":"invalidate","tags":["/a.png"],"src_images":true}"#).unwrap();
        assert!(asked.purge.src_images);
    }
}
