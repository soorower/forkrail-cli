//! Events: what happens to deployments, sent to the install's events queue (`forkrail-events`,
//! deploy/install/notify.yaml). The notifier there (deploy/notify) posts each to the team's webhooks
//! that ask for it, and turns a failed deployment into a notification for whoever made it.
//!
//! The shape is Vercel's webhook body, so a receiver written for it needs only forkrail's signature
//! header: `{"id", "type", "createdAt", "payload", "region"}`, with `payload.deployment`,
//! `payload.links`, `payload.target` (`production` or null) and `payload.project`, plus forkrail's
//! own fields (`via`, `deployer`, `hook`, `error`).
//!
//! Sending is best effort and never holds up or fails what it's about: an install without the
//! queue (the notifier not installed), or credentials that may not send to it, are noted once and
//! then left alone for ten minutes. The CLI with an administrator's credentials sends too, so a
//! deploy from a laptop is heard like one from the dashboard.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::deploy::Ctx;
use crate::state::{DeploymentRecord, Settings, Target, now_ms};

pub const DEPLOYMENT_CREATED: &str = "deployment.created";
pub const DEPLOYMENT_SUCCEEDED: &str = "deployment.succeeded";
pub const DEPLOYMENT_ERROR: &str = "deployment.error";
pub const DEPLOYMENT_CANCELED: &str = "deployment.canceled";
pub const DEPLOYMENT_PROMOTED: &str = "deployment.promoted";
pub const DEPLOYMENT_ROLLBACK: &str = "deployment.rollback";

/// After the queue can't be reached, this long before trying again.
const QUIET_FOR: Duration = Duration::from_secs(600);
/// When the queue was last found missing or out of reach.
static UNREACHABLE: Mutex<Option<Instant>> = Mutex::new(None);

/// An event's whole body.
pub fn envelope(kind: &str, region: &str, payload: Value) -> Value {
    json!({
        "id": format!("evt_{}", forkrail_common::random_hex(12)),
        "type": kind,
        "createdAt": now_ms(),
        "payload": payload,
        "region": region,
    })
}

/// A deployment's event payload.
pub fn deployment_payload(rec: &DeploymentRecord, settings: &Settings) -> Value {
    let dashboard = settings.dashboard.as_deref().map(|d| d.trim().trim_end_matches('/').to_owned());
    let mut meta = serde_json::Map::new();
    if let Some(g) = &rec.git {
        meta.insert("githubRepo".into(), json!(g.repo));
        meta.insert("githubCommitRef".into(), json!(g.branch));
        meta.insert("githubCommitSha".into(), json!(g.full_sha.as_deref().unwrap_or(&g.sha)));
        meta.insert("githubCommitMessage".into(), json!(g.message));
        if let Some(author) = &g.author {
            meta.insert("githubCommitAuthorName".into(), json!(author));
        }
    }
    let mut regions: Vec<String> = rec.region.iter().cloned().collect();
    regions.extend(rec.replicas.keys().cloned());
    json!({
        "team": {"id": null},
        "user": {"id": null, "name": rec.deployer},
        "deployment": {
            "id": rec.id,
            "name": rec.project,
            "url": rec.hosts.first(),
            "meta": meta,
            "via": rec.via,
            "deployer": rec.deployer,
            "hook": rec.deploy_hook,
            "error": rec.error,
            "created": rec.created,
        },
        "links": {
            "deployment": dashboard.as_ref().map(|d| format!("{d}/projects/{}/deployments/{}", rec.project, rec.id)),
            "project": dashboard.as_ref().map(|d| format!("{d}/projects/{}", rec.project)),
        },
        "target": if rec.target == Target::Production { json!("production") } else { Value::Null },
        "project": {"id": rec.project, "name": rec.project},
        "regions": regions,
    })
}

/// The install's settings, for links: read at most once a minute, since a deploy sends several.
static SETTINGS: Mutex<Option<(Instant, Settings)>> = Mutex::new(None);
const SETTINGS_FOR: Duration = Duration::from_secs(60);

fn quiet() -> bool {
    UNREACHABLE.lock().unwrap().is_some_and(|since| since.elapsed() < QUIET_FOR)
}

/// Send a deployment's event.
pub async fn deployment(ctx: &Ctx, kind: &str, rec: &DeploymentRecord) {
    if quiet() {
        return;
    }
    let kept = SETTINGS.lock().unwrap().as_ref().filter(|(at, _)| at.elapsed() < SETTINGS_FOR).map(|(_, s)| s.clone());
    let settings = match kept {
        Some(s) => s,
        None => match ctx.store.settings().await {
            Ok(s) => {
                *SETTINGS.lock().unwrap() = Some((Instant::now(), s.clone()));
                s
            }
            Err(e) => {
                eprintln!("warning: event {kind} for {} not sent: {e:#}", rec.id);
                return;
            }
        },
    };
    emit(ctx, kind, deployment_payload(rec, &settings)).await;
}

/// Send an event with this payload to the install's queue: best effort.
pub async fn emit(ctx: &Ctx, kind: &str, payload: Value) {
    if quiet() {
        return;
    }
    let body = envelope(kind, &ctx.install.home_region, payload).to_string();
    let config = aws_sdk_sqs::config::Builder::from(&ctx.config).region(aws_config::Region::new(ctx.install.home_region.clone())).build();
    let sent = aws_sdk_sqs::Client::from_conf(config).send_message().queue_url(ctx.install.events_queue_url()).message_body(body).send().await;
    if let Err(e) = sent {
        let missing = e.as_service_error().is_some_and(|s| s.is_queue_does_not_exist()) || e.raw_response().is_some_and(|r| r.status().as_u16() == 403 || r.status().as_u16() == 400);
        if missing {
            if UNREACHABLE.lock().unwrap().replace(Instant::now()).is_none() {
                eprintln!("note: events aren't sent: the install's events queue isn't there or these credentials may not send to it (the notifier, deploy/aws/notify.sh)");
            }
        } else {
            eprintln!("warning: event {kind} not sent: {}", aws_sdk_sqs::error::DisplayErrorContext(&e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Failure, GitCommit, HookRef, Status, Via};

    #[test]
    fn a_deployment_reads_as_the_webhook_body_expects() {
        let mut rec: DeploymentRecord = serde_json::from_value(json!({"id": "a1b2c3d4", "project": "shop", "target": "production", "created": "2026-11-15T10:00:00Z"})).unwrap();
        rec.hosts = vec!["shop-a1b2c3d4.forkrail.com".into(), "shop.forkrail.com".into()];
        rec.region = Some("us-east-1".into());
        rec.replicas.insert("us-west-1".into(), "7".into());
        rec.via = Some(Via::Hook);
        rec.deploy_hook = Some(HookRef { id: "aaaaaaaaaaaa".into(), name: "CMS".into() });
        rec.status = Status::Error;
        rec.error = Some(Failure { step: Some("Building".into()), message: "npm ERR! missing script: build".into() });
        rec.git = Some(GitCommit { repo: "ada/shop".into(), branch: "main".into(), sha: "0123456".into(), message: "Fix".into(), full_sha: Some("0123456789".into()), author: Some("Ada".into()) });
        let settings = Settings { dashboard: Some("https://app.example.com/".into()), ..Default::default() };
        let body = envelope(DEPLOYMENT_ERROR, "us-east-1", deployment_payload(&rec, &settings));
        assert_eq!(body["type"], "deployment.error");
        assert!(body["id"].as_str().unwrap().starts_with("evt_") && body["createdAt"].as_u64().unwrap() > 0);
        let p = &body["payload"];
        assert_eq!(p["deployment"]["url"], "shop-a1b2c3d4.forkrail.com");
        assert_eq!(p["deployment"]["via"], "hook");
        assert_eq!(p["deployment"]["hook"]["name"], "CMS");
        assert_eq!(p["deployment"]["error"]["message"], "npm ERR! missing script: build");
        assert_eq!(p["deployment"]["meta"]["githubCommitSha"], "0123456789");
        assert_eq!(p["links"]["deployment"], "https://app.example.com/projects/shop/deployments/a1b2c3d4");
        assert_eq!(p["target"], "production");
        assert_eq!(p["regions"], json!(["us-east-1", "us-west-1"]));
        // A preview's target is null, as in the body this copies; no dashboard, no links.
        rec.target = Target::Preview;
        let p = deployment_payload(&rec, &Settings::default());
        assert!(p["target"].is_null() && p["links"]["deployment"].is_null());
    }
}
