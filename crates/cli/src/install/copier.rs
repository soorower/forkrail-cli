//! The release copied into the account by the account itself: a function of the install's
//! (forkrail-install-fetch, deploy/install/fetch.py) downloads the published release from GitHub
//! and puts its files in the bucket and the builder's image in the registry. The release is about
//! 1 GB; this way none of it goes through the installing computer's connection, and inside AWS it
//! takes a minute or two.
//!
//! The installer reads the SHA-256 published beside the release itself and hands it to the
//! function, which checks the whole download against it before it tags the image or says it's
//! done. The function says how it's going in a status object in the bucket, which the installer
//! reads every few seconds; a copy that stops saying anything has stopped.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::time::{Duration, Instant};

use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::InvocationType;
use aws_sdk_s3::primitives::ByteStream;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::Cx;
use super::problem::aws_text;
use super::steps::MapErrText as _;

/// The function's code.
const CODE: &str = include_str!("../../../../deploy/install/fetch.py");
/// Its stack, without `forkrail-`.
pub const STACK: &str = "install-fetch";
const REPOSITORY: &str = "forkrail-builder";
/// Where the release's files are put, and the copy's status.
pub const PREFIX: &str = "install/release/";

/// How long a copy may take to say anything, to stay quiet, and to take in all.
const STARTING: Duration = Duration::from_secs(120);
const QUIET: Duration = Duration::from_secs(90);
const LONGEST: Duration = Duration::from_secs(16 * 60);
const EVERY: Duration = Duration::from_secs(3);

/// A file of the release, as the function put it in the bucket.
#[derive(Debug, Clone, Deserialize)]
pub struct File {
    pub sha256: String,
    #[allow(dead_code)]
    pub size: u64,
}

#[derive(Debug, Deserialize)]
struct Status {
    state: String,
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    beat: u64,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    files: BTreeMap<String, File>,
    #[serde(default)]
    image_digest: String,
    #[serde(default)]
    secs: Option<u64>,
}

/// A copied release: its files in the bucket under `prefix`, by their paths in the release, and
/// the builder's image in the registry.
pub struct Copied {
    pub prefix: String,
    pub files: BTreeMap<String, File>,
    pub image_digest: String,
}

impl Copied {
    pub fn key(&self, file: &str) -> String {
        format!("{}{file}", self.prefix)
    }
}

/// The function's zip: fetch.py alone.
fn zip() -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o444)
        .last_modified_time(zip::DateTime::default());
    zip.start_file("fetch.py", options).expect("a zip entry in memory");
    zip.write_all(CODE.as_bytes()).expect("writing to memory");
    zip.finish().expect("a zip in memory").into_inner()
}

/// The zip's key in the bucket, named by the code in it.
fn code_key() -> String {
    format!("install/fetch/forkrail-install-fetch-{}.zip", &forkrail_common::hex(&Sha256::digest(CODE.as_bytes()))[..16])
}

/// Where a release's files go: by version and the release's own SHA-256.
pub fn prefix(version: &str, sha256: &str) -> String {
    format!("{PREFIX}{version}-{}/", &sha256[..12.min(sha256.len())])
}

/// Copies the release at `url` (SHA-256 `sha256`, version `version`) into the account. Errors are
/// one line: the installer copies the release itself then.
pub async fn copy(cx: &Cx, url: &str, version: &str, sha256: &str) -> Result<Copied, String> {
    let s3 = aws_sdk_s3::Client::new(&cx.config);
    let bucket = cx.bucket();
    let key = code_key();
    if s3.head_object().bucket(&bucket).key(&key).send().await.is_err() {
        s3.put_object().bucket(&bucket).key(&key).body(ByteStream::from(zip())).send().await.map_err(|e| format!("uploading the copier: {}", aws_text(&e)))?;
    }
    let outputs = cx.stack("release", STACK, &[("CodeKey", key)]).await.map_err(|p| format!("{}: {}", p.action, p.said))?;
    let function = outputs.get("FunctionName").cloned().unwrap_or_else(|| format!("forkrail-{STACK}"));

    let prefix = prefix(version, sha256);
    let status_key = format!("{PREFIX}status-{}.json", forkrail_common::random_hex(6));
    let event = serde_json::json!({
        "url": url,
        "sha256": sha256,
        "bucket": bucket,
        "prefix": prefix,
        "status_key": status_key,
        "repository": REPOSITORY,
        "tag": format!("install-{version}"),
    });
    let lambda = aws_sdk_lambda::Client::new(&cx.config);
    let payload = serde_json::to_vec(&event).expect("JSON");
    cx.call("release", "starting the copy in your account", || lambda.invoke().function_name(&function).invocation_type(InvocationType::Event).payload(Blob::new(payload.clone())).send().map_err_text())
        .await
        .map_err(|p| p.said)?;

    let watched = watch(cx, &s3, &bucket, &status_key).await;
    let _ = s3.delete_object().bucket(&bucket).key(&status_key).send().await;
    let status = watched?;
    Ok(Copied { prefix, files: status.files, image_digest: status.image_digest })
}

/// Reads the copy's status until it's done or failed, saying its lines as they come.
async fn watch(cx: &Cx, s3: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Result<Status, String> {
    let started = Instant::now();
    let (mut said, mut beat, mut moved) = (0usize, 0u64, Instant::now());
    loop {
        tokio::time::sleep(EVERY).await;
        let status = match s3.get_object().bucket(bucket).key(key).send().await {
            Ok(out) => match out.body.collect().await {
                Ok(body) => serde_json::from_slice::<Status>(&body.into_bytes()).ok(),
                Err(_) => None,
            },
            Err(e) if aws_text(&e).contains("NoSuchKey") => None,
            // A blip: the next look tells.
            Err(_) => None,
        };
        match status {
            None if beat == 0 && started.elapsed() > STARTING => return Err(format!("the copy didn't start within {} seconds", STARTING.as_secs())),
            None => {}
            Some(status) => {
                for line in status.lines.iter().skip(said) {
                    cx.report.note(format!("     {line}"));
                }
                said = said.max(status.lines.len());
                if status.beat != beat {
                    (beat, moved) = (status.beat, Instant::now());
                }
                match status.state.as_str() {
                    "done" => {
                        if let Some(secs) = status.secs {
                            cx.report.note(format!("     Your account copied forkrail in {secs} seconds."));
                        }
                        return Ok(status);
                    }
                    "failed" => return Err(status.error.unwrap_or_else(|| "the copy failed".into())),
                    _ => {}
                }
            }
        }
        if beat > 0 && moved.elapsed() > QUIET {
            return Err(format!("the copy stopped answering for {} seconds", QUIET.as_secs()));
        }
        if started.elapsed() > LONGEST {
            return Err(format!("the copy took longer than {} minutes", LONGEST.as_secs() / 60));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_is_named_by_its_content_and_zipped_alone() {
        let key = code_key();
        assert!(key.starts_with("install/fetch/forkrail-install-fetch-") && key.ends_with(".zip"));
        assert_eq!(key.len(), "install/fetch/forkrail-install-fetch-".len() + 16 + 4);
        // The template's pattern for it.
        assert!(super::super::stacks::template(STACK).unwrap().contains(r#"install/fetch/forkrail-install-fetch-[0-9a-f]{16}\\.zip"#));
        let bytes = zip();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(archive.len(), 1);
        let mut file = archive.by_name("fetch.py").unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut file, &mut text).unwrap();
        assert_eq!(text, CODE);
        assert_eq!(zip(), bytes);
    }

    #[test]
    fn a_release_goes_under_its_version_and_hash() {
        assert_eq!(prefix("0.1.1", "0123456789abcdef0123"), "install/release/0.1.1-0123456789ab/");
    }

    #[test]
    fn the_status_reads_while_running_and_when_done() {
        let running: Status = serde_json::from_str(r#"{"state":"running","lines":["downloaded 100 of 950 MB"],"beat":3}"#).unwrap();
        assert_eq!((running.state.as_str(), running.lines.len(), running.beat), ("running", 1, 3));
        let done: Status = serde_json::from_str(
            r#"{"state":"done","lines":[],"beat":9,"files":{"manifest.json":{"sha256":"ab","size":456}},"image_digest":"sha256:de","sha256":"ff","bytes":1,"secs":70}"#,
        )
        .unwrap();
        assert_eq!(done.files["manifest.json"].sha256, "ab");
        assert_eq!(done.image_digest, "sha256:de");
        assert_eq!(done.secs, Some(70));
    }
}
