//! forkrail's CloudFormation stacks, as the installer applies them: the same templates
//! `deploy/aws/stack.sh` applies (deploy/install/NAME.yaml as stack forkrail-NAME), built into the
//! CLI so the installer needs no checkout. A stack is created or updated through a change set,
//! waited for, and, when it fails, explained by the resources that failed and why.

use std::collections::BTreeMap;
use std::time::Duration;

use aws_config::SdkConfig;
use aws_sdk_cloudformation::Client;
use aws_sdk_cloudformation::types::{Capability, ChangeSetType, Parameter, Tag};

use super::problem::aws_text;

/// The templates, by stack name (without `forkrail-`).
pub fn template(name: &str) -> Option<&'static str> {
    Some(match name {
        "builder" => include_str!("../../../../deploy/install/builder.yaml"),
        "control" => include_str!("../../../../deploy/install/control.yaml"),
        "cost-tags" => include_str!("../../../../deploy/install/cost-tags.yaml"),
        "guard" => include_str!("../../../../deploy/install/guard.yaml"),
        "image" => include_str!("../../../../deploy/install/image.yaml"),
        "install-analytics" | "install-router" => include_str!("../../../../deploy/install/install-box.yaml"),
        "install-fetch" => include_str!("../../../../deploy/install/install-fetch.yaml"),
        "install-network" => include_str!("../../../../deploy/install/install-network.yaml"),
        "install-roles" => include_str!("../../../../deploy/install/install-roles.yaml"),
        "install-secrets" => include_str!("../../../../deploy/install/install-secrets.yaml"),
        "notify" => include_str!("../../../../deploy/install/notify.yaml"),
        "registry" => include_str!("../../../../deploy/install/registry.yaml"),
        "retention" => include_str!("../../../../deploy/install/retention.yaml"),
        "schedules" => include_str!("../../../../deploy/install/schedules.yaml"),
        "screenshot" => include_str!("../../../../deploy/install/screenshot.yaml"),
        "secrets" => include_str!("../../../../deploy/install/secrets.yaml"),
        "spend-backstop" => include_str!("../../../../deploy/install/spend-backstop.yaml"),
        "state" => include_str!("../../../../deploy/install/state.yaml"),
        "web" => include_str!("../../../../deploy/install/web.yaml"),
        _ => return None,
    })
}

/// What a stack is, in a few plain words, for what the installer says it's doing.
pub fn purpose(name: &str) -> &'static str {
    match name {
        "builder" => "the app builder",
        "control" => "the control service",
        "cost-tags" => "cost tracking",
        "guard" => "the spending guard",
        "image" => "image resizing",
        "install-analytics" => "the log server",
        "install-fetch" => "the release copier",
        "install-router" => "the router",
        "install-network" => "the network",
        "install-roles" => "permissions",
        "install-secrets" | "secrets" => "secrets",
        "notify" => "alerts",
        "registry" => "the builder's image store",
        "retention" => "log cleanup",
        "schedules" => "scheduled jobs",
        "screenshot" => "page previews",
        "spend-backstop" => "the spending limit",
        "state" => "the install's tables",
        "web" => "the dashboard's tables",
        _ => "a part of forkrail",
    }
}

pub fn stack_name(name: &str) -> String {
    format!("forkrail-{name}")
}

/// A stack's outputs, by key.
pub type Outputs = BTreeMap<String, String>;

/// Creates or updates stack forkrail-`name` with `parameters`, and waits for it. Parameters not
/// given keep the stack's current value (or the template's default). Errors are the service's
/// words, with the failed resources' reasons for a failed stack.
pub async fn apply(config: &SdkConfig, name: &str, parameters: &[(&str, String)]) -> Result<Outputs, String> {
    let body = template(name).ok_or_else(|| format!("the CLI has no template {name}"))?;
    let cfn = Client::new(config);
    let stack = stack_name(name);
    let status = status(&cfn, &stack).await?;
    let kind = match status.as_deref() {
        None | Some("REVIEW_IN_PROGRESS") => ChangeSetType::Create,
        // A stack whose creation failed can't be updated: it goes, and is made again.
        Some("ROLLBACK_COMPLETE") | Some("ROLLBACK_FAILED") => {
            delete_and_wait(&cfn, &stack).await?;
            ChangeSetType::Create
        }
        Some(s) if s.ends_with("_IN_PROGRESS") => {
            wait(&cfn, &stack).await?;
            ChangeSetType::Update
        }
        Some(_) => ChangeSetType::Update,
    };
    if kind == ChangeSetType::Create {
        // A failed try leaves the log groups the template keeps (DeletionPolicy: Retain), and
        // CloudFormation won't make one whose name is taken. Empty ones are only in the way.
        clear_empty_log_groups(config, body).await;
    }
    let existing: Vec<String> = if kind == ChangeSetType::Update { current_parameters(&cfn, &stack).await? } else { Vec::new() };
    let declared = declared_parameters(body);
    let mut params: Vec<Parameter> = parameters
        .iter()
        .filter(|(key, _)| declared.iter().any(|d| d == key))
        .map(|(key, value)| Parameter::builder().parameter_key(*key).parameter_value(value).build())
        .collect();
    for key in existing {
        if !parameters.iter().any(|(k, _)| *k == key) && declared.contains(&key) {
            params.push(Parameter::builder().parameter_key(key).use_previous_value(true).build());
        }
    }
    let change_set = format!("forkrail-install-{}", crate::state::now_ms());
    let created = cfn
        .create_change_set()
        .stack_name(&stack)
        .change_set_name(&change_set)
        .change_set_type(kind.clone())
        .template_body(body)
        .set_parameters(Some(params))
        .capabilities(Capability::CapabilityNamedIam)
        .capabilities(Capability::CapabilityIam)
        .tags(Tag::builder().key(forkrail_common::install::TAG_KEY).value(forkrail_common::install::TAG_VALUE).build())
        .send()
        .await
        .map_err(|e| aws_text(&e))?;
    let change_set_id = created.id().map_or_else(|| change_set.clone(), str::to_owned);
    // The change set is computed; an update with nothing to change says so and is dropped.
    loop {
        let described = cfn.describe_change_set().stack_name(&stack).change_set_name(&change_set).send().await.map_err(|e| aws_text(&e))?;
        let status = described.status().map(|s| s.as_str().to_owned()).unwrap_or_default();
        match status.as_str() {
            "CREATE_COMPLETE" => break,
            "FAILED" => {
                let reason = described.status_reason().unwrap_or_default().to_owned();
                if reason.contains("didn't contain changes") || reason.contains("No updates are to be performed") {
                    let _ = cfn.delete_change_set().stack_name(&stack).change_set_name(&change_set).send().await;
                    return outputs(&cfn, &stack).await;
                }
                // Checks CloudFormation runs before it changes anything say what they found only here.
                let found = validations(&cfn, &change_set_id).await;
                let _ = cfn.delete_change_set().stack_name(&stack).change_set_name(&change_set).send().await;
                return Err(if found.is_empty() { format!("CloudFormation couldn't plan {stack}: {reason}") } else { format!("CloudFormation couldn't plan {stack}: {reason} {found}") });
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    cfn.execute_change_set().stack_name(&stack).change_set_name(&change_set).send().await.map_err(|e| aws_text(&e))?;
    let final_status = wait(&cfn, &stack).await?;
    if final_status == "CREATE_COMPLETE" || final_status == "UPDATE_COMPLETE" {
        if kind == ChangeSetType::Create {
            // As stack.sh does: forkrail's stacks are protected from deletion by accident.
            let _ = cfn.update_termination_protection().stack_name(&stack).enable_termination_protection(true).send().await;
        }
        outputs(&cfn, &stack).await
    } else {
        Err(format!("{stack} ended {final_status}: {}", failures(&cfn, &stack).await))
    }
}

/// The log groups a template names outright (`LogGroupName: /aws/lambda/forkrail-…`).
fn named_log_groups(body: &str) -> Vec<&str> {
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("LogGroupName:"))
        .map(|name| name.trim().trim_matches(['"', '\'']))
        .filter(|name| name.starts_with("/aws/lambda/forkrail-") || name.starts_with("/forkrail/"))
        .collect()
}

/// Deletes the template's named log groups that exist but hold nothing, so the stack can make them.
/// One with logs in it stays (and CloudFormation then says so).
async fn clear_empty_log_groups(config: &SdkConfig, body: &str) {
    let logs = aws_sdk_cloudwatchlogs::Client::new(config);
    for name in named_log_groups(body) {
        let Ok(found) = logs.describe_log_groups().log_group_name_prefix(name).send().await else { continue };
        let empty = found.log_groups().iter().any(|g| g.log_group_name() == Some(name) && g.stored_bytes().unwrap_or(0) == 0);
        if empty {
            let streams = logs.describe_log_streams().log_group_name(name).limit(1).send().await.map(|o| o.log_streams().len()).unwrap_or(1);
            if streams == 0 {
                let _ = logs.delete_log_group().log_group_name(name).send().await;
            }
        }
    }
}

/// Deletes stack forkrail-`name` (its protection first) and waits; resources it marks Retain stay.
pub async fn delete(config: &SdkConfig, name: &str) -> Result<(), String> {
    let cfn = Client::new(config);
    let stack = stack_name(name);
    if status(&cfn, &stack).await?.is_none() {
        return Ok(());
    }
    cfn.update_termination_protection().stack_name(&stack).enable_termination_protection(false).send().await.map_err(|e| aws_text(&e))?;
    delete_and_wait(&cfn, &stack).await
}

async fn delete_and_wait(cfn: &Client, stack: &str) -> Result<(), String> {
    cfn.delete_stack().stack_name(stack).send().await.map_err(|e| aws_text(&e))?;
    let last = wait(cfn, stack).await?;
    if last == "DELETED" { Ok(()) } else { Err(format!("{stack} ended {last}: {}", failures(cfn, stack).await)) }
}

/// The stack's status, or None when there's no such stack.
async fn status(cfn: &Client, stack: &str) -> Result<Option<String>, String> {
    match cfn.describe_stacks().stack_name(stack).send().await {
        Ok(out) => Ok(out.stacks().first().and_then(|s| s.stack_status()).map(|s| s.as_str().to_owned())),
        Err(e) => {
            let text = aws_text(&e);
            if text.contains("does not exist") { Ok(None) } else { Err(text) }
        }
    }
}

/// Waits until the stack isn't in progress; its final status (`DELETED` once gone).
async fn wait(cfn: &Client, stack: &str) -> Result<String, String> {
    loop {
        match status(cfn, stack).await? {
            None => return Ok("DELETED".to_owned()),
            Some(s) if s == "DELETE_COMPLETE" => return Ok("DELETED".to_owned()),
            Some(s) if s.ends_with("_IN_PROGRESS") => tokio::time::sleep(Duration::from_secs(5)).await,
            Some(s) => return Ok(s),
        }
    }
}

async fn current_parameters(cfn: &Client, stack: &str) -> Result<Vec<String>, String> {
    let out = cfn.describe_stacks().stack_name(stack).send().await.map_err(|e| aws_text(&e))?;
    Ok(out.stacks().first().map(|s| s.parameters().iter().filter_map(|p| p.parameter_key().map(str::to_owned)).collect()).unwrap_or_default())
}

/// Whether stack forkrail-`name` exists (in any state but deleted).
pub async fn exists(config: &SdkConfig, name: &str) -> Result<bool, String> {
    Ok(status(&Client::new(config), &stack_name(name)).await?.is_some_and(|s| s != "DELETE_COMPLETE"))
}

pub async fn outputs_of(config: &SdkConfig, name: &str) -> Result<Outputs, String> {
    outputs(&Client::new(config), &stack_name(name)).await
}

async fn outputs(cfn: &Client, stack: &str) -> Result<Outputs, String> {
    let out = cfn.describe_stacks().stack_name(stack).send().await.map_err(|e| aws_text(&e))?;
    Ok(out
        .stacks()
        .first()
        .map(|s| s.outputs().iter().filter_map(|o| Some((o.output_key()?.to_owned(), o.output_value()?.to_owned()))).collect())
        .unwrap_or_default())
}

/// What CloudFormation's own checks of a change set found wrong, one sentence per resource
/// ("Function (AWS::Lambda::Function, forkrail-control): …"); empty when they found nothing or
/// can't be read.
async fn validations(cfn: &Client, change_set: &str) -> String {
    let filter = aws_sdk_cloudformation::types::EventFilter::builder().failed_events(true).build();
    let Ok(out) = cfn.describe_events().change_set_name(change_set).filters(filter).send().await else { return String::new() };
    let mut lines: Vec<String> = out
        .operation_events()
        .iter()
        .filter(|e| e.validation_status().is_some_and(|s| s.as_str() == "FAILED") || e.validation_status_reason().is_some())
        .map(|e| {
            let what = [e.resource_type(), e.physical_resource_id().filter(|p| !p.is_empty())].into_iter().flatten().collect::<Vec<_>>().join(", ");
            let reason = e.validation_status_reason().or(e.resource_status_reason()).unwrap_or("no reason given");
            format!("{} ({what}): {reason}", e.logical_resource_id().unwrap_or("?"))
        })
        .collect();
    lines.dedup();
    lines.join("; ")
}

/// The failed resources of the stack's latest operation and why, as one line each, the first cause
/// first (later ones are mostly "Resource creation cancelled").
async fn failures(cfn: &Client, stack: &str) -> String {
    let Ok(out) = cfn.describe_stack_events().stack_name(stack).send().await else { return "(its events couldn't be read)".into() };
    let mut lines: Vec<String> = out
        .stack_events()
        .iter()
        .filter(|e| e.resource_status().is_some_and(|s| s.as_str().ends_with("_FAILED")))
        .filter(|e| !e.resource_status_reason().unwrap_or_default().contains("cancelled"))
        .take(5)
        .map(|e| format!("{} ({}): {}", e.logical_resource_id().unwrap_or("?"), e.resource_type().unwrap_or("?"), e.resource_status_reason().unwrap_or("no reason given")))
        .collect();
    lines.reverse();
    if lines.is_empty() { "no resource said why".into() } else { lines.join("; ") }
}

/// The names under `Parameters:` in a template.
fn declared_parameters(body: &str) -> Vec<String> {
    let mut inside = false;
    let mut names = Vec::new();
    for line in body.lines() {
        if line.starts_with("Parameters:") {
            inside = true;
            continue;
        }
        if inside {
            if !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
                break;
            }
            if let Some(name) = line.strip_prefix("  ").and_then(|l| l.strip_suffix(':'))
                && !name.starts_with(' ')
                && !name.starts_with('#')
            {
                names.push(name.to_owned());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_log_groups_templates_keep() {
        assert_eq!(named_log_groups(template("control").unwrap()), vec!["/aws/lambda/forkrail-control"]);
        assert_eq!(named_log_groups("      LogGroupName: !Sub /aws/lambda/${Name}\n      LogGroupName: \"/forkrail/x\""), vec!["/forkrail/x"]);
    }

    #[test]
    fn every_template_is_built_in_and_small_enough() {
        for name in ["builder", "control", "cost-tags", "guard", "image", "install-analytics", "install-network", "install-roles", "install-secrets", "notify", "registry", "retention", "schedules", "screenshot", "secrets", "spend-backstop", "state", "web"] {
            let body = template(name).unwrap();
            assert!(body.len() <= 51_200, "{name} is {} bytes, over what CloudFormation takes inline", body.len());
        }
    }

    #[test]
    fn parameters_are_read_from_the_template() {
        let names = declared_parameters(template("guard").unwrap());
        assert_eq!(names, vec!["CodeKey", "Regions", "Accelerator", "EveryMinutes", "ReservedConcurrency"]);
        assert!(declared_parameters(template("secrets").unwrap()).is_empty());
    }
}
