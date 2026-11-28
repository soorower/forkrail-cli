//! `forkrail uninstall`: everything `forkrail install` made in the account, gone, so the account is
//! as it was before. Only an install the installer made (its record is in the bucket) is
//! uninstalled; one set up by hand is refused, as the installer refuses it.
//!
//! What goes, in order: the domain's DNS records the installer made; the apps' functions, their
//! schedules and logs; forkrail's stacks, newest first; then what the stacks keep when deleted
//! (the tables, the keys, the builder's images, the audit bucket) and what was made outside them
//! (the bucket and everything in it, the secrets in SSM, the alerts topic, log groups, the apps'
//! env layers). Then every other region apps can run in: functions moved there, their code bucket
//! (`<bucket>-<region>`), env layers, schedules and logs, and stacks made there (a router, the env
//! key). KMS keys can't be deleted at once: AWS deletes them after 7 days, and keys waiting for
//! that cost nothing.
//!
//! Each piece is tried twice; one that still fails is listed at the end, and running the command
//! again picks up what's left.
//!
//! [`remove`] is the same removal for the installer, when someone changes the domain or region of
//! an install that hasn't finished: it starts over.

use anyhow::{Result, bail};
use aws_config::SdkConfig;
use forkrail_common::install::{TAG_KEY, TAG_VALUE};

use super::cloudflare::{COMMENT, Cloudflare};
use super::problem::{aws_text, twice};
use super::record::{Found, Record};
use super::{DEFAULT_REGION, NEVER_ENV, aws, checks, find, stacks};

/// forkrail's stacks in the home region, deleted a wave at a time, each wave's side by side. The
/// stacks pass nothing to each other through exports, so CloudFormation leaves the order to us;
/// what matters is what's still in use:
/// 1. the machines and the functions (each stack's own);
/// 2. the network (the machines' security groups and addresses), and the control function, which
///    the builder, the guard, the retention and the schedules stacks point at;
/// 3. the roles everything above ran as, and the stacks of the keys (which keep their keys).
const STACK_WAVES: &[&[&str]] = &[
    &["install-router", "install-analytics", "guard", "screenshot", "image", "builder", "retention", "schedules", "web", "state", "notify", "registry", "install-fetch"],
    &["install-network", "control"],
    &["install-roles", "install-secrets", "secrets"],
];
/// The ones AWS keeps in us-east-1, whatever the install's region.
const BILLING_STACKS: &[&str] = &["spend-backstop", "cost-tags"];
const ALIASES: &[&str] = &["alias/forkrail-env", "alias/forkrail-install", "alias/forkrail-state", "alias/forkrail-notify", "alias/forkrail-web"];
const TABLES: &[&str] = &[forkrail_common::install::STATE_TABLE, forkrail_common::install::WEB_TABLE];
/// The apps' env layers, `forkrail-env-<project>` (`forkrail_common::envfile::layer_name`).
const ENV_LAYER_PREFIX: &str = "forkrail-env-";

pub struct Options {
    pub profile: Option<String>,
    pub region: Option<String>,
    pub yes: bool,
}

/// A line for the person: the terminal's, or the installer's notes.
pub type Say<'a> = &'a (dyn Fn(String) + Sync);

/// What failed, to list at the end.
/// What couldn't be removed; each piece is said as it ends, side by side as they run.
struct Left<'a> {
    left: std::cell::RefCell<Vec<String>>,
    say: Say<'a>,
}

impl Left<'_> {
    fn note(&self, what: &str, result: Result<(), String>) {
        match result {
            Ok(()) => (self.say)(format!("  ✓  {what}")),
            Err(e) => {
                (self.say)(format!("  ✗  {what}: {}", e.lines().next().unwrap_or_default()));
                self.left.borrow_mut().push(format!("{what}: {e}"));
            }
        }
    }
}

/// The Cloudflare token an install keeps in SSM, if it's there.
pub async fn saved_token(config: &SdkConfig) -> Option<String> {
    aws_sdk_ssm::Client::new(config)
        .get_parameter()
        .name(forkrail_common::install::CLOUDFLARE_TOKEN_PARAM)
        .with_decryption(true)
        .send()
        .await
        .ok()
        .and_then(|o| o.parameter().and_then(|p| p.value()).map(str::to_owned))
}

/// Runs `call` twice at most, as the installer does, and reads the last error to text.
async fn try_twice<F, Fut>(what: &str, region: &str, call: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    twice("Uninstall", what, region, call).await.map_err(|p| format!("{}\n    {}", p.said, p.fix))
}

pub async fn uninstall(options: Options) -> Result<()> {
    let probe_region = options.region.clone().unwrap_or_else(|| DEFAULT_REGION.to_owned());
    let probe = aws(options.profile.as_deref(), &probe_region).await;
    let caller = checks::caller(&probe).await.map_err(|e| anyhow::anyhow!("asking AWS who these credentials are: {e}"))?;
    if std::env::var(NEVER_ENV).unwrap_or_default().split(',').any(|a| a.trim() == caller.account) {
        bail!("account {} is listed in {NEVER_ENV}: nothing is touched", caller.account);
    }
    let (found, _) = find(&probe, &caller.account).await.map_err(|e| anyhow::anyhow!("looking for the install: {e}"))?;
    let record: Record = match found {
        Found::Ours(record) => *record,
        Found::ByHand => bail!("forkrail in account {} was set up without the installer: `forkrail uninstall` leaves it alone", caller.account),
        Found::Nothing => bail!("no install made by `forkrail install` in account {}: nothing to remove", caller.account),
    };
    let region = record.home_region.clone();
    let config = aws(options.profile.as_deref(), &region).await;
    eprintln!("This deletes forkrail from AWS account {} ({region}): the router, the analytics box, every app and its data, the dashboard, and the install's bucket.", caller.account);
    if !options.yes {
        eprint!("Type the domain ({}) to go on: ", record.domain);
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != record.domain {
            bail!("nothing was deleted");
        }
    }
    let say = |line: String| eprintln!("{line}");
    let (left, app_page) = remove(&config, &caller, &record, &say).await;
    if let Some(page) = app_page {
        eprintln!("\nThe GitHub App stays on GitHub, where only you can delete it: {page}/advanced (Delete GitHub App).");
    }
    if left.is_empty() {
        eprintln!("\nforkrail is uninstalled from account {}.", caller.account);
        Ok(())
    } else {
        eprintln!("\nThese are left (run `forkrail uninstall` again to retry them):");
        for line in &left {
            eprintln!("  - {line}");
        }
        bail!("{} piece{} left", left.len(), if left.len() == 1 { "" } else { "s" })
    }
}

/// Everything the install `record` describes, removed from the account, a line to `say` for each
/// piece. `config` is for the install's region. Returns what's left (empty when all of it went)
/// and the page of the GitHub App the install made, which only its owner can delete.
pub async fn remove(config: &SdkConfig, caller: &checks::Caller, record: &Record, say: Say<'_>) -> (Vec<String>, Option<String>) {
    let region = record.home_region.clone();
    let left = Left { left: std::cell::RefCell::new(Vec::new()), say };
    let bucket = forkrail_common::install::default_bucket(&caller.account);

    // The GitHub App's page, read before its record goes with SSM.
    let ssm = aws_sdk_ssm::Client::new(config);
    let app_page = ssm
        .get_parameter()
        .name(forkrail_common::install::GITHUB_APP_PARAM)
        .send()
        .await
        .ok()
        .and_then(|o| o.parameter().and_then(|p| p.value()).map(str::to_owned))
        .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
        .and_then(|v| v["html_url"].as_str().map(str::to_owned));

    // Side by side wherever nothing depends on the order: the home region in phases, and beside
    // it the billing stacks in us-east-1 and every other region.
    let home = async {
        // The DNS record (while the token is still in SSM): only the one the installer made for
        // this install (`*.<domain>`, steps.rs `domain`), never another install's in the same zone.
        // Then the apps' functions and schedules.
        let dns = async {
            if record.zone_id.is_empty() {
                return;
            }
            let result = match saved_token(config).await {
                None => Err(format!("the Cloudflare token isn't in SSM any more: remove *.{} (commented \"forkrail install\") by hand", record.domain)),
                Some(token) => async {
                    let cf = Cloudflare::new(&token).map_err(|e| e.to_string())?;
                    for r in cf.records(&record.zone_id, &format!("*.{}", record.domain)).await.map_err(|e| format!("{e:#}"))? {
                        if r.comment.as_deref() == Some(COMMENT) {
                            cf.delete(&record.zone_id, &r.id).await.map_err(|e| format!("{e:#}"))?;
                        }
                    }
                    Ok(())
                }
                .await,
            };
            left.note(&format!("DNS records for {} in Cloudflare", record.domain), result);
        };
        let functions = async { left.note("the apps' functions", try_twice("deleting the apps' functions", &region, || apps(config)).await) };
        let schedules = async { left.note("the apps' schedules", try_twice("deleting the apps' schedules", &region, || schedule_groups(config)).await) };
        tokio::join!(dns, functions, schedules);

        // The stacks. The bucket is emptied before them: some resources (the image optimizer's)
        // write into it.
        left.note(&format!("the bucket {bucket}"), try_twice("emptying the bucket", &region, || empty_bucket(config, &bucket)).await);
        (left.say)(format!("  …  deleting forkrail's {} stacks, side by side where they don't need each other (a few minutes)", STACK_WAVES.iter().map(|w| w.len()).sum::<usize>()));
        for wave in STACK_WAVES {
            let deletes = wave.iter().map(|name| {
                let (left, region) = (&left, &region);
                async move {
                    let what = format!("stack forkrail-{name}");
                    left.note(&what, try_twice(&format!("deleting {what}"), region, || stacks::delete(config, name)).await);
                }
            });
            futures_util::future::join_all(deletes).await;
        }

        // What the stacks keep.
        let audit = format!("forkrail-audit-{}", caller.account);
        let tables = TABLES.iter().map(|table| {
            let (left, region) = (&left, &region);
            async move { left.note(&format!("table {table}"), try_twice(&format!("deleting table {table}"), region, || delete_table(config, table)).await) }
        });
        tokio::join!(
            futures_util::future::join_all(tables),
            async { left.note("the builder's images (ECR forkrail-builder)", try_twice("deleting the builder's images", &region, || delete_repository(config)).await) },
            async { left.note(&format!("the audit bucket {audit}"), try_twice("deleting the audit bucket", &region, || delete_bucket(config, &audit)).await) },
            async { left.note(&format!("the bucket {bucket}"), try_twice("deleting the bucket", &region, || delete_bucket(config, &bucket)).await) },
            async { left.note("forkrail's secrets in SSM (/forkrail/)", try_twice("deleting the secrets", &region, || delete_parameters(config)).await) },
            async { left.note("the alerts topic", try_twice("deleting the alerts topic", &region, || delete_topic(config, caller)).await) },
            async { left.note("forkrail's log groups", try_twice("deleting log groups", &region, || delete_log_groups(config)).await) },
            async { left.note("the encryption keys (AWS deletes them in 7 days)", try_twice("scheduling the keys' deletion", &region, || delete_keys(config)).await) },
            async { left.note("the apps' env layers", try_twice("deleting the env layers", &region, || delete_layers(config)).await) },
        );
    };

    let billing = async {
        let billing = config.to_builder().region(aws_config::Region::new("us-east-1")).build();
        let deletes = BILLING_STACKS.iter().map(|name| {
            let (left, billing) = (&left, &billing);
            async move {
                let what = format!("stack forkrail-{name} (us-east-1)");
                left.note(&what, try_twice(&format!("deleting {what}"), "us-east-1", || stacks::delete(billing, name)).await);
            }
        });
        futures_util::future::join_all(deletes).await;
    };

    // The other regions: only those with something of forkrail's in them are named.
    let others = forkrail_common::regions::FUNCTION_REGIONS.iter().map(|r| r.aws).filter(|r| *r != region).map(|other| {
        let (left, bucket) = (&left, &bucket);
        async move {
            let here = config.to_builder().region(aws_config::Region::new(other)).build();
            let code_bucket = format!("{bucket}-{other}");
            match found_in(&here, &code_bucket).await {
                Ok(found) if found.is_empty() => {}
                Ok(found) => {
                    (left.say)(format!("  ·  {other}: {}", found.join(", ")));
                    tokio::join!(
                        async { left.note(&format!("{other}: the apps' functions"), try_twice("deleting the apps' functions", other, || apps(&here)).await) },
                        async { left.note(&format!("{other}: the apps' schedules"), try_twice("deleting the apps' schedules", other, || schedule_groups(&here)).await) },
                        async { left.note(&format!("{other}: the code bucket {code_bucket}"), try_twice("deleting the code bucket", other, || delete_bucket(&here, &code_bucket)).await) },
                    );
                    let names = stacks_in(&here).await.unwrap_or_default();
                    let deletes = names.iter().map(|name| {
                        let here = &here;
                        async move {
                            let what = format!("{other}: stack forkrail-{name}");
                            left.note(&what, try_twice(&format!("deleting {what}"), other, || stacks::delete(here, name)).await);
                        }
                    });
                    futures_util::future::join_all(deletes).await;
                    tokio::join!(
                        async { left.note(&format!("{other}: forkrail's log groups"), try_twice("deleting log groups", other, || delete_log_groups(&here)).await) },
                        async { left.note(&format!("{other}: the apps' env layers"), try_twice("deleting the env layers", other, || delete_layers(&here)).await) },
                        async { left.note(&format!("{other}: the encryption keys (AWS deletes them in 7 days)"), try_twice("scheduling the keys' deletion", other, || delete_keys(&here)).await) },
                    );
                }
                Err(e) => left.note(&format!("{other}: looking for what's there"), Err(e)),
            }
        }
    });
    tokio::join!(home, billing, futures_util::future::join_all(others));

    // An app the installer made up front but never got to store.
    let pending = super::github_file(&caller.account).filter(|f| f.is_file());
    let pending_page = pending
        .as_ref()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|text| serde_json::from_str::<crate::github::Made>(&text).ok())
        .map(|made| made.html_url().to_owned());
    if let Some(file) = &pending {
        let _ = std::fs::remove_file(file);
    }
    (left.left.into_inner(), app_page.or(pending_page))
}

async fn apps(config: &SdkConfig) -> Result<(), String> {
    let lambda = aws_sdk_lambda::Client::new(config);
    let mut marker = None;
    loop {
        let page = lambda.list_functions().set_marker(marker).send().await.map_err(|e| aws_text(&e))?;
        for f in page.functions() {
            let (Some(name), Some(arn)) = (f.function_name(), f.function_arn()) else { continue };
            if !name.starts_with(forkrail_common::install::FUNCTION_PREFIX) {
                continue;
            }
            let tags = lambda.list_tags().resource(arn).send().await.map_err(|e| aws_text(&e))?;
            if tags.tags().and_then(|t| t.get(TAG_KEY)).map(String::as_str) != Some(TAG_VALUE) {
                continue;
            }
            match lambda.delete_function().function_name(name).send().await {
                Ok(_) => {}
                Err(e) if aws_text(&e).contains("ResourceNotFound") => {}
                Err(e) => return Err(aws_text(&e)),
            }
        }
        marker = page.next_marker().map(str::to_owned);
        if marker.is_none() {
            return Ok(());
        }
    }
}

/// What forkrail has in another region, in a few words each; empty for nothing. A region the
/// account hasn't turned on has nothing.
async fn found_in(config: &SdkConfig, code_bucket: &str) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let lambda = aws_sdk_lambda::Client::new(config);
    let mut marker = None;
    let mut functions = 0;
    loop {
        let page = match lambda.list_functions().set_marker(marker).send().await {
            Ok(page) => page,
            Err(e) if region_off(&aws_text(&e)) => return Ok(Vec::new()),
            Err(e) => return Err(aws_text(&e)),
        };
        functions += page.functions().iter().filter(|f| f.function_name().is_some_and(|n| n.starts_with("forkrail-"))).count();
        marker = page.next_marker().map(str::to_owned);
        if marker.is_none() {
            break;
        }
    }
    if functions > 0 {
        found.push(format!("{functions} function{}", if functions == 1 { "" } else { "s" }));
    }
    let stacks = stacks_in(config).await?;
    if !stacks.is_empty() {
        found.push(format!("stack{} {}", if stacks.len() == 1 { "" } else { "s" }, stacks.iter().map(|s| format!("forkrail-{s}")).collect::<Vec<_>>().join(", ")));
    }
    match aws_sdk_s3::Client::new(config).head_bucket().bucket(code_bucket).send().await {
        Ok(_) => found.push(format!("bucket {code_bucket}")),
        Err(e) if e.as_service_error().is_some_and(|e| e.is_not_found()) => {}
        Err(e) => return Err(aws_text(&e)),
    }
    let layers = lambda.list_layers().send().await.map_err(|e| aws_text(&e))?;
    if layers.layers().iter().any(|l| l.layer_name().is_some_and(|n| n.starts_with(ENV_LAYER_PREFIX))) {
        found.push("env layers".into());
    }
    let kms = aws_sdk_kms::Client::new(config);
    for alias in ALIASES {
        match kms.describe_key().key_id(*alias).send().await {
            Ok(out) if out.key_metadata().and_then(|m| m.key_state()).is_some_and(|s| s.as_str() != "PendingDeletion") => found.push(format!("key {alias}")),
            Ok(_) => {}
            Err(e) if aws_text(&e).contains("NotFound") => {}
            Err(e) => return Err(aws_text(&e)),
        }
    }
    if !found.iter().any(|f| f.starts_with("key ")) && !tagged_keys(&kms).await?.is_empty() {
        found.push("encryption keys".into());
    }
    let logs = aws_sdk_cloudwatchlogs::Client::new(config);
    for prefix in ["/aws/lambda/forkrail-", "/forkrail/"] {
        let page = logs.describe_log_groups().log_group_name_prefix(prefix).limit(1).send().await.map_err(|e| aws_text(&e))?;
        if !page.log_groups().is_empty() {
            found.push("log groups".into());
            break;
        }
    }
    Ok(found)
}

/// Whether an error says the region isn't on for this account (an opt-in region left off).
fn region_off(said: &str) -> bool {
    ["UnrecognizedClientException", "InvalidClientTokenId", "AuthFailure", "OptInRequired", "not enabled", "security token included in the request is invalid"].iter().any(|n| said.contains(n))
}

/// forkrail's stacks in the region, newest first, by the names `stacks::delete` takes.
async fn stacks_in(config: &SdkConfig) -> Result<Vec<String>, String> {
    let cfn = aws_sdk_cloudformation::Client::new(config);
    let mut found: Vec<(String, i64)> = Vec::new();
    let mut token = None;
    loop {
        let page = cfn.list_stacks().set_next_token(token).send().await.map_err(|e| aws_text(&e))?;
        for s in page.stack_summaries() {
            let gone = s.stack_status().is_some_and(|st| st.as_str() == "DELETE_COMPLETE");
            if let (false, Some(name)) = (gone, s.stack_name().and_then(|n| n.strip_prefix("forkrail-"))) {
                found.push((name.to_owned(), s.creation_time().map_or(0, |t| t.secs())));
            }
        }
        token = page.next_token().map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    found.sort_by_key(|(_, created)| std::cmp::Reverse(*created));
    found.dedup_by(|a, b| a.0 == b.0);
    Ok(found.into_iter().map(|(name, _)| name).collect())
}

/// Every version of the apps' env layers (`forkrail-env-<project>`). A publish deletes its own;
/// these are what an interrupted one left.
async fn delete_layers(config: &SdkConfig) -> Result<(), String> {
    let lambda = aws_sdk_lambda::Client::new(config);
    let mut marker = None;
    loop {
        let page = lambda.list_layers().set_marker(marker).send().await.map_err(|e| aws_text(&e))?;
        for layer in page.layers() {
            let Some(name) = layer.layer_name().filter(|n| n.starts_with(ENV_LAYER_PREFIX)) else { continue };
            let mut versions_marker = None;
            loop {
                let versions = lambda.list_layer_versions().layer_name(name).set_marker(versions_marker).send().await.map_err(|e| aws_text(&e))?;
                for v in versions.layer_versions() {
                    match lambda.delete_layer_version().layer_name(name).version_number(v.version()).send().await {
                        Ok(_) => {}
                        Err(e) if aws_text(&e).contains("ResourceNotFound") => {}
                        Err(e) => return Err(aws_text(&e)),
                    }
                }
                versions_marker = versions.next_marker().map(str::to_owned);
                if versions_marker.is_none() {
                    break;
                }
            }
        }
        marker = page.next_marker().map(str::to_owned);
        if marker.is_none() {
            return Ok(());
        }
    }
}

async fn schedule_groups(config: &SdkConfig) -> Result<(), String> {
    let scheduler = aws_sdk_scheduler::Client::new(config);
    let groups = scheduler.list_schedule_groups().name_prefix("forkrail-").send().await.map_err(|e| aws_text(&e))?;
    for group in groups.schedule_groups() {
        let Some(name) = group.name() else { continue };
        // The retention stack's own group goes with its stack.
        if name == "forkrail-retention" {
            continue;
        }
        match scheduler.delete_schedule_group().name(name).send().await {
            Ok(_) => {}
            Err(e) if aws_text(&e).contains("ResourceNotFound") => {}
            Err(e) => return Err(aws_text(&e)),
        }
    }
    Ok(())
}

/// Every object and version in `bucket`; a bucket that isn't there is fine.
async fn empty_bucket(config: &SdkConfig, bucket: &str) -> Result<(), String> {
    use aws_sdk_s3::types::{Delete, ObjectIdentifier};
    let s3 = aws_sdk_s3::Client::new(config);
    let mut key_marker = None;
    let mut version_marker = None;
    loop {
        let page = match s3.list_object_versions().bucket(bucket).set_key_marker(key_marker).set_version_id_marker(version_marker).send().await {
            Ok(page) => page,
            Err(e) if aws_text(&e).contains("NoSuchBucket") => return Ok(()),
            Err(e) => return Err(aws_text(&e)),
        };
        let mut ids: Vec<ObjectIdentifier> = Vec::new();
        for v in page.versions() {
            if let Some(key) = v.key() {
                ids.push(ObjectIdentifier::builder().key(key).set_version_id(v.version_id().map(str::to_owned)).build().map_err(|e| e.to_string())?);
            }
        }
        for m in page.delete_markers() {
            if let Some(key) = m.key() {
                ids.push(ObjectIdentifier::builder().key(key).set_version_id(m.version_id().map(str::to_owned)).build().map_err(|e| e.to_string())?);
            }
        }
        for chunk in ids.chunks(1000) {
            let delete = Delete::builder().set_objects(Some(chunk.to_vec())).quiet(true).build().map_err(|e| e.to_string())?;
            let out = s3.delete_objects().bucket(bucket).delete(delete).send().await.map_err(|e| aws_text(&e))?;
            if let Some(err) = out.errors().first() {
                return Err(format!("{}: {}", err.key().unwrap_or("?"), err.message().unwrap_or("not deleted")));
            }
        }
        if page.is_truncated() != Some(true) {
            return Ok(());
        }
        key_marker = page.next_key_marker().map(str::to_owned);
        version_marker = page.next_version_id_marker().map(str::to_owned);
    }
}

async fn delete_bucket(config: &SdkConfig, bucket: &str) -> Result<(), String> {
    empty_bucket(config, bucket).await?;
    match aws_sdk_s3::Client::new(config).delete_bucket().bucket(bucket).send().await {
        Ok(_) => Ok(()),
        Err(e) if aws_text(&e).contains("NoSuchBucket") => Ok(()),
        Err(e) => Err(aws_text(&e)),
    }
}

async fn delete_table(config: &SdkConfig, table: &str) -> Result<(), String> {
    let dynamodb = aws_sdk_dynamodb::Client::new(config);
    match dynamodb.update_table().table_name(table).deletion_protection_enabled(false).send().await {
        Ok(_) => {}
        Err(e) if aws_text(&e).contains("ResourceNotFound") => return Ok(()),
        // Nothing to change, or the table is still updating: the delete says which.
        Err(_) => {}
    }
    // The change takes a moment to apply.
    for _ in 0..30 {
        match dynamodb.delete_table().table_name(table).send().await {
            Ok(_) => return Ok(()),
            Err(e) if aws_text(&e).contains("ResourceNotFound") => return Ok(()),
            Err(e) if ["ResourceInUse", "deletion protection", "protected against deletion"].iter().any(|s| aws_text(&e).contains(s)) => tokio::time::sleep(std::time::Duration::from_secs(5)).await,
            Err(e) => return Err(aws_text(&e)),
        }
    }
    Err(format!("table {table} kept refusing deletion"))
}

async fn delete_repository(config: &SdkConfig) -> Result<(), String> {
    match aws_sdk_ecr::Client::new(config).delete_repository().repository_name("forkrail-builder").force(true).send().await {
        Ok(_) => Ok(()),
        Err(e) if aws_text(&e).contains("RepositoryNotFound") => Ok(()),
        Err(e) => Err(aws_text(&e)),
    }
}

async fn delete_parameters(config: &SdkConfig) -> Result<(), String> {
    let ssm = aws_sdk_ssm::Client::new(config);
    let mut names = Vec::new();
    let mut token = None;
    loop {
        let page = ssm.get_parameters_by_path().path("/forkrail/").recursive(true).set_next_token(token).send().await.map_err(|e| aws_text(&e))?;
        names.extend(page.parameters().iter().filter_map(|p| p.name().map(str::to_owned)));
        token = page.next_token().map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    for chunk in names.chunks(10) {
        ssm.delete_parameters().set_names(Some(chunk.to_vec())).send().await.map_err(|e| aws_text(&e))?;
    }
    Ok(())
}

async fn delete_topic(config: &SdkConfig, caller: &checks::Caller) -> Result<(), String> {
    let region = config.region().map(|r| r.to_string()).unwrap_or_default();
    let arn = format!("arn:{}:sns:{region}:{}:{}", caller.partition, caller.account, super::steps::ALERTS_TOPIC);
    aws_sdk_sns::Client::new(config).delete_topic().topic_arn(arn).send().await.map(drop).map_err(|e| aws_text(&e))
}

async fn delete_log_groups(config: &SdkConfig) -> Result<(), String> {
    let logs = aws_sdk_cloudwatchlogs::Client::new(config);
    for prefix in ["/aws/lambda/forkrail-", "/forkrail/"] {
        let mut token = None;
        loop {
            let page = logs.describe_log_groups().log_group_name_prefix(prefix).set_next_token(token).send().await.map_err(|e| aws_text(&e))?;
            for group in page.log_groups() {
                if let Some(name) = group.log_group_name() {
                    match logs.delete_log_group().log_group_name(name).send().await {
                        Ok(_) => {}
                        Err(e) if aws_text(&e).contains("ResourceNotFound") => {}
                        Err(e) => return Err(aws_text(&e)),
                    }
                }
            }
            token = page.next_token().map(str::to_owned);
            if token.is_none() {
                break;
            }
        }
    }
    Ok(())
}

async fn delete_keys(config: &SdkConfig) -> Result<(), String> {
    let kms = aws_sdk_kms::Client::new(config);
    for alias in ALIASES {
        let key = match kms.describe_key().key_id(*alias).send().await {
            Ok(out) => out.key_metadata().map(|m| (m.key_id().to_owned(), m.key_state().map(|s| s.as_str().to_owned()))),
            Err(e) if aws_text(&e).contains("NotFound") => None,
            Err(e) => return Err(aws_text(&e)),
        };
        let Some((id, state)) = key else { continue };
        if state.as_deref() != Some("PendingDeletion") {
            kms.schedule_key_deletion().key_id(&id).pending_window_in_days(7).send().await.map_err(|e| aws_text(&e))?;
        }
        let _ = kms.delete_alias().alias_name(*alias).send().await;
    }
    // The stacks keep their keys (DeletionPolicy: Retain) but not the aliases, so once the stacks
    // are gone a key is found by its tag only. The region setup's keys are tagged too.
    for id in tagged_keys(&kms).await? {
        kms.schedule_key_deletion().key_id(&id).pending_window_in_days(7).send().await.map_err(|e| aws_text(&e))?;
    }
    Ok(())
}

/// The keys forkrail made here that aren't waiting for deletion, whatever their aliases: tagged
/// `project=forkrail` (the stacks' keys, and the region setup's), or [`crate::regions::ENV_KEY_TAG`]
/// (a region setup's).
async fn tagged_keys(kms: &aws_sdk_kms::Client) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    let mut marker = None;
    loop {
        let page = kms.list_keys().set_marker(marker).limit(1000).send().await.map_err(|e| aws_text(&e))?;
        for key in page.keys() {
            let Some(id) = key.key_id() else { continue };
            // Keys AWS manages for its services answer no tags, or refuse: neither is one of these.
            let Ok(tags) = kms.list_resource_tags().key_id(id).send().await else { continue };
            if !tags.tags().iter().any(|t| (t.tag_key() == TAG_KEY && t.tag_value() == TAG_VALUE) || (t.tag_key() == crate::regions::ENV_KEY_TAG && t.tag_value() == "env")) {
                continue;
            }
            let state = kms.describe_key().key_id(id).send().await.map_err(|e| aws_text(&e))?;
            if state.key_metadata().and_then(|m| m.key_state()).is_some_and(|s| s.as_str() != "PendingDeletion") {
                ids.push(id.to_owned());
            }
        }
        if !page.truncated() {
            break;
        }
        marker = page.next_marker().map(str::to_owned);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_waves_delete_every_stack_once_and_in_use_ones_last() {
        let mut all: Vec<&str> = STACK_WAVES.iter().flat_map(|w| w.iter().copied()).collect();
        let count = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), count, "a stack is in two waves");
        assert_eq!(count, 18);
        for name in &all {
            assert!(super::super::stacks::template(name).is_some(), "no template {name}");
        }
        let wave = |name: &str| STACK_WAVES.iter().position(|w| w.contains(&name)).unwrap();
        assert!(wave("install-network") > wave("install-router") && wave("install-network") > wave("install-analytics"));
        for user in ["builder", "guard", "retention", "schedules"] {
            assert!(wave("control") > wave(user));
        }
        assert!(STACK_WAVES[..2].iter().all(|w| !w.contains(&"install-roles")));
    }
}
