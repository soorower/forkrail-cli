//! What the router sees that a team wants to hear about, sent to the install's events queue
//! (`FORKRAIL_EVENTS_QUEUE`, deploy/install/notify.yaml): an attack the system mitigations are
//! refusing (`attack.detected`, shield.rs, at most every 10 minutes) and a custom domain's
//! certificate failing to renew (`domain.certificate.renew.failed`, certs.rs, the issuer only, once
//! as it starts failing). The notifier (deploy/notify) posts them to the team's webhooks and tells
//! its owners.
//!
//! Each is one SQS message, sent off the request path; a send that fails is logged and dropped.
//! Without the queue set, nothing is sent.

use std::sync::OnceLock;

use serde_json::{Value, json};
use tracing::{info, warn};

pub const ATTACK_DETECTED: &str = "attack.detected";
pub const CERTIFICATE_RENEW_FAILED: &str = "domain.certificate.renew.failed";

struct Queue {
    client: aws_sdk_sqs::Client,
    url: String,
    region: String,
}

static QUEUE: OnceLock<Queue> = OnceLock::new();
/// Control's queue for spend pauses (`FORKRAIL_SPEND_QUEUE`, deploy/install/control.yaml): it
/// records each, and sets the paused function's reserved concurrency to 0.
static SPEND_QUEUE: OnceLock<Queue> = OnceLock::new();

/// Send spend pauses to control's queue at `url` from now on.
pub async fn start_spend(url: String) {
    let region = url.strip_prefix("https://sqs.").and_then(|rest| rest.split('.').next()).unwrap_or_default().to_owned();
    let client = crate::schedules::sqs_client(&url).await;
    info!(queue = %url, "spend pauses go to control's queue");
    let _ = SPEND_QUEUE.set(Queue { client, url, region });
}

/// Send events to the queue at `url` from now on.
pub async fn start(url: String) {
    let region = url.strip_prefix("https://sqs.").and_then(|rest| rest.split('.').next()).unwrap_or_default().to_owned();
    let client = crate::schedules::sqs_client(&url).await;
    info!(queue = %url, "events go to the queue");
    let _ = QUEUE.set(Queue { client, url, region });
}

/// An event's whole body, as forkrail's other events are (crates/cli/src/events.rs). `id` None: a
/// new one.
fn envelope(kind: &str, id: Option<&str>, region: &str, payload: Value) -> Value {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    json!({
        "id": id.map_or_else(|| format!("evt_{}", forkrail_common::random_hex(12)), str::to_owned),
        "type": kind,
        "createdAt": now,
        "payload": payload,
        "region": region,
    })
}

/// The id of the event that says `what`: the same on every router and after a restart, so the
/// notifier, which handles each id once, tells the team once (both regions see a budget reached).
pub fn id_of(what: &str) -> String {
    use sha2::Digest;
    format!("evt_{}", &forkrail_common::hex(&sha2::Sha256::digest(what.as_bytes()))[..24])
}

/// Send one, in the background (a spend pause to control's queue too).
pub fn send(kind: &'static str, payload: Value) {
    send_as(kind, None, payload);
}

/// Send one with its id (`id_of`).
pub fn send_as(kind: &'static str, id: Option<String>, payload: Value) {
    let Some(queue) = QUEUE.get() else { return };
    let body = envelope(kind, id.as_deref(), &queue.region, payload).to_string();
    tokio::spawn(async move {
        if let Err(e) = queue.client.send_message().queue_url(&queue.url).message_body(body).send().await {
            warn!(event = kind, error = %aws_sdk_sqs::error::DisplayErrorContext(&e), "sending an event failed");
        }
    });
}

/// Tell control (`spend.paused` for one project, `spend.month_end`), in the background.
pub fn to_control(kind: &'static str, payload: Value) {
    let Some(queue) = SPEND_QUEUE.get() else { return };
    let body = envelope(kind, None, &queue.region, payload).to_string();
    tokio::spawn(async move {
        if let Err(e) = queue.client.send_message().queue_url(&queue.url).message_body(body).send().await {
            warn!(event = kind, error = %aws_sdk_sqs::error::DisplayErrorContext(&e), "telling control of a pause failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_has_the_shape_of_the_others() {
        let e = envelope(ATTACK_DETECTED, None, "us-east-1", json!({"refused": 12000}));
        assert_eq!(e["type"], "attack.detected");
        assert_eq!(e["region"], "us-east-1");
        assert_eq!(e["payload"]["refused"], 12000);
        assert!(e["id"].as_str().unwrap().starts_with("evt_") && e["createdAt"].as_u64().unwrap() > 1_700_000_000_000);
        // Unset, nothing is sent and nothing fails.
        send(CERTIFICATE_RENEW_FAILED, json!({}));
        let id = id_of("budget.reached|project|a|202611|50|5000000000");
        assert_eq!(id, id_of("budget.reached|project|a|202611|50|5000000000"), "every router names it alike");
        assert_ne!(id, id_of("budget.reached|project|a|202611|75|5000000000"));
        assert_eq!(envelope("budget.reached", Some(&id), "us-west-1", json!({}))["id"], id.as_str());
    }
}
