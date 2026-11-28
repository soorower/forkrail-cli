//! Hugo for a build, as Vercel's static-build gets it: the release `HUGO_VERSION` names (else
//! Vercel's default, 0.58.2) is downloaded from Hugo's GitHub releases at build time into the
//! project's `.vercel/cache/hugo-v<version>-<os>-<arch>`, which the build cache keeps
//! (framework.rs `CACHES`), and put first on the build's PATH. The archive is the one Vercel picks:
//! the extended edition when there's one for this machine, else the plain one. Unlike Vercel, its
//! SHA-256 is checked against the release's checksums file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::buildlog::BuildLog;
use crate::fetch::Http;

/// Vercel's, when the project doesn't set HUGO_VERSION (`@vercel/static-build` 28.0.0).
pub const DEFAULT_VERSION: &str = "0.58.2";
/// Bigger than any Hugo archive (the largest, with deploy, are about 30 MB).
const MAX_BYTES: u64 = 200 << 20;

/// Make Hugo `version` (or the default) available for a build in `dir`; the folder it's in.
pub fn install(dir: &Path, version: Option<&str>, log: &dyn BuildLog) -> Result<PathBuf> {
    let version = version.map(str::trim).filter(|v| !v.is_empty()).unwrap_or(DEFAULT_VERSION);
    let version = version.strip_prefix('v').unwrap_or(version);
    if !version.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
        bail!("HUGO_VERSION {version:?} isn't a Hugo version (such as 0.120.4)");
    }
    let (os, arch) = (node_platform(), node_arch());
    let cache = dir.join(".vercel").join("cache");
    let name = format!("hugo-v{version}-{os}-{arch}");
    let bin = cache.join(&name);
    if bin.join("hugo").is_file() {
        log.info(&format!("Hugo {version}: kept from an earlier build"));
        return Ok(bin);
    }
    log.info(&format!("Installing Hugo version {version}"));
    std::fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    // Only this version's is kept: the cache would otherwise carry every version the project used.
    for entry in std::fs::read_dir(&cache)?.flatten() {
        if entry.file_name().to_string_lossy().starts_with("hugo-v") {
            std::fs::remove_dir_all(entry.path()).ok();
        }
    }
    let staging = cache.join(format!(".{name}.partial"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let fetched = fetch(version, os, arch, &staging);
    let file = match fetched {
        Ok(file) => file,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let unpacked = staging.join("hugo-files");
    unpack(&staging.join("archive.tar.gz"), &unpacked).with_context(|| format!("unpacking {file}"))?;
    if !unpacked.join("hugo").is_file() {
        bail!("{file} has no `hugo` at its top");
    }
    std::fs::rename(&unpacked, &bin).with_context(|| format!("moving Hugo into {}", bin.display()))?;
    let _ = std::fs::remove_dir_all(&staging);
    log.info(&format!("Hugo {version}: {file}"));
    Ok(bin)
}

/// Download the archive for `version` into `into/archive.tar.gz` and check it; its file name.
fn fetch(version: &str, os: &str, arch: &str, into: &Path) -> Result<String> {
    let base = format!("https://github.com/gohugoio/hugo/releases/download/v{version}");
    let http = Http::new()?;
    let sums_path = into.join("checksums.txt");
    let got = block_on(http.download(&format!("{base}/hugo_{version}_checksums.txt"), &[], &sums_path, 1 << 20));
    if let Err(e) = got {
        if e.downcast_ref::<crate::fetch::Status>().is_some_and(|s| s.0 == 404) {
            bail!("Version {version} of Hugo does not exist. Please specify a different one.");
        }
        return Err(e).context("downloading Hugo's checksums");
    }
    let sums = std::fs::read_to_string(&sums_path).context("reading Hugo's checksums")?;
    let sums: Vec<(&str, &str)> = sums.lines().filter_map(|l| l.split_once(char::is_whitespace)).map(|(sha, name)| (sha.trim(), name.trim())).collect();
    let file = pick(sums.iter().map(|(_, name)| *name), os, arch).with_context(|| format!("Hugo {version} has no build for {os} {arch}"))?;
    let want = sums.iter().find(|(_, name)| *name == file).map(|(sha, _)| sha.to_ascii_lowercase()).expect("picked from the list");
    let archive = into.join("archive.tar.gz");
    block_on(http.download(&format!("{base}/{file}"), &[], &archive, MAX_BYTES)).with_context(|| format!("downloading {file}"))?;
    let got = forkrail_common::hex(&Sha256::digest(std::fs::read(&archive)?));
    if got != want {
        bail!("{file} isn't the one Hugo's checksums name (SHA-256 {got}, not {want})");
    }
    Ok(file.to_owned())
}

/// The archive Vercel's `getHugoUrl` picks among a release's files: the first extended one for
/// this OS and architecture, else the first plain one. Names end `_<os>-<arch>.tar.gz`
/// (`_linux-arm64`, and `_Linux-ARM64`, `_macOS-64bit` in old releases).
pub fn pick<'a>(names: impl Iterator<Item = &'a str> + Clone, os: &str, arch: &str) -> Option<&'a str> {
    let oses: &[&str] = match os {
        "linux" => &["linux"],
        "darwin" => &["darwin", "macos"],
        _ => &[],
    };
    let mut arches: Vec<&str> = match arch {
        "arm64" => vec!["arm64"],
        "x64" => vec!["amd64", "64bit"],
        _ => Vec::new(),
    };
    if os == "darwin" {
        arches.push("universal");
        if arch == "arm64" {
            arches.push("64bit");
        }
    }
    let find = |extended: bool| {
        let prefix = if extended { "hugo_extended_" } else { "hugo_" };
        names.clone().find(|name| {
            if !name.starts_with(prefix) || !name.ends_with(".tar.gz") {
                return false;
            }
            let suffix = name.rsplit('_').next().unwrap_or("");
            let target = suffix.split('.').next().unwrap_or("").to_ascii_lowercase();
            let mut parts = target.split('-');
            let (o, a) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            oses.contains(&o) && arches.contains(&a)
        })
    };
    find(true).or_else(|| find(false))
}

/// Node's names for this machine, which Vercel's folder and choice use.
fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        arch => arch,
    }
}

/// The archive's files into `into`, refusing any that would land outside it.
fn unpack(archive: &Path, into: &Path) -> Result<()> {
    std::fs::create_dir_all(into)?;
    let file = std::fs::File::open(archive)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::io::BufReader::new(file)));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.unpack_in(into)? {
            bail!("{} would be written outside the folder", entry.path()?.display());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hugo = into.join("hugo");
        if hugo.is_file() {
            std::fs::set_permissions(&hugo, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

/// Builds run on a blocking thread of the CLI's runtime (build.rs's callers), whose handle can
/// wait for a download; a test has none, and gets its own.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(future),
        Err(_) => tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime").block_on(future),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_archive_vercel_picks() {
        // Hugo 0.58.2's Linux and macOS files, and 0.170.0's.
        let old = [
            "hugo_0.58.2_Linux-32bit.tar.gz",
            "hugo_0.58.2_Linux-64bit.tar.gz",
            "hugo_0.58.2_Linux-ARM.tar.gz",
            "hugo_0.58.2_Linux-ARM64.tar.gz",
            "hugo_0.58.2_macOS-64bit.tar.gz",
            "hugo_extended_0.58.2_Linux-64bit.tar.gz",
            "hugo_extended_0.58.2_macOS-64bit.tar.gz",
        ];
        let new = [
            "hugo_0.170.0_darwin-universal.tar.gz",
            "hugo_0.170.0_linux-amd64.tar.gz",
            "hugo_0.170.0_linux-arm64.tar.gz",
            "hugo_extended_0.170.0_darwin-universal.tar.gz",
            "hugo_extended_0.170.0_linux-amd64.tar.gz",
            "hugo_extended_0.170.0_linux-arm64.tar.gz",
            "hugo_extended_withdeploy_0.170.0_linux-arm64.tar.gz",
        ];
        // The builder (Linux arm64): 0.58.2 has no extended build for it, Vercel's x86_64 does.
        assert_eq!(pick(old.iter().copied(), "linux", "arm64"), Some("hugo_0.58.2_Linux-ARM64.tar.gz"));
        assert_eq!(pick(old.iter().copied(), "linux", "x64"), Some("hugo_extended_0.58.2_Linux-64bit.tar.gz"));
        assert_eq!(pick(new.iter().copied(), "linux", "arm64"), Some("hugo_extended_0.170.0_linux-arm64.tar.gz"));
        // A Mac: macOS-64bit (Intel) for an old one, universal for a new one.
        assert_eq!(pick(old.iter().copied(), "darwin", "arm64"), Some("hugo_extended_0.58.2_macOS-64bit.tar.gz"));
        assert_eq!(pick(new.iter().copied(), "darwin", "arm64"), Some("hugo_extended_0.170.0_darwin-universal.tar.gz"));
        assert_eq!(pick(old.iter().copied(), "windows", "x64"), None);
    }

    #[test]
    fn refuses_what_isnt_a_version() {
        struct Quiet;
        impl BuildLog for Quiet {
            fn line(&self, _: crate::buildlog::Stream, _: &str) {}
        }
        let dir = std::env::temp_dir().join(format!("forkrail-hugo-{}", forkrail_common::random_hex(6)));
        for bad in ["latest", "0.58.2/../x", "1..2"] {
            assert!(install(&dir, Some(bad), &Quiet).unwrap_err().to_string().contains("isn't a Hugo version"), "{bad}");
        }
    }
}
