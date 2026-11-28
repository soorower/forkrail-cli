//! What went wrong in an install step, said so a person can act on it: the step, what the service
//! answered, the likely cause in plain words, and what to do about it.
//!
//! Every call the installer makes goes through [`twice`]: it is tried, and if it fails it is tried
//! once more after a pause (longer for the errors AWS is known to give while something it just made
//! spreads, such as a new role). A second failure stops the step with a [`Problem`].

use std::fmt;
use std::future::Future;
use std::time::Duration;

/// A failure the installer stops on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The step, as the person saw it in the list ("Create the state bucket").
    pub step: String,
    /// What was being done when it failed ("creating bucket forkrail-artifacts-1234").
    pub action: String,
    /// What the service answered, as it said it.
    pub said: String,
    /// The likely cause, in plain words.
    pub cause: String,
    /// What to do about it.
    pub fix: String,
    /// Where to do it, when there's a page for it.
    pub link: Option<String>,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} failed while {}.", self.step, self.action)?;
        writeln!(f, "  What happened: {}", self.cause)?;
        writeln!(f, "  How to fix it: {}", self.fix)?;
        if let Some(link) = &self.link {
            writeln!(f, "  Where: {link}")?;
        }
        writeln!(f, "  AWS said: {}", self.said)?;
        write!(f, "Run `forkrail install` again to continue from this step; the steps before it are kept.")
    }
}

impl std::error::Error for Problem {}

/// How an error looks once read: its cause and fix, and how long to wait before the second try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub cause: String,
    pub fix: String,
    pub link: Option<String>,
    /// How long to wait before the one retry.
    pub wait: Duration,
}

const SHORT: Duration = Duration::from_secs(3);
/// IAM changes take several seconds to reach other services (a new role, a new instance profile).
const SPREAD: Duration = Duration::from_secs(15);

fn expired(said: &str) -> bool {
    ["ExpiredToken", "TokenRefreshRequired", "the SSO session", "login session", "session has expired", "reauthenticate", "TOKEN_EXPIRED"].iter().any(|n| said.contains(n))
}

fn no_credentials(said: &str) -> bool {
    ["InvalidClientTokenId", "UnrecognizedClientException", "SignatureDoesNotMatch", "no credentials", "NoCredentials", "failed to load credentials", "CredentialsNotLoaded", "no providers in chain"]
        .iter()
        .any(|n| said.contains(n))
}

/// Whether an AWS error says there's no sign-in, or the one there is has run out: signing in again fixes it.
pub fn signed_out(said: &str) -> bool {
    expired(said) || no_credentials(said)
}

/// Reads an error's text (the AWS SDK's full context, or Cloudflare's answer) into a cause and fix.
/// `region` is the region the call was made in, for the links.
pub fn read(said: &str, region: &str) -> Reading {
    let has = |needle: &str| said.contains(needle);
    let reading = |cause: &str, fix: &str, link: Option<String>, wait: Duration| Reading {
        cause: cause.to_owned(),
        fix: fix.to_owned(),
        link,
        wait,
    };
    let quotas = |service: &str| Some(format!("https://{region}.console.aws.amazon.com/servicequotas/home/services/{service}/quotas"));

    // The Mac app has no terminal to send anyone to.
    let window = super::page::in_window();
    if expired(said) {
        let fix = if window {
            "Open Forkrail again and click Sign in to AWS."
        } else {
            "Run `forkrail install` again in a terminal: it signs you in to AWS in the browser. (`aws login` or `aws sso login` work too.)"
        };
        return reading("Your AWS sign-in has expired.", fix, None, SHORT);
    }
    if no_credentials(said) {
        if window {
            return reading(
                "No valid AWS credentials were found.",
                &format!("Open Forkrail again and click Sign in to AWS. AWS keys set in this Mac's environment (AWS_ACCESS_KEY_ID) are used instead of a sign-in, so remove them if they're old. {}", crate::aws_signin::ROOT_NEEDS_MFA),
                None,
                SHORT,
            );
        }
        return reading(
            "No valid AWS credentials were found.",
            "Run `forkrail install` in a terminal, without --yes: it signs you in to AWS in the browser. Or run it in AWS CloudShell, which is signed in already. Signing in as the root user needs an MFA device on it first (IAM → Security credentials → Assign MFA device).",
            Some("https://console.aws.amazon.com/cloudshell/home".to_owned()),
            SHORT,
        );
    }
    if let Some(cap) = super::checks::memory_limit(said) {
        return reading(
            &format!("This new AWS account lets each function use only {cap} MB of memory for now, and this part needs more."),
            &format!(
                "AWS raises it by itself as a new account gets used. To get it sooner, open a free support case (Account and billing) asking AWS to raise the Lambda function memory limit to 10,240 MB in {region}, then run `forkrail install` again."
            ),
            Some("https://support.console.aws.amazon.com/support/home#/case/create?issueType=customer-service".to_owned()),
            SHORT,
        );
    }
    if has("OperationAborted") || has("conflicting conditional operation") {
        return reading(
            "AWS is still freeing this bucket name from where it was just deleted (an install started over in another region). That can take up to an hour.",
            "Wait a while, then run `forkrail install` again: it carries on from here.",
            None,
            Duration::from_secs(60),
        );
    }
    if (has("ResourceExistenceCheck") || has("already exists") || has("AlreadyExists")) && !has("BucketAlreadyExists") {
        return reading(
            "Something forkrail makes already exists in your AWS account, usually left over from an earlier try.",
            "Run `forkrail install` again: it clears empty leftovers by itself. If it stops here again, delete what's named in AWS's message below, then run it again.",
            None,
            // A queue just deleted keeps its name for 60 seconds.
            Duration::from_secs(65),
        );
    }
    if has("OptInRequired") || has("is not enabled for this account") || has("not subscribed") || has("SubscriptionRequiredException") {
        return reading(
            "This AWS account isn't set up for this service or region yet. A brand-new account can take a few minutes after sign-up, or the region may need turning on.",
            "Turn the region on under Account settings (only needed for opt-in regions), or wait for the account's activation email, then run the installer again.",
            Some("https://console.aws.amazon.com/billing/home#/account".to_owned()),
            SPREAD,
        );
    }
    // Errors AWS gives while something it just made spreads: retried after a longer pause.
    if has("cannot be assumed by Lambda") || has("role defined for the function cannot be assumed") || has("Invalid IamInstanceProfile") || has("Value (") && has("for parameter iamInstanceProfile") || has("InvalidParameterValue") && has("instance profile") {
        return reading(
            "A role the installer just made hadn't reached every AWS service yet.",
            "This usually clears by itself within a minute: run `forkrail install` again.",
            None,
            SPREAD,
        );
    }
    if has("Throttling") || has("TooManyRequests") || has("RequestLimitExceeded") || has("SlowDown") || has("Rate exceeded") || has("429") && has("Cloudflare") {
        return reading(
            "AWS asked the installer to slow down.",
            "Wait a minute and run `forkrail install` again.",
            None,
            SPREAD,
        );
    }
    if has("PendingVerification") || has("being validated") || has("currently being verified") {
        return reading(
            "AWS is still verifying this new account before it lets it start machines. This usually takes minutes, sometimes a few hours.",
            "Wait for AWS's email saying the account is ready, then run `forkrail install` again.",
            None,
            SPREAD,
        );
    }
    if has("NoDefaultVpc") || has("NoSubnet") {
        return reading(
            "The region's default network (default VPC), where the install puts its two machines, is missing or has no subnet that offers the machines' size.",
            "Create the default VPC again in the VPC console (Actions → Create default VPC), then run the installer again.",
            Some(format!("https://{region}.console.aws.amazon.com/vpcconsole/home?region={region}#vpcs:")),
            SHORT,
        );
    }
    if has("SsmNotReady") {
        return reading(
            "A machine the installer started didn't check in with AWS Systems Manager, which the installer sets it up through.",
            "Run `forkrail install` again in a few minutes. If it fails again, check in the EC2 console that the machine is running and its subnet reaches the internet.",
            Some(format!("https://{region}.console.aws.amazon.com/ec2/home?region={region}#Instances:tag:project=forkrail")),
            SPREAD,
        );
    }
    if has("VcpuLimitExceeded") || has("You have requested more vCPU capacity") {
        return reading(
            "The account's limit on machines (EC2 vCPUs) is too low. New accounts start low.",
            "Ask for more on-demand Standard vCPUs in Service Quotas (8 is plenty), then run the installer again.",
            quotas("ec2"),
            SHORT,
        );
    }
    if has("AddressLimitExceeded") {
        return reading(
            "The account has used all its fixed public addresses (Elastic IPs) in this region; the default is 5.",
            "Release one you don't use, or ask for more in Service Quotas, then run the installer again.",
            quotas("ec2"),
            SHORT,
        );
    }
    if has("InsufficientInstanceCapacity") {
        return reading(
            "AWS has no spare machines of this size in this zone right now.",
            "Run the installer again in a few minutes; it tries another zone.",
            None,
            SPREAD,
        );
    }
    if has("ServiceQuotaExceeded") || has("LimitExceeded") || has("TooManyBuckets") || has("CodeStorageExceeded") {
        return reading(
            "An AWS account limit was reached.",
            "Raise the limit named above in Service Quotas, or remove something unused, then run the installer again.",
            Some(format!("https://{region}.console.aws.amazon.com/servicequotas/home")),
            SHORT,
        );
    }
    if has("BucketAlreadyExists") {
        return reading(
            "Another AWS account already owns a bucket with this name.",
            "Run the installer with --bucket NAME to pick another name for the install's bucket.",
            None,
            SHORT,
        );
    }
    if has("AccessDenied") || has("UnauthorizedOperation") || has("not authorized to perform") || has("AuthorizationError") || has("explicit deny") {
        let action = said
            .split("not authorized to perform: ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .map(|a| a.trim_end_matches([',', '.', ':']));
        let cause = match action {
            Some(action) => format!("Your AWS user or role isn't allowed to do {action}, which the install needs."),
            None => "Your AWS user or role isn't allowed to do something the install needs.".to_owned(),
        };
        let fix = if has("explicit deny") || has("service control policy") {
            "A policy explicitly denies it (often an organization's service control policy): ask whoever manages the account to allow it for the install, then run the installer again."
        } else {
            "Install with a user or role that has the AdministratorAccess policy (the install creates roles, keys and machines), then run the installer again."
        };
        return Reading {
            cause,
            fix: fix.to_owned(),
            link: Some("https://console.aws.amazon.com/iam/home#/users".to_owned()),
            wait: SHORT,
        };
    }
    reading(
        &format!("AWS refused it: {}", gist(said)),
        "Run `forkrail install` again: it carries on from here. If it fails the same way, open an issue on forkrail's GitHub with AWS's message below (it holds no secrets).",
        None,
        SHORT,
    )
}

/// AWS's answer in a line, for the page: the error's code and message when the SDK's long account
/// of it has them, else its [`gist`]. The terminal and the log keep all of it.
pub fn brief(said: &str) -> String {
    let field = |name: &str| said.split(&format!("{name}: \"")).nth(1).and_then(|rest| rest.split('"').next()).filter(|v| !v.is_empty());
    match (field("code"), field("message")) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (None, Some(message)) => message.to_owned(),
        _ => gist(said),
    }
}

/// The heart of an AWS error, for a person: its first sentence or so, without request ids.
fn gist(said: &str) -> String {
    let text = said.split(" (Service:").next().unwrap_or(said).split(" (RequestToken").next().unwrap_or(said);
    let text = text.rsplit("Resource handler returned message: ").next().unwrap_or(text).trim().trim_matches('"');
    let short: String = text.chars().take(240).collect();
    if short.len() < text.len() { format!("{short}…") } else { short }
}

/// Runs `call` once, and once more after a pause if it fails; a second failure is a [`Problem`]
/// for `step` while doing `action`. `region` is where the call goes, for the links.
pub async fn twice<T, E, F, Fut>(step: &str, action: &str, region: &str, mut call: F) -> Result<T, Problem>
where
    E: fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    match call().await {
        Ok(value) => Ok(value),
        Err(first) => {
            let first = first.to_string();
            let reading = read(&first, region);
            tokio::time::sleep(reading.wait).await;
            call().await.map_err(|second| {
                let second = second.to_string();
                // The second try can fail only because of what the first left: then the first is
                // what went wrong, and both are shown.
                let (reading, said) = if second == first {
                    (read(&second, region), second)
                } else {
                    let leftover = second.contains("ResourceExistenceCheck") || second.contains("already exists");
                    (read(if leftover { &first } else { &second }, region), format!("First try: {first}\n\nSecond try: {second}"))
                };
                Problem { step: step.to_owned(), action: action.to_owned(), said, cause: reading.cause, fix: reading.fix, link: reading.link }
            })
        }
    }
}

/// The SDK's error with everything it knows (service, code, message), as one line.
pub fn aws_text<E: std::error::Error>(e: &E) -> String {
    aws_sdk_sts::error::DisplayErrorContext(e).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_gets_aws_answer_in_a_line() {
        let said = r#"service error: unhandled error (InvalidClientTokenId): Error { code: "InvalidClientTokenId", message: "The security token included in the request is invalid.", aws_request_id: "0092" } (ServiceError(ServiceError { source: ...raw: Response { status: StatusCode(403) } }))"#;
        assert_eq!(brief(said), "InvalidClientTokenId: The security token included in the request is invalid.");
        assert_eq!(brief("no sign-in in 30 minutes; run the installer again to sign in"), "no sign-in in 30 minutes; run the installer again to sign in");
    }

    #[test]
    fn todays_failure_reads_plainly() {
        let memory = "forkrail-control ended ROLLBACK_COMPLETE: Function (AWS::Lambda::Function): Resource handler returned message: \"'MemorySize' value failed to satisfy constraint: Member must have value less than or equal to 512 (Service: Lambda, Status Code: 400, Request ID: 63ca)\"";
        assert!(read(memory, "us-east-1").cause.contains("512 MB"));
        let leftover = "CloudFormation couldn't plan forkrail-control: Validation failed with 1 error(s). The following validation(s) failed: [AWS::EarlyValidation::ResourceExistenceCheck].";
        assert!(read(leftover, "us-east-1").cause.contains("already exists"));
        let odd = read("Function (AWS::Lambda::Function): Resource handler returned message: \"Something odd happened\" (Service: Lambda, Status Code: 400)", "us-east-1");
        assert_eq!(odd.cause, "AWS refused it: Something odd happened");
    }

    #[test]
    fn permission_names_the_action() {
        let r = read(
            "AccessDenied: User: arn:aws:iam::1:user/x is not authorized to perform: iam:CreateRole on resource: role forkrail-worker",
            "us-east-2",
        );
        assert!(r.cause.contains("iam:CreateRole"), "{}", r.cause);
        assert!(r.fix.contains("AdministratorAccess"));
    }

    #[test]
    fn scp_says_who_to_ask() {
        let r = read("AccessDenied: ... with an explicit deny in a service control policy", "us-east-2");
        assert!(r.fix.contains("service control policy"));
    }

    #[test]
    fn new_role_waits_longer() {
        let r = read("InvalidParameterValueException: The role defined for the function cannot be assumed by Lambda.", "us-east-2");
        assert_eq!(r.wait, SPREAD);
    }

    #[test]
    fn vcpu_points_at_quotas() {
        let r = read("VcpuLimitExceeded: You have requested more vCPU capacity than your current vCPU limit of 1", "eu-west-1");
        assert_eq!(r.link.as_deref(), Some("https://eu-west-1.console.aws.amazon.com/servicequotas/home/services/ec2/quotas"));
    }

    #[test]
    fn unknown_is_still_explained() {
        let r = read("InternalFailure: boom", "us-east-1");
        assert!(r.fix.contains("forkrail install"));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_once_then_reports() {
        let mut calls = 0;
        let out: Result<(), Problem> = twice("Create the bucket", "creating it", "us-east-2", || {
            calls += 1;
            async { Err::<(), _>("AccessDenied: not authorized to perform: s3:CreateBucket on resource") }
        })
        .await;
        assert_eq!(calls, 2);
        let problem = out.unwrap_err();
        assert!(problem.cause.contains("s3:CreateBucket"));
        assert!(problem.to_string().contains("Run `forkrail install` again"));
    }

    #[tokio::test(start_paused = true)]
    async fn second_try_can_succeed() {
        let mut calls = 0;
        let out = twice("Step", "doing it", "us-east-2", || {
            calls += 1;
            let first = calls == 1;
            async move { if first { Err("Throttling: Rate exceeded") } else { Ok(7) } }
        })
        .await;
        assert_eq!(out, Ok(7));
        assert_eq!(calls, 2);
    }
}
