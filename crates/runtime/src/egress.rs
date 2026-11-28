//! Connections the app opens to private addresses through its router, such as the analytics box,
//! which admits only the routers. Lambda outside a VPC has no fixed address to admit.
//!
//! `FORKRAIL_EGRESS` names the targets the deployment may reach (`analytics`). For each, the
//! runtime listens on a loopback port and tells the app where with `FORKRAIL_EGRESS_<NAME>`
//! (`FORKRAIL_EGRESS_ANALYTICS=127.0.0.1:41234`). A connection there goes to the router over TLS,
//! as the current invocation's worker with its token; the router checks the deployment allows the
//! target, connects to its own address for it, and passes bytes both ways. The app's own protocol,
//! TLS included, goes through untouched.
//!
//! Connections only work while an invocation runs (between them Lambda freezes the environment),
//! and they end when the router retires the worker, so the app must expect a kept-alive
//! connection to be closed after a pause.

use std::sync::{Mutex, OnceLock};

use forkrail_common::{EgressHello, InvokePayload, send_egress};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::tls;

/// The invocation in progress, which egress connections go out as.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// What the app is told: `FORKRAIL_EGRESS_<NAME>` → `127.0.0.1:<port>`.
static APP_ENV: OnceLock<Vec<(String, String)>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) router_addr: String,
    pub(crate) cert_sha256: String,
    pub(crate) worker_id: String,
    pub(crate) token: String,
}

/// The invocation in progress, which the runtime's own connections to the router go out as
/// (`purge.rs`).
pub(crate) fn session() -> Option<Session> {
    SESSION.lock().unwrap().clone()
}

/// Marks an invocation's tunnel as up; egress connections go out as its worker until it drops.
pub struct Active(());

impl Active {
    pub fn begin(payload: &InvokePayload) -> Self {
        *SESSION.lock().unwrap() = Some(Session {
            router_addr: payload.router_addr.clone(),
            cert_sha256: payload.cert_sha256.clone(),
            worker_id: payload.worker_id.clone(),
            token: payload.token.clone(),
        });
        Active(())
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        *SESSION.lock().unwrap() = None;
    }
}

/// The variables to give the app.
pub fn app_env() -> &'static [(String, String)] {
    APP_ENV.get().map_or(&[], Vec::as_slice)
}

/// Listen for each target `FORKRAIL_EGRESS` names. Called once, before the app starts.
pub fn start() {
    let names = std::env::var("FORKRAIL_EGRESS").unwrap_or_default();
    let mut env = Vec::new();
    for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !forkrail_common::routes::valid_egress_name(name) {
            warn!(name, "egress target name not valid; skipped");
            continue;
        }
        let listener = match std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.set_nonblocking(true).map(|()| l))
            .and_then(TcpListener::from_std)
        {
            Ok(listener) => listener,
            Err(e) => {
                warn!(name, error = %e, "egress listener failed");
                continue;
            }
        };
        let addr = match listener.local_addr() {
            Ok(addr) => addr,
            Err(e) => {
                warn!(name, error = %e, "egress listener failed");
                continue;
            }
        };
        env.push((variable(name), addr.to_string()));
        tokio::spawn(accept(listener, name.to_owned()));
    }
    if !env.is_empty() {
        info!(targets = %names, "egress listeners up");
    }
    let _ = APP_ENV.set(env);
}

/// `FORKRAIL_EGRESS_<NAME>`, with `-` as `_`.
fn variable(name: &str) -> String {
    format!("FORKRAIL_EGRESS_{}", name.to_ascii_uppercase().replace('-', "_"))
}

async fn accept(listener: TcpListener, target: String) {
    loop {
        let (app, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!(target, error = %e, "egress accept failed");
                continue;
            }
        };
        let target = target.clone();
        tokio::spawn(async move {
            if let Err(e) = forward(app, &target).await {
                warn!(target, error = %e, "egress connection failed");
            }
        });
    }
}

async fn forward(mut app: TcpStream, target: &str) -> std::io::Result<()> {
    let Some(session) = SESSION.lock().unwrap().clone() else {
        return Err(std::io::Error::other("no invocation running"));
    };
    app.set_nodelay(true)?;
    let tcp = TcpStream::connect(&session.router_addr).await?;
    tcp.set_nodelay(true)?;
    // What it carries is the router's to count, not the app's own egress (`net.rs`).
    let _counted = crate::net::to_router_socket(&tcp);
    let mut router = tls::connect(&session.cert_sha256, tcp).await?;
    send_egress(&mut router, &EgressHello { worker_id: session.worker_id, token: session.token, target: target.to_owned() }).await?;
    // Either side closing ends it, and the router closes it when the worker retires.
    let _ = tokio::io::copy_bidirectional(&mut app, &mut router).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_names() {
        assert_eq!(variable("analytics"), "FORKRAIL_EGRESS_ANALYTICS");
        assert_eq!(variable("main-db"), "FORKRAIL_EGRESS_MAIN_DB");
    }
}
