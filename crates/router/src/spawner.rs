use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aws_sdk_lambda::error::DisplayErrorContext;
use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::InvocationType;
use forkrail_common::{InvokePayload, LOCAL_PAYLOAD_ENV, WarmReport, WarmRequest, WorkerReport};
use tokio::process::Command;
use tracing::{info, warn};

/// Longer than Lambda's 15-minute maximum, because a worker's invoke call stays open its whole life.
pub const INVOKE_TIMEOUT: Duration = Duration::from_secs(16 * 60);
/// A warm invocation answers within an environment's start (10 s at most) and its app's (9 s).
const WARM_TIMEOUT: Duration = Duration::from_secs(20);

pub enum Spawner {
    /// `qualifier` is the version a deployment runs; `None` means `$LATEST`. The client is shared
    /// by every pool.
    Lambda { client: aws_sdk_lambda::Client, function: String, qualifier: Option<String> },
    /// Runs the runtime binary as a child process per worker, for local testing. `env` is set on
    /// top of the router's own; with `isolated`, the app the router's environment names
    /// (`FORKRAIL_APP_DIR`, `FORKRAIL_APP_ENTRY`) is cleared first, so a deployment without
    /// them runs the echo app.
    Local { bin: PathBuf, env: Vec<(String, String)>, isolated: bool },
}

/// Lambda clients: the router's own region's, and one for each other region it has had to invoke
/// in, made on first use from the same settings (timeouts, no retries, credentials).
#[derive(Clone)]
pub struct Lambdas {
    config: aws_config::SdkConfig,
    /// The router's region: deployments run here when they have a version here.
    pub here: String,
    /// The region of deployments whose table entry names none (`FORKRAIL_HOME_REGION`).
    pub home: String,
    clients: Arc<Mutex<HashMap<String, aws_sdk_lambda::Client>>>,
}

impl Lambdas {
    pub fn new(config: aws_config::SdkConfig, home: Option<String>) -> Self {
        let here = config.region().map(|r| r.to_string()).unwrap_or_default();
        let home = home.filter(|h| !h.is_empty()).unwrap_or_else(|| here.clone());
        let own = aws_sdk_lambda::Client::new(&config);
        Self { config, clients: Arc::new(Mutex::new(HashMap::from([(here.clone(), own)]))), here, home }
    }

    pub fn client(&self, region: &str) -> aws_sdk_lambda::Client {
        let mut clients = self.clients.lock().unwrap();
        clients
            .entry(region.to_owned())
            .or_insert_with(|| {
                let config = aws_sdk_lambda::config::Builder::from(&self.config)
                    .region(aws_config::Region::new(region.to_owned()))
                    .build();
                aws_sdk_lambda::Client::from_conf(config)
            })
            .clone()
    }
}

/// Variables that pick the app a local worker runs.
const LOCAL_APP_VARS: [&str; 4] = ["FORKRAIL_APP_DIR", "FORKRAIL_APP_ENTRY", "FORKRAIL_EGRESS", "FORKRAIL_EGRESS_TARGETS"];

/// Why a worker's invocation ended badly.
#[derive(Debug)]
pub struct InvokeFailure {
    /// Lambda refused to start it: the function's reserved concurrency or the account limit.
    pub throttled: bool,
    pub message: String,
}

impl InvokeFailure {
    fn other(message: impl ToString) -> Self {
        Self { throttled: false, message: message.to_string() }
    }
}

impl Spawner {
    /// Start a worker and wait until its invocation ends. Returns how long that took, in seconds,
    /// and the worker's report, if it sent one the router could read.
    pub async fn invoke(&self, payload: &InvokePayload) -> (f64, Result<Option<WorkerReport>, InvokeFailure>) {
        let started = Instant::now();
        let json = serde_json::to_string(payload).expect("payload serializes");
        let outcome = match self {
            Spawner::Lambda { client, function, qualifier } => {
                invoke_lambda(client, function, qualifier.as_deref(), json).await
            }
            Spawner::Local { bin, env, isolated } => run_local(bin, env, *isolated, json).await,
        };
        let secs = started.elapsed().as_secs_f64();
        match &outcome {
            Ok(report) => info!(worker = %payload.worker_id, secs, %report, "worker ended"),
            Err(e) => warn!(worker = %payload.worker_id, secs, throttled = e.throttled, error = %e.message, "worker failed"),
        }
        (secs, outcome.map(|report| serde_json::from_str(&report).ok()))
    }

    /// Send a warm invocation (`WarmRequest`): Lambda runs it in an environment that isn't busy,
    /// starting one if there's none, and the runtime answers once its app is up, without a request
    /// to the app or a tunnel to this router. The environment's idle clock starts again, which is
    /// what keeping it alive takes (`keepwarm.rs`). `hold_ms` keeps the invocation open that long,
    /// so several sent together land in environments of their own. `None` from a runtime that
    /// predates warm invocations: its environment was used all the same.
    pub async fn warm(&self, hold_ms: u64) -> Result<Option<WarmReport>, InvokeFailure> {
        let json = serde_json::to_string(&WarmRequest { warm: true, hold_ms }).expect("payload serializes");
        let sent = async {
            match self {
                Spawner::Lambda { client, function, qualifier } => invoke_lambda(client, function, qualifier.as_deref(), json).await,
                Spawner::Local { bin, env, isolated } => run_local(bin, env, *isolated, json).await,
            }
        };
        match tokio::time::timeout(WARM_TIMEOUT, sent).await {
            Err(_) => Err(InvokeFailure::other(format!("no answer in {} s", WARM_TIMEOUT.as_secs()))),
            // lambda_runtime's words for a payload the handler's type can't hold: the handler
            // never ran, and the environment is there.
            Ok(Err(e)) if !e.throttled && e.message.contains("deserialize") => Ok(None),
            Ok(Err(e)) => Err(e),
            Ok(Ok(body)) => match serde_json::from_str::<WarmReport>(&body) {
                Ok(report) if report.warm => Ok(Some(report)),
                _ => Err(InvokeFailure::other(format!("an answer that isn't a warm-up's: {}", body.chars().take(200).collect::<String>()))),
            },
        }
    }
}

async fn invoke_lambda(
    client: &aws_sdk_lambda::Client,
    function: &str,
    qualifier: Option<&str>,
    json: String,
) -> Result<String, InvokeFailure> {
    let out = client
        .invoke()
        .function_name(function)
        .set_qualifier(qualifier.map(str::to_owned))
        .invocation_type(InvocationType::RequestResponse)
        .payload(Blob::new(json))
        .send()
        .await
        .map_err(|e| InvokeFailure {
            throttled: e.as_service_error().is_some_and(|e| e.is_too_many_requests_exception()),
            message: DisplayErrorContext(&e).to_string(),
        })?;
    let body = out.payload().map(|b| String::from_utf8_lossy(b.as_ref()).into_owned()).unwrap_or_default();
    match out.function_error() {
        Some(kind) => Err(InvokeFailure::other(format!("{kind}: {body}"))),
        None => Ok(body),
    }
}

async fn run_local(bin: &PathBuf, env: &[(String, String)], isolated: bool, json: String) -> Result<String, InvokeFailure> {
    let mut command = Command::new(bin);
    if isolated {
        for var in LOCAL_APP_VARS {
            command.env_remove(var);
        }
    }
    // `output()` would pipe stderr too; spawn so the worker's logs reach our stderr.
    let child = command
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env(LOCAL_PAYLOAD_ENV, json)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(InvokeFailure::other)?;
    let out = child.wait_with_output().await.map_err(InvokeFailure::other)?;
    if !out.status.success() {
        return Err(InvokeFailure::other(format!("runtime exited with {}", out.status)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
