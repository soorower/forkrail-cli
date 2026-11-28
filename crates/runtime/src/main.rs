//! Lambda worker runtime. Each invocation dials the router, then serves HTTP/2
//! requests the router sends over that connection until the router retires it.
//! A warm invocation (`{"warm": true}`) only makes sure the app has started and
//! returns, leaving the environment frozen for the first worker that needs it.
//! Requests go to the user's app, Node or Python (see `app.rs` for its configuration), or
//! to the built-in echo app when none is configured.

mod cpu;
mod echo;
mod egress;
mod logs;
mod mem;
mod net;
mod nextcache;
mod ownhosts;
mod purge;
mod app;
mod tls;
mod userenv;
mod worker;

use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use forkrail_common::{EndReason, InvokePayload, LOCAL_PAYLOAD_ENV, WARM_HOLD_MAX_MS, WarmReport, WarmRequest, random_hex};
use lambda_runtime::{Error, LambdaEvent, service_fn};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::app::Supervisor;
use crate::worker::App;

/// Lambda's maximum function timeout, used as the deadline in local mode.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(900);

static ENV_ID: OnceLock<String> = OnceLock::new();
static INVOCATIONS: AtomicU64 = AtomicU64::new(0);

/// Random id for this execution environment. It survives freeze/thaw, so a
/// repeated id across invocations means Lambda reused a warm environment.
pub fn env_id() -> &'static str {
    ENV_ID.get_or_init(|| random_hex(6))
}

fn next_invocation() -> u64 {
    INVOCATIONS.fetch_add(1, Ordering::Relaxed) + 1
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .without_time()
        .init();

    // Before anything else: nothing of this process may be read by the app, which runs as the same
    // user. Not dumpable, its /proc files (`environ`, with FORKRAIL_ENV_KEY and Lambda's keys;
    // `mem`, with the worker's token) belong to root. The app's own process, started by exec, is
    // dumpable again (its memory figures keep coming from /proc).
    #[cfg(target_os = "linux")]
    // SAFETY: a plain syscall on this process.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        warn!(error = %std::io::Error::last_os_error(), "couldn't make the runtime undumpable");
    }
    // The deployment's sealed env file, when its variables didn't fit Lambda's environment.
    match userenv::load() {
        Ok(0) => {}
        Ok(count) => info!(count, "env vars from the deployment's sealed file"),
        Err(e) => {
            error!(error = %e, "the deployment's env vars can't be read: not starting its app without them");
            return Err(e.into());
        }
    }
    // Before the app starts, which is told where the listeners are.
    egress::start();
    purge::start();
    nextcache::start();
    let supervisor = app::Config::from_env().map(|config| Arc::new(Supervisor::new(config)));
    if let Some(supervisor) = &supervisor {
        // Start the app during init, so the first invocation finds it ready. If it fails,
        // the invocation tries again and returns the error to the router.
        if let Err(e) = supervisor.ready().await {
            warn!(error = %e, "app failed to start");
        }
    }

    if let Ok(raw) = std::env::var(LOCAL_PAYLOAD_ENV) {
        if let Some(request) = warm_request(&serde_json::from_str(&raw)?) {
            println!("{}", serde_json::to_string(&warm(&supervisor, &request).await)?);
            return Ok(());
        }
        let payload: InvokePayload = serde_json::from_str(&raw)?;
        let app = current_app(&supervisor).await?;
        let report = worker::run(payload, SystemTime::now() + LOCAL_TIMEOUT, app).await?;
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }

    lambda_runtime::run(service_fn(|event: LambdaEvent<serde_json::Value>| {
        let supervisor = supervisor.clone();
        async move {
            if let Some(request) = warm_request(&event.payload) {
                return Ok(serde_json::to_value(warm(&supervisor, &request).await)?);
            }
            let deadline = event.context.deadline();
            let payload: InvokePayload = serde_json::from_value(event.payload)?;
            let report = worker::run(payload, deadline, current_app(&supervisor).await?).await?;
            if let (EndReason::AppExited, Some(supervisor)) = (report.end_reason, &supervisor) {
                // Restart now rather than in the next invocation, which then starts warm.
                if let Err(e) = supervisor.ready().await {
                    warn!(error = %e, "app failed to restart");
                }
            }
            Ok::<_, Error>(serde_json::to_value(report)?)
        }
    }))
    .await
}

/// The payload as a warm request, if it is one.
fn warm_request(payload: &serde_json::Value) -> Option<WarmRequest> {
    if payload.get("warm").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    serde_json::from_value(payload.clone()).ok()
}

/// A warm invocation: the app is started if it isn't running (init started it in a new
/// environment), nothing is sent to it, and the invocation stays a moment so that others sent
/// with it land in environments of their own.
async fn warm(supervisor: &Option<Arc<Supervisor>>, request: &WarmRequest) -> WarmReport {
    let env_invocations = next_invocation();
    let (ok, app_start_ms, error) = match supervisor {
        None => (true, 0, None),
        Some(supervisor) => match supervisor.ready().await {
            Ok(app) => (true, app.take_start_ms(), None),
            Err(e) => (false, 0, Some(e.to_string())),
        },
    };
    tokio::time::sleep(Duration::from_millis(request.hold_ms.min(WARM_HOLD_MAX_MS))).await;
    WarmReport { warm: true, ok, env_id: env_id().to_owned(), env_invocations, app_start_ms, error }
}

async fn current_app(supervisor: &Option<Arc<Supervisor>>) -> Result<App, Error> {
    Ok(match supervisor {
        Some(supervisor) => App::User(supervisor.ready().await?),
        None => App::Echo,
    })
}
