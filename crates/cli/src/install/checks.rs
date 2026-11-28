//! What the installer looks at before it changes anything: who the credentials are, whether they
//! may do what the install needs, the region, the account's limits, the domain on Cloudflare, and
//! whether forkrail is in the account already. Every check reads; none writes.

use std::fmt;

use aws_config::SdkConfig;
use aws_sdk_iam::types::PolicyEvaluationDecisionType;

use super::cloudflare::{Cloudflare, TOKEN_PAGE, Zone};
use super::problem::aws_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// Worth knowing; the install can go on.
    Warn,
    /// The install can't go on until it's fixed.
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub verdict: Verdict,
    pub detail: String,
    pub fix: Option<String>,
    pub link: Option<String>,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Check { name, verdict: Verdict::Pass, detail: detail.into(), fix: None, link: None }
    }
    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>, link: Option<String>) -> Self {
        Check { name, verdict: Verdict::Warn, detail: detail.into(), fix: Some(fix.into()), link }
    }
    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>, link: Option<String>) -> Self {
        Check { name, verdict: Verdict::Fail, detail: detail.into(), fix: Some(fix.into()), link }
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mark = match self.verdict {
            Verdict::Pass => "ok  ",
            Verdict::Warn => "note",
            Verdict::Fail => "FAIL",
        };
        write!(f, "[{mark}] {}: {}", self.name, self.detail)?;
        if let Some(fix) = &self.fix {
            write!(f, "\n       {fix}")?;
        }
        if let Some(link) = &self.link {
            write!(f, "\n       {link}")?;
        }
        Ok(())
    }
}

/// Who the credentials are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub account: String,
    pub arn: String,
    pub partition: String,
}

pub async fn caller(config: &SdkConfig) -> Result<Caller, String> {
    let identity = aws_sdk_sts::Client::new(config).get_caller_identity().send().await.map_err(|e| aws_text(&e))?;
    let arn = identity.arn().unwrap_or_default().to_owned();
    Ok(Caller {
        account: identity.account().unwrap_or_default().to_owned(),
        partition: forkrail_common::install::partition_of(&arn).unwrap_or("aws").to_owned(),
        arn,
    })
}

/// What the install does in IAM terms; a principal that may do these may install.
const NEEDED: &[&str] = &[
    "iam:CreateRole",
    "iam:PutRolePolicy",
    "iam:PassRole",
    "iam:CreateInstanceProfile",
    "cloudformation:CreateStack",
    "cloudformation:CreateChangeSet",
    "s3:CreateBucket",
    "s3:PutObject",
    "kms:CreateKey",
    "ssm:PutParameter",
    "dynamodb:CreateTable",
    "lambda:CreateFunction",
    "ec2:RunInstances",
    "ec2:AllocateAddress",
    "ec2:CreateSecurityGroup",
    "ecr:CreateRepository",
    "sqs:CreateQueue",
    "sns:CreateTopic",
    "scheduler:CreateSchedule",
    "events:PutRule",
    "cloudtrail:CreateTrail",
    "logs:CreateLogGroup",
    "budgets:ModifyBudget",
];

/// The IAM entity to simulate for `arn`: a user as it is, an assumed role as its role (with the
/// role's path, which the session's ARN leaves out). None for the account's root user.
pub(crate) async fn principal(iam: &aws_sdk_iam::Client, arn: &str) -> Result<Option<String>, String> {
    if arn.ends_with(":root") {
        return Ok(None);
    }
    if let Some(rest) = arn.split(":assumed-role/").nth(1) {
        let role = rest.split('/').next().unwrap_or_default();
        let found = iam.get_role().role_name(role).send().await.map_err(|e| aws_text(&e))?;
        return Ok(found.role().map(|r| r.arn().to_owned()));
    }
    Ok(Some(arn.to_owned()))
}

pub async fn permissions(config: &SdkConfig, caller: &Caller) -> Check {
    const NAME: &str = "Access";
    let iam = aws_sdk_iam::Client::new(config);
    let entity = match principal(&iam, &caller.arn).await {
        Ok(None) => return Check::pass(NAME, "you're signed in as the account owner, who can set everything up"),
        Ok(Some(entity)) => entity,
        Err(said) => return Check::warn(NAME, format!("couldn't look up the role to check it ({said})"), "The install goes on; a missing permission will be named when a step needs it.", None),
    };
    let simulated = iam
        .simulate_principal_policy()
        .policy_source_arn(&entity)
        .set_action_names(Some(NEEDED.iter().map(|a| (*a).to_owned()).collect()))
        .send()
        .await;
    match simulated {
        Ok(out) => {
            let denied: Vec<String> = out
                .evaluation_results()
                .iter()
                .filter(|r| r.eval_decision() != &PolicyEvaluationDecisionType::Allowed)
                .map(|r| r.eval_action_name().to_owned())
                .collect();
            if denied.is_empty() {
                Check::pass(NAME, format!("{} can set everything up", short(&entity)))
            } else {
                Check::fail(
                    NAME,
                    format!("{} isn't allowed: {}", short(&entity), denied.join(", ")),
                    "Install with a user or role that has the AdministratorAccess policy.",
                    Some("https://console.aws.amazon.com/iam/home#/policies/details/arn%3Aaws%3Aiam%3A%3Aaws%3Apolicy%2FAdministratorAccess".to_owned()),
                )
            }
        }
        Err(e) => Check::warn(
            NAME,
            format!("couldn't check them ({})", first_line(&aws_text(&e))),
            "The install goes on; a missing permission will be named when a step needs it.",
            None,
        ),
    }
}

fn short(arn: &str) -> &str {
    arn.rsplit(['/', ':']).next().unwrap_or(arn)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

pub async fn region(config: &SdkConfig, region: &str) -> Check {
    const NAME: &str = "Region";
    let ec2 = aws_sdk_ec2::Client::new(config);
    match ec2.describe_regions().all_regions(true).send().await {
        Ok(out) => match out.regions().iter().find(|r| r.region_name() == Some(region)) {
            None => Check::fail(NAME, format!("{region} isn't an AWS region"), "Pick a region such as us-east-1, us-east-2, eu-central-1 or ap-southeast-1.", None),
            Some(r) if r.opt_in_status() == Some("not-opted-in") => Check::fail(
                NAME,
                format!("{region} is an opt-in region that this account hasn't turned on"),
                "Turn it on under Account → AWS Regions (it takes a few minutes), or pick another region.",
                Some("https://console.aws.amazon.com/billing/home#/account".to_owned()),
            ),
            Some(_) => Check::pass(NAME, format!("{} is available in your account", super::region_name(region))),
        },
        Err(e) => Check::fail(NAME, first_line(&aws_text(&e)).to_owned(), "Check the region's name and that the account is active.", None),
    }
}

/// How many app requests the account's functions may serve at once. New accounts start low (5 or
/// 10) and AWS raises it as the account is used; the install works either way, so this only says.
pub async fn lambda_concurrency(config: &SdkConfig) -> (Check, Option<i32>) {
    const NAME: &str = "App capacity";
    let lambda = aws_sdk_lambda::Client::new(config);
    match lambda.get_account_settings().send().await {
        Ok(out) => {
            let limit = out.account_limit().map(|l| l.concurrent_executions()).unwrap_or_default();
            let detail = if limit >= 100 {
                format!("your apps can answer {limit} requests at the same time")
            } else {
                format!("your apps can answer {limit} requests at the same time for now, which is fine to start. AWS raises it by itself as the account gets used")
            };
            (Check::pass(NAME, detail), Some(limit))
        }
        Err(e) => (Check::fail(NAME, first_line(&aws_text(&e)).to_owned(), "Make sure the account is active and Lambda is available in this region.", None), None),
    }
}

/// What the probe asks for: Lambda's usual most.
const MOST_MEMORY_MB: u32 = 10_240;

/// How much memory the account lets a function have. New accounts start low (512 MB has been seen)
/// and no API says how low, so this asks Lambda to make a function with the most memory and a role
/// that doesn't exist: Lambda refuses it either for the memory, naming the account's limit (it
/// checks the memory first), or for the role, which means the memory was fine. Nothing is made.
/// A low limit stops nothing: forkrail's functions are made to fit it, and so are apps' (lambda.rs).
pub async fn function_memory(config: &SdkConfig, account: &str) -> (Check, Option<u32>) {
    const NAME: &str = "App memory";
    const PROBE: &str = "forkrail-memory-check";
    let lambda = aws_sdk_lambda::Client::new(config);
    let Ok(zip) = probe_zip() else { return (Check::pass(NAME, "not checked"), None) };
    let tried = lambda
        .create_function()
        .function_name(PROBE)
        .runtime(aws_sdk_lambda::types::Runtime::Providedal2023)
        .handler("bootstrap")
        .architectures(aws_sdk_lambda::types::Architecture::Arm64)
        .role(format!("arn:aws:iam::{account}:role/forkrail-no-such-role"))
        .memory_size(MOST_MEMORY_MB as i32)
        .code(aws_sdk_lambda::types::FunctionCode::builder().zip_file(aws_sdk_lambda::primitives::Blob::new(zip)).build())
        .send()
        .await;
    let said = match tried {
        Ok(_) => {
            // Lambda took a role that doesn't exist: the function goes at once.
            let _ = lambda.delete_function().function_name(PROBE).send().await;
            return (Check::pass(NAME, "functions may use all the memory they need"), None);
        }
        Err(e) => aws_text(&e),
    };
    match memory_limit(&said) {
        None => (Check::pass(NAME, "functions may use all the memory they need"), None),
        Some(cap) => (
            Check::pass(NAME, format!("this new AWS account lets each function use up to {cap} MB for now. forkrail sizes its functions to fit, and AWS raises the limit as the account gets used")),
            Some(cap),
        ),
    }
}

pub use crate::lambda::memory_limit;

/// A zip with one small file, for the memory probe.
fn probe_zip() -> zip::result::ZipResult<Vec<u8>> {
    use std::io::Write as _;
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    out.start_file("bootstrap", zip::write::SimpleFileOptions::default())?;
    out.write_all(b"#!/bin/sh\n")?;
    Ok(out.finish()?.into_inner())
}

/// vCPUs the install's machines take: a router (t4g.small) and the analytics box (t4g.medium).
const VCPUS: f64 = 4.0;

pub async fn machines(config: &SdkConfig, region: &str) -> Check {
    const NAME: &str = "Servers";
    let quotas = aws_sdk_servicequotas::Client::new(config);
    let vcpus = quotas.get_service_quota().service_code("ec2").quota_code("L-1216C47A").send().await;
    let limit = match vcpus {
        Ok(out) => out.quota().and_then(|q| q.value()).unwrap_or(0.0),
        Err(e) => return Check::warn(NAME, format!("couldn't read the vCPU limit ({})", first_line(&aws_text(&e))), "The install goes on.", None),
    };
    let ec2 = aws_sdk_ec2::Client::new(config);
    let in_use = ec2
        .describe_instances()
        .filters(aws_sdk_ec2::types::Filter::builder().name("instance-state-name").values("pending").values("running").build())
        .send()
        .await
        .map(|out| out.reservations().iter().flat_map(|r| r.instances()).filter_map(|i| i.cpu_options().and_then(|c| c.core_count().zip(c.threads_per_core()))).map(|(c, t)| c * t).sum::<i32>())
        .map(f64::from)
        .unwrap_or(0.0);
    if limit - in_use >= VCPUS {
        Check::pass(NAME, "there's room for forkrail's two small servers")
    } else {
        Check::fail(
            NAME,
            format!("the install needs 4 vCPUs and only {:.0} are free ({in_use:.0} of {limit:.0} in use)", (limit - in_use).max(0.0)),
            "Ask for more \"Running On-Demand Standard instances\" vCPUs in Service Quotas (8 is plenty).",
            Some(format!("https://{region}.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-1216C47A")),
        )
    }
}

/// No uninstall is still at work here: an install started meanwhile would make what it's deleting,
/// and each would undo the other.
pub async fn not_uninstalling(config: &SdkConfig) -> Check {
    const NAME: &str = "No uninstall running";
    use aws_sdk_cloudformation::types::StackStatus;
    let cfn = aws_sdk_cloudformation::Client::new(config);
    let mut deleting = Vec::new();
    let mut token = None;
    loop {
        let page = match cfn.list_stacks().stack_status_filter(StackStatus::DeleteInProgress).set_next_token(token).send().await {
            Ok(page) => page,
            // Not knowing isn't a reason to stop: the other checks say whether the account works.
            Err(_) => return Check::pass(NAME, "no forkrail stack is being deleted"),
        };
        deleting.extend(page.stack_summaries().iter().filter_map(|s| s.stack_name()).filter(|n| n.starts_with("forkrail-")).map(str::to_owned));
        token = page.next_token().map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    if deleting.is_empty() {
        Check::pass(NAME, "no forkrail stack is being deleted")
    } else {
        Check::fail(
            NAME,
            format!("an uninstall is still removing forkrail here ({} being deleted)", deleting.join(", ")),
            "Wait for `forkrail uninstall` to finish, then run `forkrail install` again.",
            None,
        )
    }
}

pub async fn addresses(config: &SdkConfig, region: &str) -> Check {
    const NAME: &str = "Public addresses";
    let quotas = aws_sdk_servicequotas::Client::new(config);
    let limit = match quotas.get_service_quota().service_code("ec2").quota_code("L-0263D0A3").send().await {
        Ok(out) => out.quota().and_then(|q| q.value()).unwrap_or(5.0),
        Err(_) => 5.0,
    };
    let used = aws_sdk_ec2::Client::new(config).describe_addresses().send().await.map(|o| o.addresses().len() as f64).unwrap_or(0.0);
    if limit - used >= 2.0 {
        Check::pass(NAME, "there's room for forkrail's two fixed addresses")
    } else {
        Check::fail(
            NAME,
            format!("the install needs 2 Elastic IPs and {used:.0} of {limit:.0} are in use"),
            "Release one you don't use, or ask for more in Service Quotas.",
            Some(format!("https://{region}.console.aws.amazon.com/servicequotas/home/services/ec2/quotas/L-0263D0A3")),
        )
    }
}

/// The domain: the token works, a zone it can see holds the domain, the zone is active, and the
/// names the install takes are free (or the installer's own from before).
pub async fn domain(token: &str, domain: &str) -> (Check, Option<Zone>) {
    const NAME: &str = "Domain";
    let token_fix = format!("Make a new token with everything filled in (Continue to summary, then Create Token): {TOKEN_PAGE}");
    let cf = match Cloudflare::new(token) {
        Ok(cf) => cf,
        Err(e) => return (Check::fail(NAME, e.to_string(), "Check this machine's root certificates.", None), None),
    };
    if let Err(e) = cf.verify().await {
        return (Check::fail(NAME, format!("{e:#}"), token_fix, None), None);
    }
    let zone = match cf.zone_for(domain).await {
        Ok(Some(zone)) => zone,
        Ok(None) => {
            return (
                Check::fail(
                    NAME,
                    format!("the token can't see a Cloudflare zone for {domain}"),
                    format!("Add the domain to Cloudflare (the free plan works) and give the token access to its zone. {token_fix}"),
                    Some("https://dash.cloudflare.com/?to=/:account/add-site".to_owned()),
                ),
                None,
            );
        }
        Err(e) => return (Check::fail(NAME, format!("{e:#}"), token_fix, None), None),
    };
    if zone.status != "active" {
        let servers = zone.name_servers.join(" and ");
        return (
            Check::fail(
                NAME,
                format!("{}'s zone is {} on Cloudflare: the domain's nameservers aren't Cloudflare's yet", zone.name, zone.status),
                format!("At your domain's registrar, set its nameservers to {servers}. Cloudflare usually sees it within an hour."),
                None,
            ),
            Some(zone),
        );
    }
    for name in [format!("*.{domain}"), format!("dashboard.{domain}")] {
        match cf.records(&zone.id, &name).await {
            Ok(records) => {
                if let Some(other) = records.iter().find(|r| r.comment.as_deref() != Some(super::cloudflare::COMMENT)) {
                    return (
                        Check::fail(
                            NAME,
                            format!("{name} already has a {} record ({}) that the installer didn't make", other.kind, other.content),
                            format!("The install serves every name under {domain}. Remove that record in Cloudflare, or install under a subdomain such as apps.{domain}."),
                            Some(format!("https://dash.cloudflare.com/?to=/:account/{}/dns", zone.name)),
                        ),
                        Some(zone),
                    );
                }
            }
            Err(e) => return (Check::fail(NAME, format!("the token can't read {}'s DNS records: {e:#}", zone.name), token_fix, None), Some(zone)),
        }
    }
    (Check::pass(NAME, format!("{domain} is on Cloudflare, and the token can set up its addresses")), Some(zone))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(short("arn:aws:iam::1:role/aws-reserved/sso.amazonaws.com/AWSReservedSSO_Admin_ab"), "AWSReservedSSO_Admin_ab");
        assert_eq!(short("arn:aws:iam::1:user/ada"), "ada");
    }

    #[test]
    fn the_memory_probe_is_a_zip() {
        assert!(probe_zip().unwrap().starts_with(b"PK"));
    }

    #[test]
    fn checks_print_their_fix() {
        let c = Check::fail("Domain", "no zone", "add it", Some("https://x".into()));
        let text = c.to_string();
        assert!(text.starts_with("[FAIL] Domain: no zone"));
        assert!(text.contains("add it") && text.contains("https://x"));
    }
}
