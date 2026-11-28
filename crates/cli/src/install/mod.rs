//! `forkrail install`: forkrail in an AWS account, from nothing, with one command.
//!
//! 1. Who the credentials are, and what the account holds already: an install the installer
//!    started is continued from its record ([`record`]); one set up by hand is left alone.
//! 2. What's needed from the person: the region, the domain, the Cloudflare token, an alert email.
//! 3. The checks ([`checks`]): nothing is changed until they pass.
//! 4. The steps ([`steps`]), each skipped when its record says it's done. Every call is tried
//!    twice before a step stops with a [`problem::Problem`] that says what to do; running the
//!    installer again continues from that step.
//!
//! The person follows it in the terminal.

pub mod boxes;
pub mod checks;
pub mod copier;
pub mod download;
pub mod cloudflare;
pub mod page;
pub mod problem;
pub mod record;
pub mod release;
pub mod stacks;
pub mod steps;
pub mod uninstall;

use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use aws_config::{BehaviorVersion, Region, SdkConfig};
use tokio::sync::mpsc;

use crate::aws_signin;
use checks::{Check, Verdict};
use problem::Problem;
use record::{Found, RECORD_KEY, ROUTES_KEY, Record};

/// The region a new install's state goes in unless another is picked.
pub const DEFAULT_REGION: &str = forkrail_common::install::DEFAULT_HOME_REGION;
/// Accounts the installer refuses to touch, comma-separated (an operator's production account,
/// when testing the installer from the same machine).
pub const NEVER_ENV: &str = "FORKRAIL_INSTALL_NEVER";
/// The Cloudflare token, for a run with no one to ask.
pub const TOKEN_ENV: &str = "FORKRAIL_CLOUDFLARE_TOKEN";
/// What the install's two machines and what comes with them cost a day, before apps' traffic: a
/// t4g.small and a t4g.medium, their disks, two public addresses, two keys (us-east-1 prices).
pub const PRICE_PER_DAY: &str = "about $1.80 a day";

/// The regions offered, with their names: every region a new account has on (no opt-in ones).
pub const REGIONS: &[(&str, &str)] = &[
    ("us-east-1", "US East (N. Virginia)"),
    ("us-east-2", "US East (Ohio)"),
    ("us-west-1", "US West (N. California)"),
    ("us-west-2", "US West (Oregon)"),
    ("ca-central-1", "Canada (Central)"),
    ("sa-east-1", "South America (São Paulo)"),
    ("eu-west-1", "Europe (Ireland)"),
    ("eu-west-2", "Europe (London)"),
    ("eu-west-3", "Europe (Paris)"),
    ("eu-central-1", "Europe (Frankfurt)"),
    ("eu-north-1", "Europe (Stockholm)"),
    ("ap-south-1", "Asia Pacific (Mumbai)"),
    ("ap-southeast-1", "Asia Pacific (Singapore)"),
    ("ap-southeast-2", "Asia Pacific (Sydney)"),
    ("ap-northeast-1", "Asia Pacific (Tokyo)"),
    ("ap-northeast-2", "Asia Pacific (Seoul)"),
    ("ap-northeast-3", "Asia Pacific (Osaka)"),
];

/// A region's name for people ("US East (N. Virginia)"), or its code.
pub fn region_name(code: &str) -> &str {
    REGIONS.iter().find(|(c, _)| *c == code).map_or(code, |(_, name)| name)
}

/// Whether `domain` is a domain the install can take: lowercase labels of letters, digits and
/// hyphens, at least two, the last letters only.
pub fn valid_domain(domain: &str) -> bool {
    let labels: Vec<&str> = domain.split('.').collect();
    domain.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        && labels.last().is_some_and(|tld| tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_lowercase()))
}

/// Who signed in, for people: "the account owner (root)", "user ada", "role Admin".
pub fn who(arn: &str) -> String {
    if arn.ends_with(":root") {
        return "the account owner (root)".to_owned();
    }
    let resource = arn.split(':').nth(5).unwrap_or(arn);
    match resource.split_once('/') {
        Some(("user", rest)) => format!("user {}", rest.rsplit('/').next().unwrap_or(rest)),
        Some(("assumed-role", rest)) => format!("role {}", rest.split('/').next().unwrap_or(rest)),
        Some(("role", rest)) => format!("role {}", rest.rsplit('/').next().unwrap_or(rest)),
        _ => resource.to_owned(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub profile: Option<String>,
    pub region: Option<String>,
    pub domain: Option<String>,
    pub email: Option<String>,
    /// Only look: run the checks and stop.
    pub check_only: bool,
    /// Don't ask before creating anything.
    pub yes: bool,
    /// Ask in the terminal even where a browser could open.
    pub terminal: bool,
    /// The release to install from: a folder with the binaries and packages (`forkrail release`
    /// makes one), uploaded from here. Without it, the account copies the published release for
    /// this CLI's version itself.
    pub release: Option<PathBuf>,
}

/// What happens, as the person sees it: the terminal prints these, the page shows them.
#[derive(Debug, Clone)]
pub enum Event {
    /// A line of what's happening; from a step, when one said it.
    Note { step: Option<&'static str>, text: String },
    /// The AWS account the install goes in, and who signed in to it.
    Account { account: String, arn: String },
    /// The install's GitHub App is made (or was, in an earlier run).
    GitHubReady { name: String, url: String },
    Check(Check),
    /// The steps, in order, by id and title, before the first starts.
    Plan(Vec<(&'static str, &'static str)>),
    Started(&'static str),
    /// Done, already done (skipped), with how long it took.
    Finished { id: &'static str, skipped: bool, secs: u64 },
    Failed(Problem),
    /// A page the person should open, and why.
    Open { url: String, why: String },
    Installed { dashboard: String },
}

/// Where events go. A step's own says which step its notes are from: steps run side by side.
#[derive(Clone)]
pub struct Reporter {
    sender: mpsc::UnboundedSender<Event>,
    step: Option<&'static str>,
}

impl Reporter {
    pub fn new(sender: mpsc::UnboundedSender<Event>) -> Self {
        Self { sender, step: None }
    }
    /// This reporter, for step `id`'s notes.
    pub fn for_step(&self, id: &'static str) -> Self {
        Self { sender: self.sender.clone(), step: Some(id) }
    }
    pub fn send(&self, event: Event) {
        let _ = self.sender.send(event);
    }
    pub fn note(&self, text: impl Into<String>) {
        self.send(Event::Note { step: self.step, text: text.into() });
    }
}

/// The SDK's settings for `region`, with the person's profile.
pub async fn aws(profile: Option<&str>, region: &str) -> SdkConfig {
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region.to_owned()));
    if let Some(profile) = profile {
        loader = loader.profile_name(profile);
    }
    // A browser sign-in is refreshed where it began, whatever region the install is in.
    if let Some(provider) = crate::aws_signin::pinned(profile) {
        loader = loader.credentials_provider(provider);
    }
    loader.load().await
}

/// Where the installer keeps the GitHub App it made before the steps, until the last step stores
/// its keys in the account: a file only its user can read, deleted once they're stored.
pub fn github_file(account: &str) -> Option<PathBuf> {
    crate::config::path().and_then(|p| p.parent().map(|dir| dir.join(format!("install-github-{account}.json"))))
}

/// What the account holds: an install bucket, and in it the installer's record or a routing table.
/// Bucket names are global and this one is easy to guess, so it's read only as this account's
/// (`expected_bucket_owner`): a bucket of the same name in another account, however its policy
/// lets this one in, is refused, and with it any record planted there.
pub async fn find(config: &SdkConfig, account: &str) -> Result<(Found, Option<String>), String> {
    let bucket = forkrail_common::install::default_bucket(account);
    let s3 = aws_sdk_s3::Client::new(config);
    let not_ours = |text: String| {
        if text.contains("AccessDenied") || text.contains("Forbidden") {
            format!("s3://{bucket} exists but isn't this AWS account's ({account}): another account owns that name, so the install can't use it. ({text})")
        } else {
            text
        }
    };
    let region = match s3.get_bucket_location().bucket(&bucket).expected_bucket_owner(account).send().await {
        Ok(out) => match out.location_constraint().map(|c| c.as_str().to_owned()) {
            None => DEFAULT_REGION.to_owned(),
            Some(c) if c.is_empty() => DEFAULT_REGION.to_owned(),
            Some(c) if c == "EU" => "eu-west-1".to_owned(),
            Some(c) => c,
        },
        Err(e) => {
            let text = problem::aws_text(&e);
            if text.contains("NoSuchBucket") || text.contains("NotFound") {
                return Ok((Found::Nothing, None));
            }
            return Err(not_ours(text));
        }
    };
    let s3 = aws_sdk_s3::Client::new(&config.to_builder().region(Region::new(region.clone())).build());
    match s3.get_object().bucket(&bucket).key(RECORD_KEY).expected_bucket_owner(account).send().await {
        Ok(out) => {
            let bytes = out.body.collect().await.map_err(|e| e.to_string())?.into_bytes();
            let record: Record = serde_json::from_slice(&bytes).map_err(|e| format!("{RECORD_KEY}: {e}"))?;
            Ok((Found::Ours(Box::new(record)), Some(region)))
        }
        Err(e) if problem::aws_text(&e).contains("NoSuchKey") => match s3.head_object().bucket(&bucket).key(ROUTES_KEY).expected_bucket_owner(account).send().await {
            Ok(_) => Ok((Found::ByHand, Some(region))),
            // The bucket alone: an install that stopped right after its first step.
            Err(_) => Ok((Found::Nothing, Some(region))),
        },
        Err(e) => Err(not_ours(problem::aws_text(&e))),
    }
}

/// Everything the steps need.
/// What the steps work with. Steps that don't need each other run at once, each with a copy of
/// this taken when it starts (the record as it was then: its own needs are in it).
#[derive(Clone)]
pub struct Cx {
    /// In the home region.
    pub config: SdkConfig,
    pub profile: Option<String>,
    pub caller: checks::Caller,
    pub record: Record,
    pub token: Option<String>,
    pub release: Option<PathBuf>,
    pub report: Reporter,
    /// The Lambda concurrency the account had at the checks.
    pub concurrency: Option<i32>,
    /// The most memory the account lets a function have, when it's limited: from the checks, or
    /// from a refusal during the steps. Shared by the steps running at once.
    pub memory_cap: std::rc::Rc<std::cell::Cell<Option<u32>>>,
}

impl Cx {
    pub fn region(&self) -> &str {
        &self.record.home_region
    }
    pub fn account(&self) -> &str {
        &self.caller.account
    }
    pub fn bucket(&self) -> String {
        forkrail_common::install::default_bucket(&self.caller.account)
    }
    /// The SDK's settings for another region, with the same credentials.
    pub fn config_in(&self, region: &str) -> SdkConfig {
        self.config.to_builder().region(Region::new(region.to_owned())).build()
    }
}

pub fn now_ms() -> u64 {
    crate::state::now_ms()
}

/// `forkrail install`.
pub async fn install(options: Options) -> Result<()> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let report = Reporter::new(sender);
    // A page in the browser where there is one to open; the terminal otherwise (and as well).
    let page = if options.terminal || options.yes || options.check_only || !page::desktop() { None } else { page::Page::open().await };
    if let Some(page) = &page
        && !page::in_window()
    {
        eprintln!("The installer's page is open in your browser: {}", page.url);
    }
    let printer = tokio::spawn(print(receiver, page.clone()));
    let result = run(options, report, page.as_ref()).await;
    // The printer ends once every sender is gone, and every event is the page's then.
    let _ = printer.await;
    if let Some(page) = &page {
        page.finish();
        // The page shows the end before the installer goes: a page that isn't in front may look
        // for news only now and then (browsers and macOS slow hidden pages down), and one that
        // looks after the installer has gone shows it stopped halfway. In the Mac app's window
        // it may wait long; the app knows it can close (page::ENDED_LINE).
        let longest = if page::in_window() { std::time::Duration::from_secs(3600) } else { std::time::Duration::from_secs(60) };
        page.shown(longest).await;
    }
    result
}

async fn run(options: Options, report: Reporter, page: Option<&page::Page>) -> Result<()> {
    let probe_region = options.region.clone().unwrap_or_else(|| DEFAULT_REGION.to_owned());
    let never: Vec<String> = std::env::var(NEVER_ENV).unwrap_or_default().split(',').map(|a| a.trim().to_owned()).filter(|a| !a.is_empty()).collect();
    let mut profile = options.profile.clone();
    let mut probe = aws(profile.as_deref(), &probe_region).await;
    let failed = |said: String| {
        let reading = problem::read(&said, &probe_region);
        report.send(Event::Failed(Problem {
            step: "Sign-in".into(),
            action: "asking AWS who these credentials are".into(),
            said,
            cause: reading.cause,
            fix: reading.fix,
            link: reading.link,
        }));
    };
    let caller = match checks::caller(&probe).await {
        Ok(caller) => caller,
        // No AWS sign-in on this computer, or an old one: the installer signs in itself, in the
        // browser, when someone is there to.
        Err(said)
            if problem::signed_out(&said)
                && !options.yes
                && (page.is_some() || std::io::stdin().is_terminal())
                // Keys in the environment come first; signing in wouldn't change what's used.
                && std::env::var_os("AWS_ACCESS_KEY_ID").is_none_or(|v| v.is_empty()) =>
        {
            report.note("Signing in to AWS…");
            // With the page, AWS's sign-in opens when the person clicks for it, not before.
            let how = if page.is_some() {
                aws_signin::How::Button
            } else if page::desktop() {
                aws_signin::How::Browser
            } else {
                aws_signin::How::Paste
            };
            let show = |url: &str, why: &str| report.send(Event::Open { url: url.to_owned(), why: why.to_owned() });
            let signed = match aws_signin::sign_in(&probe_region, profile.as_deref(), how, &never, &show).await {
                Ok(signed) => signed,
                Err(e) => {
                    failed(format!("{e:#}"));
                    bail!("not signed in to AWS");
                }
            };
            report.note(format!("Signed in to AWS account {}, saved as AWS profile \"{}\".", signed.account, signed.profile));
            profile = Some(signed.profile);
            probe = aws(profile.as_deref(), &probe_region).await;
            match checks::caller(&probe).await {
                Ok(caller) => caller,
                Err(said) => {
                    failed(said);
                    bail!("signed in, but AWS doesn't take the sign-in");
                }
            }
        }
        Err(said) => {
            failed(said);
            bail!("no AWS credentials");
        }
    };
    if never.contains(&caller.account) {
        bail!("account {} is listed in {NEVER_ENV}: the installer won't touch it", caller.account);
    }
    report.send(Event::Account { account: caller.account.clone(), arn: caller.arn.clone() });

    let (found, found_region) = find(&probe, &caller.account).await.map_err(|said| anyhow::anyhow!("looking for an install in the account: {said}"))?;
    let mut record = match found {
        Found::ByHand => {
            report.note(format!(
                "forkrail already runs in account {} (bucket {} in {}), set up before the installer existed. The installer leaves it alone; use deploy/aws/release.sh to update it.",
                caller.account,
                forkrail_common::install::default_bucket(&caller.account),
                found_region.unwrap_or_default()
            ));
            return Ok(());
        }
        Found::Ours(record) => {
            if record.finished_ms.is_some() {
                report.note(format!("forkrail is installed in this account already: {}", record.dashboard_url()));
            } else {
                report.note(format!("continuing the install started here for {} in {}", record.domain, record.home_region));
            }
            if let Some(region) = &options.region
                && region != &record.home_region
            {
                bail!("this account's install is in {}, not {region}: an account holds one install", record.home_region);
            }
            *record
        }
        Found::Nothing => {
            if let Some(region) = found_region {
                // The bucket alone, from a run that stopped after it: that region it is.
                Record { home_region: region, ..Default::default() }
            } else {
                Record::default()
            }
        }
    };
    record.account.clone_from(&caller.account);
    record.forkrail = env!("CARGO_PKG_VERSION").to_owned();

    // What's needed from the person: on the page, or in the terminal.
    let mut given_token = None;
    let mut confirmed = false;
    // A started install the person moved to another region or domain: removed once the checks pass.
    let mut previous: Option<Record> = None;
    // A new alert email for an install whose alerts topic is made already.
    let mut new_email = None;
    if let Some(page) = page {
        let finished = record.finished_ms.is_some();
        let started = !record.home_region.is_empty();
        let questions = page::Questions {
            region: if record.home_region.is_empty() { options.region.clone().unwrap_or_else(|| DEFAULT_REGION.to_owned()) } else { record.home_region.clone() },
            regions: REGIONS.iter().map(|(code, name)| page::Region { code: (*code).to_owned(), name: (*name).to_owned() }).collect(),
            price: PRICE_PER_DAY.to_owned(),
            domain: if record.domain.is_empty() { options.domain.clone().unwrap_or_default() } else { record.domain.clone() },
            email: record.email.clone().or_else(|| options.email.clone()).unwrap_or_default(),
            token: !record.done("secrets") && std::env::var(TOKEN_ENV).map_or(true, |t| t.trim().is_empty()),
            token_saved: record.done("secrets"),
            token_page: cloudflare::TOKEN_PAGE.to_owned(),
            started,
            fixed: finished,
        };
        let answers = page.ask(questions.clone()).await.context("the page closed before it was answered")?;
        if answers.moves(&questions) {
            let fresh = Record { account: record.account.clone(), forkrail: record.forkrail.clone(), ..Default::default() };
            previous = Some(std::mem::replace(&mut record, fresh));
        }
        if record.home_region.is_empty() {
            record.home_region = answers.region.trim().to_owned();
        }
        if record.domain.is_empty() {
            record.domain = answers.domain();
        }
        let email = answers.email.trim();
        if !email.is_empty() && record.email.as_deref() != Some(email) {
            // The budget takes it when its step runs again; the alerts topic, once the checks pass.
            record.steps.remove("protection");
            if record.done("services") {
                new_email = Some(email.to_owned());
            }
            record.email = Some(email.to_owned());
        }
        given_token = Some(answers.token).filter(|t| !t.trim().is_empty());
        if given_token.is_some() {
            // Stored again, over the earlier one.
            record.steps.remove("secrets");
        } else if let Some(previous) = &previous {
            // Starting over keeps the token the earlier run stored, if it reaches the new domain
            // (the checks say).
            given_token = uninstall::saved_token(&aws(profile.as_deref(), &previous.home_region).await).await;
        }
        confirmed = true;
    }
    let asking = page.is_none() && std::io::stdin().is_terminal() && !options.yes;
    if record.home_region.is_empty() {
        record.home_region = match &options.region {
            Some(region) => region.clone(),
            None if asking => loop {
                let region = ask("AWS region for the install (us-east-1 is US East, N. Virginia)", Some(DEFAULT_REGION))?;
                if REGIONS.iter().any(|(code, _)| *code == region) {
                    break region;
                }
                eprintln!("  Pick one of: {}", REGIONS.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(", "));
            },
            None => DEFAULT_REGION.to_owned(),
        };
    }
    if record.domain.is_empty() {
        record.domain = match &options.domain {
            Some(domain) => domain.clone(),
            None if asking => loop {
                let domain = ask("Domain for your apps (they'll be <name>.DOMAIN, the dashboard dashboard.DOMAIN)", None)?.trim().trim_end_matches('.').to_ascii_lowercase();
                if valid_domain(&domain) {
                    break domain;
                }
                eprintln!("  That isn't a domain: give one like example.com");
            },
            None => bail!("--domain is needed"),
        }
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
        if !valid_domain(&record.domain) {
            bail!("{} isn't a domain: give one like example.com", record.domain);
        }
    }
    if record.email.is_none() {
        record.email = match &options.email {
            Some(email) => Some(email.clone()),
            None if asking => Some(ask("Email for alerts (budget, certificates); empty for none", Some(""))?).filter(|e| !e.is_empty()),
            None => None,
        };
    }
    let config = aws(profile.as_deref(), &record.home_region).await;
    let token = match given_token.or_else(|| std::env::var(TOKEN_ENV).ok().filter(|t| !t.trim().is_empty())) {
        Some(token) => Some(token),
        // Kept in SSM by an earlier run.
        None if record.done("secrets") => None,
        None if asking => {
            let on_cloudflare = ask(&format!("Is {}'s DNS on Cloudflare? [Y/n]", record.domain), Some("y"))?;
            if on_cloudflare.eq_ignore_ascii_case("n") || on_cloudflare.eq_ignore_ascii_case("no") {
                eprintln!("\n{}\n", cloudflare::MOVE_DNS);
                bail!("nothing was changed: move the domain's DNS to Cloudflare, then run `forkrail install` again");
            }
            report.note(format!("Make a Cloudflare API token here (everything is filled in: click Continue to summary, then Create Token): {}", cloudflare::TOKEN_PAGE));
            Some(rpassword::prompt_password("Paste the Cloudflare API token (it isn't shown): ").context("reading the token")?)
        }
        None => bail!("the Cloudflare token is needed: set {TOKEN_ENV}"),
    };

    // The checks: nothing changes until they pass.
    let mut checks = vec![checks::permissions(&config, &caller).await, checks::region(&config, &record.home_region).await, checks::not_uninstalling(&config).await];
    let (concurrency_check, concurrency) = checks::lambda_concurrency(&config).await;
    checks.push(concurrency_check);
    if !record.done("services") {
        let (memory_check, cap) = checks::function_memory(&config, &caller.account).await;
        checks.push(memory_check);
        record.memory_cap_mb = cap;
    }
    if !record.done("router") {
        checks.push(checks::machines(&config, &record.home_region).await);
        checks.push(checks::addresses(&config, &record.home_region).await);
    }
    if let Some(token) = &token {
        let (check, zone) = checks::domain(token, &record.domain).await;
        checks.push(check);
        if let Some(zone) = zone {
            record.zone_id = zone.id;
            record.zone_name = zone.name;
        }
    }
    let failed = checks.iter().filter(|c| c.verdict == Verdict::Fail).count();
    for check in checks {
        report.send(Event::Check(check));
    }
    if failed > 0 {
        report.note(format!("{failed} check{} failed, so nothing was changed. Fix what's marked, then run `forkrail install` again.", if failed == 1 { "" } else { "s" }));
        bail!("{failed} check{} failed: nothing was changed. Fix {} and run `forkrail install` again.", if failed == 1 { "" } else { "s" }, if failed == 1 { "it" } else { "them" });
    }
    if options.check_only {
        report.note("Every check passed. Nothing was changed (--check).");
        return Ok(());
    }
    if record.finished_ms.is_some() && steps::ALL.iter().all(|(id, _)| record.done(id)) {
        report.note(format!("Nothing to do. The dashboard: {}", record.dashboard_url()));
        return Ok(());
    }
    if asking && !confirmed && !record.done("bucket") {
        let answer = ask(
            &format!(
                "Install forkrail in account {} ({}) for {}? Its two small servers cost {PRICE_PER_DAY} on your AWS bill, plus what your apps use. [y/N]",
                caller.account,
                region_name(&record.home_region),
                record.domain
            ),
            Some("n"),
        )?;
        if !answer.eq_ignore_ascii_case("y") && !answer.eq_ignore_ascii_case("yes") {
            bail!("nothing was changed");
        }
    }
    if let Some(previous) = &previous {
        let what = if previous.domain.is_empty() { String::new() } else { format!(" for {}", previous.domain) };
        report.note(format!("Starting over: first removing what the earlier run made{what} in {}.", region_name(&previous.home_region)));
        let old = aws(profile.as_deref(), &previous.home_region).await;
        let say = |line: String| report.note(line);
        let (left, app_page) = uninstall::remove(&old, &caller, previous, &say).await;
        if !left.is_empty() {
            report.send(Event::Failed(Problem {
                step: "Start over".into(),
                action: "removing what the earlier run made".into(),
                said: left.join("\n"),
                cause: "Some of what the earlier run made couldn't be removed yet.".into(),
                fix: "Run `forkrail install` again with the same answers: it removes what's left first, then installs.".into(),
                link: None,
            }));
            bail!("the earlier run's install isn't all removed yet");
        }
        if let Some(page) = app_page {
            report.note(format!("The earlier run's GitHub App stays on GitHub, where only you can delete it: {page}/advanced"));
        }
    }
    if record.started_ms == 0 {
        record.started_ms = now_ms();
    }
    // The GitHub App, made now: the one click the install needs, so the steps then run unattended.
    if !options.yes && (page.is_some() || asking) && !record.done("github") {
        make_github_app(&record, &caller.account, &report, page.is_some()).await;
    }

    let memory_cap = std::rc::Rc::new(std::cell::Cell::new(record.memory_cap_mb));
    let mut cx = Cx { config, profile, caller, record, token, release: options.release.clone(), report: report.clone(), concurrency, memory_cap };
    if let Some(email) = &new_email
        && let Err(problem) = steps::subscribe_alerts(&cx, email).await
    {
        report.send(Event::Failed(problem));
        bail!("the install stopped; run `forkrail install` again to continue")
    }
    let started = Instant::now();
    match steps::run(&mut cx).await {
        Ok(()) => {
            report.send(Event::Installed { dashboard: cx.record.dashboard_url() });
            report.note(format!("Installed in {} min.", started.elapsed().as_secs().div_ceil(60)));
            Ok(())
        }
        // The steps have shown it already.
        Err(_) => {
            bail!("the install stopped; run `forkrail install` again to continue")
        }
    }
}

/// Makes the install's GitHub App on GitHub (the person clicks Create) and keeps it in
/// [`github_file`] for the last step. Not made, the last step asks again.
/// With the setup page (`on_page`), GitHub opens only from its button.
async fn make_github_app(record: &Record, account: &str, report: &Reporter, on_page: bool) {
    let Some(file) = github_file(account) else { return };
    if file.is_file() {
        let made = std::fs::read_to_string(&file).ok().and_then(|text| serde_json::from_str::<crate::github::Made>(&text).ok());
        if let Some(made) = made {
            report.send(Event::GitHubReady { name: made.name().to_owned(), url: made.html_url().to_owned() });
        }
        report.note("The GitHub App was made in an earlier run; its keys are stored at the last step.");
        return;
    }
    let show = |local: &str| {
        report.send(Event::Open {
            url: local.to_owned(),
            why: if on_page {
                "One click on GitHub. The button opens GitHub with forkrail's GitHub App filled in: click \"Create GitHub App\" there. It's how you'll sign in to your dashboard, and everything after it runs on its own."
            } else {
                "One click on GitHub: a tab opens with forkrail's GitHub App filled in. Click \"Create GitHub App\" there. It's how you'll sign in to your dashboard, and everything after it runs on its own."
            }
            .into(),
        });
    };
    let setup = crate::github::Setup { opened_by_page: on_page, ..Default::default() };
    let made = match crate::github::make(&record.dashboard_url(), &record.domain, &setup, &show).await {
        Ok(made) => made,
        Err(e) => {
            report.note(format!("The GitHub App wasn't made ({e:#}). The install goes on, and its last step asks again."));
            return;
        }
    };
    let saved = serde_json::to_string(&made).map_err(anyhow::Error::from).and_then(|text| aws_signin::write_private(&file, &text));
    match saved {
        Ok(()) => {
            report.send(Event::GitHubReady { name: made.name().to_owned(), url: made.html_url().to_owned() });
            report.note(format!("GitHub App \"{}\" made: {}. Its keys go into your account at the last step.", made.name(), made.html_url()));
        }
        Err(e) => report.note(format!("The GitHub App {} was made, but couldn't be kept here ({e:#}). Delete it on GitHub; the last step makes another.", made.html_url())),
    }
}

/// One line from the person, `default` when they just press Enter.
fn ask(question: &str, default: Option<&str>) -> Result<String> {
    let mut out = std::io::stderr();
    match default {
        Some(d) if !d.is_empty() && d != "n" => write!(out, "{question} [{d}]: ")?,
        _ => write!(out, "{question}: ")?,
    }
    out.flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).context("reading the answer")?;
    let line = line.trim();
    Ok(if line.is_empty() { default.unwrap_or_default().to_owned() } else { line.to_owned() })
}

/// The terminal's view, and the page's when there is one.
async fn print(mut events: mpsc::UnboundedReceiver<Event>, page: Option<page::Page>) {
    let mut titles: Vec<(&'static str, &'static str)> = Vec::new();
    let title = |titles: &[(&'static str, &'static str)], id: &str| titles.iter().find(|(i, _)| *i == id).map_or(id.to_owned(), |(_, t)| (*t).to_owned());
    while let Some(event) = events.recv().await {
        if let Some(page) = &page {
            page.event(&event);
        }
        match event {
            Event::Note { text, .. } => eprintln!("{text}"),
            Event::Account { account, arn } => eprintln!("✓  AWS account {account}, signed in as {}", who(&arn)),
            Event::GitHubReady { name, url } => eprintln!("✓  GitHub App \"{name}\": {url}"),
            Event::Check(check) => eprintln!("{check}"),
            Event::Plan(steps) => {
                eprintln!("\nSetting up forkrail: {} steps, about 15 minutes. Nothing more is needed from you.", steps.len());
                titles = steps;
            }
            Event::Started(id) => {
                eprintln!("  …  {}", title(&titles, id));
                let about = steps::about(id);
                if !about.is_empty() {
                    eprintln!("     {about}");
                }
            }
            Event::Finished { id, skipped: true, .. } => eprintln!("  ✓  {} (done before)", title(&titles, id)),
            Event::Finished { id, secs, .. } => eprintln!("  ✓  {} ({secs} s)", title(&titles, id)),
            Event::Failed(problem) => eprintln!("\n{problem}"),
            Event::Open { url, why } => eprintln!("\n{why}\n  {url}\n"),
            Event::Installed { dashboard } => eprintln!("\nforkrail is installed. Open {dashboard} and sign in with GitHub."),
        }
    }
}

/// Shared with the page's server.
pub type Shared<T> = Arc<tokio::sync::Mutex<T>>;
