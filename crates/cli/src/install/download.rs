//! A release for this installer's own version. The account copies it itself ([`super::copier`]),
//! with the SHA-256 [`published`] reads; when it can't, the release is downloaded here the first
//! time a run needs it, and uploaded. Releases are
//! published as one tar file each (deploy/release/publish.sh) beside a file with its SHA-256, on
//! GitHub, which serves them for nothing; the installer checks the file against the SHA-256 built
//! into it (`pinned`), or the published one, unpacks it into the cache, and keeps it there for the
//! next run. The cache names each release by its SHA-256 too,
//! so a release published again under the same version is downloaded again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

use super::Reporter;
use super::release::MANIFEST;

/// Where releases are published, as `<base>/v<version>/<file>`.
pub const RELEASES: &str = "https://github.com/soorower/forkrail-cli/releases/download";
/// Another place to take releases from (a mirror, or a test), in the same layout.
pub const RELEASES_ENV: &str = "FORKRAIL_RELEASES_URL";
/// A release is about 1 GB; more than this is refused.
const MOST_BYTES: u64 = 4 << 30;

/// The release's SHA-256 as deploy/release/publish.sh built it into this CLI: it packs the release
/// first and builds the CLIs after, so each CLI names the one release it installs. What's
/// downloaded must be that, whatever is published beside it: the `.sha256` next to a release on
/// GitHub only shows a download wasn't cut short, since whoever could change the release could
/// change it too, and the release is the routers', functions' and dashboard's code, run in the
/// account with its keys. A CLI built without it (`cargo build`, tests) takes the published one.
fn pinned() -> Option<&'static str> {
    option_env!("FORKRAIL_RELEASE_SHA256").filter(|s| is_sha256(s))
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// This installer's version, which the release it takes must be.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The release file's name for `version`, and the folder inside it.
pub fn file_name(version: &str) -> String {
    format!("forkrail-release-{version}.tar")
}

fn base() -> String {
    std::env::var(RELEASES_ENV).ok().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| RELEASES.to_owned()).trim_end_matches('/').to_owned()
}

/// This version's published release: its URL, and its SHA-256: the one built into this CLI
/// (`pinned`), else the one published beside it. The account's copy and a download here are both
/// checked against it.
pub async fn published() -> Result<(String, String)> {
    let version = version();
    let releases = releases_dir()?;
    let name = file_name(version);
    let url = format!("{}/v{version}/{name}", base());
    if let Some(sha256) = pinned() {
        return Ok((url, sha256.to_owned()));
    }
    let http = crate::fetch::Http::new()?;
    let sha256 = published_sha256(&http, &url, &releases.join(format!("{name}.sha256.{}", forkrail_common::random_hex(4)))).await?;
    Ok((url, sha256))
}

fn releases_dir() -> Result<PathBuf> {
    let releases = crate::package::cache_dir()?.join("releases");
    std::fs::create_dir_all(&releases).with_context(|| format!("creating {}", releases.display()))?;
    Ok(releases)
}

/// The release folder for this version: from the cache, or downloaded into it first.
pub async fn release(report: &Reporter) -> Result<PathBuf> {
    let releases = releases_dir()?;
    match published().await {
        Ok((url, wanted)) => fetch(report, &url, &wanted).await,
        // Offline, the newest one kept for this version does.
        Err(e) => kept(&releases, version()).ok_or(e),
    }
}

/// The release at `url`, whose SHA-256 is `wanted`: from the cache, or downloaded into it first.
pub async fn fetch(report: &Reporter, url: &str, wanted: &str) -> Result<PathBuf> {
    let version = version();
    let releases = releases_dir()?;
    let name = file_name(version);
    let http = crate::fetch::Http::new()?;
    let dir = releases.join(format!("{version}-{}", &wanted[..12]));
    if dir.join(MANIFEST).is_file() {
        return Ok(dir);
    }

    report.note(format!("     Downloading forkrail {version} (about 1 GB, once; it's kept for later runs)…"));
    let part = releases.join(format!("{name}.part"));
    let shown = AtomicU64::new(0);
    let progress = |got: u64, whole: Option<u64>| {
        // A line for every tenth, or every 100 MB when the size isn't known.
        let step = whole.map_or(100 << 20, |w| (w / 10).max(1));
        let mark = got / step;
        if mark > shown.load(Ordering::Relaxed) {
            shown.store(mark, Ordering::Relaxed);
            let text = match whole {
                Some(w) => format!("     downloaded {} of {} MB", got >> 20, w >> 20),
                None => format!("     downloaded {} MB", got >> 20),
            };
            report.note(text);
        }
    };
    http.download_watched(url, &[], &part, MOST_BYTES, &progress).await.with_context(|| format!("downloading {url}"))?;

    let got = super::release::sha256_file(&part)?;
    if got != wanted {
        let _ = std::fs::remove_file(&part);
        if pinned().is_some() {
            bail!(
                "{url} isn't the release this forkrail {version} was built for (it has SHA-256 {got}, not {wanted}): nothing was installed. If it happens again, the published release isn't the one this CLI came with: get the CLI again from https://forkrail.com/install"
            );
        }
        bail!("{url} isn't the release its SHA-256 names (got {got}): try again");
    }
    let unpacking = releases.join(format!("{version}.unpacking"));
    let _ = std::fs::remove_dir_all(&unpacking);
    unpack(&part, &unpacking).with_context(|| format!("unpacking {}", part.display()))?;
    let top = unpacking.join(format!("forkrail-release-{version}"));
    if !top.join(MANIFEST).is_file() {
        bail!("{url} has no forkrail-release-{version}/{MANIFEST}");
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&top, &dir).with_context(|| format!("moving the release to {}", dir.display()))?;
    let _ = std::fs::remove_dir_all(&unpacking);
    let _ = std::fs::remove_file(&part);
    Ok(dir)
}

/// The SHA-256 published beside the release at `url`.
async fn published_sha256(http: &crate::fetch::Http, url: &str, into: &Path) -> Result<String> {
    http.download(&format!("{url}.sha256"), &[], into, 4096).await.with_context(|| format!("downloading {url}.sha256"))?;
    let text = std::fs::read_to_string(into);
    let _ = std::fs::remove_file(into);
    let wanted = text?.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    if wanted.len() != 64 || !wanted.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{url}.sha256 holds no SHA-256");
    }
    Ok(wanted)
}

/// The newest release of `version` the cache has, whole.
fn kept(releases: &Path, version: &str) -> Option<PathBuf> {
    let prefix = format!("{version}-");
    std::fs::read_dir(releases)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix) && e.path().join(MANIFEST).is_file())
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// Unpacks the tar file `from` into `into`, refusing paths that would land outside it.
fn unpack(from: &Path, into: &Path) -> Result<()> {
    std::fs::create_dir_all(into)?;
    let mut archive = tar::Archive::new(std::fs::File::open(from)?);
    for entry in archive.entries()? {
        let mut entry = entry?;
        // `unpack_in` skips any path with `..` or a root in it.
        entry.unpack_in(into)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_release_is_a_sha256() {
        assert!(is_sha256(&"ab01".repeat(16)));
        assert!(!is_sha256(&"AB01".repeat(16)), "lowercase, as shasum writes it");
        assert!(!is_sha256("ab01"));
        assert!(!is_sha256(&"g".repeat(64)));
        if let Some(sha) = pinned() {
            assert!(is_sha256(sha));
        }
    }

    #[test]
    fn a_release_is_named_by_version() {
        assert_eq!(file_name("0.1.0"), "forkrail-release-0.1.0.tar");
        assert!(!version().is_empty());
    }

    #[test]
    fn offline_the_newest_whole_release_of_the_version_is_used() {
        let dir = crate::package::TempDir::new("kept-test").unwrap();
        assert_eq!(kept(dir.path(), "1.0.0"), None);
        for name in ["1.0.0-aaaaaaaaaaaa", "1.0.0-bbbbbbbbbbbb", "1.0.1-cccccccccccc"] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join(name).join(MANIFEST), "{}").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::fs::create_dir_all(dir.path().join("1.0.0-dddddddddddd")).unwrap();
        assert_eq!(kept(dir.path(), "1.0.0"), Some(dir.path().join("1.0.0-bbbbbbbbbbbb")));
        assert_eq!(kept(dir.path(), "1.0.2"), None);
    }

    #[test]
    fn unpacks_a_release_file() {
        let dir = crate::package::TempDir::new("release-test").unwrap();
        let src = dir.path().join("forkrail-release-9.9.9");
        std::fs::create_dir_all(src.join("builder")).unwrap();
        std::fs::write(src.join(MANIFEST), "{}").unwrap();
        std::fs::write(src.join("builder/index.json"), "{}").unwrap();
        let file = dir.path().join("r.tar");
        {
            let mut tar = tar::Builder::new(std::fs::File::create(&file).unwrap());
            tar.append_dir_all("forkrail-release-9.9.9", &src).unwrap();
            tar.finish().unwrap();
        }
        let into = dir.path().join("out");
        unpack(&file, &into).unwrap();
        assert!(into.join("forkrail-release-9.9.9").join(MANIFEST).is_file());
        assert!(into.join("forkrail-release-9.9.9/builder/index.json").is_file());
    }
}
