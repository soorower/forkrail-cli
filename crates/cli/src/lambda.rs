//! A project's Lambda function, `forkrail-app-<project>`. Each deployment is one published version
//! of it, which freezes the code and the configuration, env vars and memory included.
//!
//! The account is shared with other workloads, so a function is only ever changed if it carries
//! `project=forkrail` and `forkrail-project=<name>`.
//!
//! A deployment whose env vars don't fit Lambda's 4 KB carries them in a sealed file
//! (forkrail_common::envfile), published as a version of the layer `forkrail-env-<project>` just
//! for it and deleted once the function version using it is published: the version keeps the
//! file, and nothing else can fetch it.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use aws_sdk_lambda::error::DisplayErrorContext;
use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::{Architecture, Environment, FunctionCode, LastUpdateStatus, LayerVersionContentInput, Runtime, State};
use forkrail_common::envfile;

pub use forkrail_common::install::{FUNCTION_PREFIX, PROJECT_TAG, TAG_KEY, TAG_VALUE, function_name};

/// Lambda's maximum. The router hands a worker off at 13 minutes, before Lambda's limit.
const TIMEOUT_S: i32 = 900;
const WAIT_LIMIT: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct Config {
    pub runtime: String,
    pub memory_mb: u32,
    pub env: BTreeMap<String, String>,
    pub role_arn: String,
    /// The KMS key the environment is encrypted with: the install's key in the function's region
    /// (`Ctx::env_key`), so reading the configuration without it shows no values.
    pub kms_key_arn: String,
    /// The sealed env file, when the env vars don't fit `env` (whose `FORKRAIL_ENV_KEY` opens it).
    pub env_file: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct Lambda {
    pub client: aws_sdk_lambda::Client,
}

impl Lambda {
    /// Whether the function exists. One that exists but isn't tagged for this project is refused.
    pub async fn check(&self, name: &str, project: &str) -> Result<bool> {
        match self.client.get_function().function_name(name).send().await {
            Ok(out) => {
                let tags = out.tags().cloned().unwrap_or_default();
                check_tags(name, project, &tags)?;
                Ok(true)
            }
            Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_not_found_exception()) => Ok(false),
            Err(e) => bail!("reading {name}: {}", DisplayErrorContext(&e)),
        }
    }

    /// Makes the function; returns the memory it got (`fit`).
    pub async fn create(&self, name: &str, project: &str, cfg: &Config, bucket: &str, key: &str, layers: &[String]) -> Result<u32> {
        let tags = HashMap::from([(TAG_KEY.to_owned(), TAG_VALUE.to_owned()), (PROJECT_TAG.to_owned(), project.to_owned())]);
        let code = FunctionCode::builder().s3_bucket(bucket).s3_key(key).build();
        let mut tries = 0;
        let mut memory = cfg.memory_mb;
        loop {
            let created = self
                .client
                .create_function()
                .function_name(name)
                .description(format!("forkrail project {project}"))
                .runtime(Runtime::from(cfg.runtime.as_str()))
                // Required, and unused: the exec wrapper starts in place of the runtime's bootstrap.
                .handler("unused")
                .role(&cfg.role_arn)
                .architectures(Architecture::Arm64)
                .memory_size(memory as i32)
                .timeout(TIMEOUT_S)
                .environment(environment(&cfg.env))
                .kms_key_arn(&cfg.kms_key_arn)
                .set_layers(Some(layers.to_vec()))
                .code(code.clone())
                .set_tags(Some(tags.clone()))
                .send()
                .await;
            match created {
                Ok(_) => return Ok(memory),
                Err(e) if let Some(cap) = fit(name, memory, &DisplayErrorContext(&e).to_string()) => memory = cap,
                // A role created moments ago takes a few seconds before Lambda can assume it.
                Err(e) if tries < 10 && DisplayErrorContext(&e).to_string().contains("cannot be assumed") => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                Err(e) => bail!("creating {name}: {}", DisplayErrorContext(&e)),
            }
        }
    }

    /// New code; then `update_config`.
    async fn update_code(&self, name: &str, bucket: &str, key: &str) -> Result<()> {
        // Another deploy may still be updating it.
        self.wait(name).await?;
        self.client
            .update_function_code()
            .function_name(name)
            .s3_bucket(bucket)
            .s3_key(key)
            .architectures(Architecture::Arm64)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("updating {name}'s code: {}", DisplayErrorContext(&e)))?;
        Ok(())
    }

    /// The configuration that goes with new code. `layers` replaces the function's: none takes
    /// off a sealed env file an earlier deployment had. Returns the memory it got (`fit`).
    async fn update_config(&self, name: &str, cfg: &Config, layers: &[String]) -> Result<u32> {
        // Until the code update lands, the configuration can't change.
        self.wait(name).await?;
        let mut memory = cfg.memory_mb;
        loop {
            let updated = self
                .client
                .update_function_configuration()
                .function_name(name)
                .runtime(Runtime::from(cfg.runtime.as_str()))
                .handler("unused")
                .role(&cfg.role_arn)
                .memory_size(memory as i32)
                .timeout(TIMEOUT_S)
                .environment(environment(&cfg.env))
                .kms_key_arn(&cfg.kms_key_arn)
                .set_layers(Some(layers.to_vec()))
                .send()
                .await;
            match updated {
                Ok(_) => return Ok(memory),
                Err(e) if let Some(cap) = fit(name, memory, &DisplayErrorContext(&e).to_string()) => memory = cap,
                Err(e) => bail!("updating {name}'s configuration: {}", DisplayErrorContext(&e)),
            }
        }
    }

    /// Encrypt `$LATEST`'s environment with `kms_key_arn`, leaving everything else as it is.
    /// `$LATEST` is never what serves (deployments are versions), but a role that may list
    /// functions sees its environment. Returns false if it already used the key.
    pub async fn encrypt_latest(&self, name: &str, kms_key_arn: &str) -> Result<bool> {
        let (_, _) = self.wait(name).await?;
        let current = self
            .client
            .get_function_configuration()
            .function_name(name)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("reading {name}: {}", DisplayErrorContext(&e)))?;
        if current.kms_key_arn() == Some(kms_key_arn) {
            return Ok(false);
        }
        self.client
            .update_function_configuration()
            .function_name(name)
            .kms_key_arn(kms_key_arn)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("encrypting {name}'s environment: {}", DisplayErrorContext(&e)))?;
        self.wait(name).await?;
        Ok(true)
    }

    /// The project functions in this region, by name: every `forkrail-app-*`. Their tags aren't
    /// listed with them, so `check` each before changing it.
    pub async fn project_functions(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut pages = self.client.list_functions().into_paginator().send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("listing functions: {}", DisplayErrorContext(&e)))?;
            names.extend(page.functions().iter().filter_map(|f| f.function_name()).filter(|n| n.starts_with(FUNCTION_PREFIX)).map(str::to_owned));
        }
        Ok(names)
    }

    /// Wait until no update is in progress. Returns the revision and code hash it settled on.
    pub async fn wait(&self, name: &str) -> Result<(String, String)> {
        let started = Instant::now();
        loop {
            let cfg = self
                .client
                .get_function_configuration()
                .function_name(name)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("reading {name}: {}", DisplayErrorContext(&e)))?;
            match (cfg.state(), cfg.last_update_status()) {
                (Some(State::Failed), _) => bail!("{name} failed: {}", cfg.state_reason().unwrap_or("no reason given")),
                (_, Some(LastUpdateStatus::Failed)) => {
                    bail!("updating {name} failed: {}", cfg.last_update_status_reason().unwrap_or("no reason given"))
                }
                (Some(State::Active), Some(LastUpdateStatus::Successful) | None) => {
                    return Ok((cfg.revision_id().unwrap_or_default().to_owned(), cfg.code_sha256().unwrap_or_default().to_owned()));
                }
                _ if started.elapsed() > WAIT_LIMIT => bail!("{name} is still updating after {} s", WAIT_LIMIT.as_secs()),
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }

    /// Publish what's in `$LATEST` as a version, only if it's still the code and the revision we
    /// set: two deploys at once then fail cleanly instead of mixing one's code with the other's
    /// configuration. Lambda returns the latest version instead of a new one only if nothing was
    /// updated since; after `update`, it publishes a new one even for identical code and settings.
    pub async fn publish(&self, name: &str, code_sha256: &str, revision_id: &str, description: &str) -> Result<String> {
        let out = self
            .client
            .publish_version()
            .function_name(name)
            .code_sha256(code_sha256)
            .revision_id(revision_id)
            .description(description)
            .send()
            .await;
        match out {
            Ok(out) => {
                let version = out.version().unwrap_or_default().to_owned();
                self.wait_version(name, &version).await?;
                Ok(version)
            }
            Err(e) if e.as_service_error().is_some_and(|e| e.is_precondition_failed_exception()) => {
                bail!("{name} changed while publishing, probably another deploy at the same time; deploy again")
            }
            Err(e) => bail!("publishing {name}: {}", DisplayErrorContext(&e)),
        }
    }

    /// Wait until a published version can be invoked. With SnapStart it stays `Pending` while Lambda
    /// runs init and takes the snapshot, and invoking it fails until then.
    async fn wait_version(&self, name: &str, version: &str) -> Result<()> {
        let started = Instant::now();
        loop {
            let cfg = self
                .client
                .get_function_configuration()
                .function_name(name)
                .qualifier(version)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("reading {name}:{version}: {}", DisplayErrorContext(&e)))?;
            match cfg.state() {
                Some(State::Active) => {
                    if let Some(status) = cfg.snap_start().and_then(|s| s.optimization_status()) {
                        crate::deploy::log(&format!("{name}:{version} active after {:.0} s (SnapStart {})", started.elapsed().as_secs_f64(), status.as_str()));
                    }
                    return Ok(());
                }
                Some(State::Failed) => {
                    bail!("{name}:{version} failed: {}", cfg.state_reason().unwrap_or("no reason given"))
                }
                _ if started.elapsed() > WAIT_LIMIT => {
                    bail!("{name}:{version} is still {:?} after {} s", cfg.state(), WAIT_LIMIT.as_secs())
                }
                _ => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    }

    /// Make `$LATEST` the package at `bucket`/`key` with `cfg`, creating the function if need be,
    /// and publish it as a version, which is returned with the memory it got (less than `cfg`'s
    /// only where the account allows less, `fit`). `code_sha256` is the package's base64 SHA-256:
    /// a function left with other code (another deploy at the same time) is an error.
    pub async fn publish_package(
        &self,
        name: &str,
        project: &str,
        cfg: &Config,
        (bucket, key): (&str, &str),
        code_sha256: &str,
        description: &str,
    ) -> Result<(String, u32)> {
        let exists = self.check(name, project).await?;
        // A sealed env file's layer takes seconds to publish: it goes up while the code does.
        let layer = async {
            match &cfg.env_file {
                Some(file) => self.publish_env_layer(project, file, description).await.map(Some),
                None => Ok(None),
            }
        };
        let code = async {
            if exists { self.update_code(name, bucket, key).await } else { Ok(()) }
        };
        let (layer, code) = tokio::join!(layer, code);
        let layer = match (layer, code) {
            (Ok(layer), Ok(())) => layer,
            (Ok(layer), Err(e)) => {
                if let Some((_, version)) = layer {
                    let _ = self.delete_env_layer(project, version).await;
                }
                return Err(e);
            }
            (Err(e), _) => return Err(e),
        };
        let layers: Vec<String> = layer.iter().map(|(arn, _)| arn.clone()).collect();
        let published = async {
            let memory = if exists { self.update_config(name, cfg, &layers).await? } else { self.create(name, project, cfg, bucket, key, &layers).await? };
            let (revision, settled) = self.wait(name).await?;
            if settled != code_sha256 {
                bail!("{name} has other code than this package, probably another deploy at the same time; deploy again");
            }
            Ok((self.publish(name, &settled, &revision, description).await?, memory))
        }
        .await;
        // Published or not, the layer version goes: the function version keeps its file.
        if let Some((_, version)) = layer
            && let Err(e) = self.delete_env_layer(project, version).await
        {
            crate::deploy::warn(&format!("{e:#}; `forkrail sweep` removes it"));
        }
        published
    }

    /// Publish a sealed env file as a new version of the project's env layer: its ARN and number.
    async fn publish_env_layer(&self, project: &str, file: &[u8], description: &str) -> Result<(String, i64)> {
        let layer = envfile::layer_name(project);
        let content = LayerVersionContentInput::builder().zip_file(Blob::new(env_layer_zip(file)?)).build();
        let out = self
            .client
            .publish_layer_version()
            .layer_name(&layer)
            .description(format!("{description}: its env vars, sealed"))
            .content(content)
            .compatible_architectures(Architecture::Arm64)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("publishing {layer}: {}", DisplayErrorContext(&e)))?;
        match (out.layer_version_arn(), out.version()) {
            (Some(arn), version) if version > 0 => Ok((arn.to_owned(), version)),
            _ => bail!("publishing {layer}: Lambda answered without the new version"),
        }
    }

    /// Env layer versions in this region (`only`'s, or every project's) made over an hour ago: what
    /// a publish failed to delete. A younger one may belong to a publish under way. Any version can
    /// go: a function version using it keeps its file.
    pub async fn env_layer_leftovers(&self, only: Option<&str>) -> Result<Vec<(String, i64)>> {
        let prefix = only.map_or_else(|| envfile::layer_name(""), envfile::layer_name);
        let mut layers = Vec::new();
        let mut pages = self.client.list_layers().into_paginator().send();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| anyhow::anyhow!("listing layers: {}", DisplayErrorContext(&e)))?;
            layers.extend(page.layers().iter().filter_map(|l| l.layer_name()).filter(|n| if only.is_some() { *n == prefix } else { n.starts_with(&prefix) }).map(str::to_owned));
        }
        let hour_ago = chrono::Utc::now() - chrono::Duration::hours(1);
        let mut leftovers = Vec::new();
        for layer in layers {
            let mut pages = self.client.list_layer_versions().layer_name(&layer).into_paginator().send();
            while let Some(page) = pages.next().await {
                let page = page.map_err(|e| anyhow::anyhow!("listing {layer}'s versions: {}", DisplayErrorContext(&e)))?;
                for v in page.layer_versions() {
                    let created = v.created_date().and_then(layer_created);
                    if created.is_some_and(|c| c < hour_ago) {
                        leftovers.push((layer.clone(), v.version()));
                    }
                }
            }
        }
        Ok(leftovers)
    }

    pub async fn delete_layer_version(&self, layer: &str, version: i64) -> Result<()> {
        if !layer.starts_with(&envfile::layer_name("")) {
            bail!("refusing to delete {layer}:{version}: not one of forkrail's env layers");
        }
        self.client
            .delete_layer_version()
            .layer_name(layer)
            .version_number(version)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("deleting {layer}:{version}: {}", DisplayErrorContext(&e)))?;
        Ok(())
    }

    async fn delete_env_layer(&self, project: &str, version: i64) -> Result<()> {
        self.delete_layer_version(&envfile::layer_name(project), version).await
    }

    /// A published version's configuration, for publishing the same deployment elsewhere: its
    /// runtime, memory, environment and role, and its code's base64 SHA-256.
    pub async fn version_config(&self, name: &str, version: &str) -> Result<(Config, String)> {
        let cfg = self
            .client
            .get_function_configuration()
            .function_name(name)
            .qualifier(version)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("reading {name}:{version}: {}", DisplayErrorContext(&e)))?;
        // Lambda reports, rather than fails, an environment the caller can't decrypt.
        if let Some(error) = cfg.environment().and_then(|e| e.error()) {
            bail!("{name}:{version}: its env vars can't be read ({}): this needs kms:Decrypt on the env key", error.message().unwrap_or("no reason given"));
        }
        let env = cfg.environment().and_then(|e| e.variables()).cloned().unwrap_or_default();
        let config = Config {
            runtime: cfg.runtime().map(|r| r.as_str().to_owned()).unwrap_or_default(),
            memory_mb: cfg.memory_size().unwrap_or_default().max(0) as u32,
            env: env.into_iter().collect(),
            role_arn: cfg.role().unwrap_or_default().to_owned(),
            // The caller sets the key of the region the copy goes to.
            kms_key_arn: String::new(),
            env_file: None,
        };
        if config.runtime.is_empty() || config.memory_mb == 0 || config.role_arn.is_empty() {
            bail!("{name}:{version}: its configuration is missing runtime, memory or role");
        }
        Ok((config, cfg.code_sha256().unwrap_or_default().to_owned()))
    }

    /// Delete the whole function, every version with it, after checking its tags. False if there
    /// was none.
    pub async fn delete_function(&self, name: &str, project: &str) -> Result<bool> {
        if !self.check(name, project).await? {
            return Ok(false);
        }
        match self.client.delete_function().function_name(name).send().await {
            Ok(_) => Ok(true),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_not_found_exception()) => Ok(false),
            Err(e) => bail!("deleting {name}: {}", DisplayErrorContext(&e)),
        }
    }

    /// Delete one published version, after checking the function's tags again.
    pub async fn delete_version(&self, name: &str, project: &str, version: &str) -> Result<()> {
        if version.is_empty() || version == "$LATEST" || version.parse::<u64>().is_err() {
            bail!("refusing to delete {name}:{version}: only numbered versions are deleted");
        }
        if !self.check(name, project).await? {
            return Ok(());
        }
        match self.client.delete_function().function_name(name).qualifier(version).send().await {
            Ok(_) => Ok(()),
            Err(e) if e.as_service_error().is_some_and(|e| e.is_resource_not_found_exception()) => Ok(()),
            Err(e) => bail!("deleting {name}:{version}: {}", DisplayErrorContext(&e)),
        }
    }
}

fn check_tags(name: &str, project: &str, tags: &HashMap<String, String>) -> Result<()> {
    let ours = tags.get(TAG_KEY).map(String::as_str) == Some(TAG_VALUE);
    let this_project = tags.get(PROJECT_TAG).map(String::as_str) == Some(project);
    if !ours || !this_project {
        bail!("{name} exists but isn't tagged {TAG_KEY}={TAG_VALUE} and {PROJECT_TAG}={project}; not touching it");
    }
    Ok(())
}

/// When a layer version was made, from Lambda's `2026-11-15T08:22:56.615+0000`.
fn layer_created(date: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_str(date, "%Y-%m-%dT%H:%M:%S%.f%z").ok()
}

/// A layer holding just the sealed env file, where the runtime looks for it.
fn env_layer_zip(file: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).unix_permissions(0o444);
    zip.start_file(envfile::LAYER_ENTRY, options)?;
    zip.write_all(file)?;
    Ok(zip.finish()?.into_inner())
}

fn environment(env: &BTreeMap<String, String>) -> Environment {
    Environment::builder().set_variables(Some(env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())).build()
}

/// The most memory this account lets a function have, from Lambda's refusal of more ("'MemorySize'
/// value failed to satisfy constraint: Member must have value less than or equal to 512"), however
/// it's passed on (CloudFormation quotes it). New accounts start low, and AWS raises the limit as
/// the account gets used; no API says what it is.
pub fn memory_limit(said: &str) -> Option<u32> {
    let lower = said.to_ascii_lowercase();
    let at = lower.find("memorysize")?;
    let rest = lower[at..].split("less than or equal to ").nth(1)?;
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok().filter(|cap| (128..10_240).contains(cap))
}

/// A refusal of `asked` MB for the account's lower limit: that limit, to try again with, said in
/// the log. None for any other refusal.
fn fit(name: &str, asked: u32, said: &str) -> Option<u32> {
    let cap = memory_limit(said).filter(|cap| *cap < asked)?;
    crate::deploy::log(&format!(
        "{name}: this AWS account lets each function use up to {cap} MB for now, so it gets {cap} MB instead of {asked}. AWS raises the limit as the account gets used"
    ));
    Some(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_accounts_memory_limit() {
        assert_eq!(memory_limit("Resource handler returned message: \"'MemorySize' value failed to satisfy constraint: Member must have value less than or equal to 512 (Service: Lambda, Status Code: 400"), Some(512));
        assert_eq!(memory_limit("ValidationException: 1 validation error detected: Value '4096' at 'memorySize' failed to satisfy constraint: Member must have value less than or equal to 3008"), Some(3008));
        assert_eq!(memory_limit("Timeout: value must be less than or equal to 900"), None);
        assert_eq!(memory_limit("The role defined for the function cannot be assumed by Lambda."), None);
        // Lambda's own most is no account limit.
        assert_eq!(memory_limit("'MemorySize' value failed to satisfy constraint: Member must have value less than or equal to 10240"), None);
        // Only a limit below what was asked is tried again.
        let said = "'MemorySize' value failed to satisfy constraint: Member must have value less than or equal to 512";
        assert_eq!(fit("f", 640, said), Some(512));
        assert_eq!(fit("f", 512, said), None);
        assert_eq!(fit("f", 640, "AccessDenied"), None);
    }

    #[test]
    fn env_layers() {
        let created = layer_created("2026-11-15T08:22:56.615+0000").unwrap();
        assert_eq!(created.timestamp(), 1_794_730_976);
        assert!(layer_created("yesterday").is_none());
        // The zip holds the file where the runtime looks: /opt/forkrail/env.
        let zip = env_layer_zip(b"sealed").unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
        let mut file = archive.by_name(envfile::LAYER_ENTRY).unwrap();
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut body).unwrap();
        assert_eq!(body, b"sealed");
        assert_eq!(format!("/opt/{}", envfile::LAYER_ENTRY), envfile::FILE);
    }

    #[test]
    fn tags_must_name_the_project() {
        let tags = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
        assert!(check_tags("f", "pyapp", &tags(&[("project", "forkrail"), ("forkrail-project", "pyapp")])).is_ok());
        assert!(check_tags("f", "pyapp", &tags(&[("project", "forkrail"), ("forkrail-project", "other")])).is_err());
        assert!(check_tags("f", "pyapp", &tags(&[("project", "forkrail")])).is_err());
        assert!(check_tags("f", "pyapp", &tags(&[])).is_err());
    }
}
