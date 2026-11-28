//! A deployment's source code, for the dashboard's Source view: the project's own files as its
//! folder has them, never its dependencies or build output.
//!
//! A build keeps them (`keep`): what `.gitignore` and `.vercelignore` leave, without dependencies,
//! build output, caches, `.env*` or files whose names say they hold secrets, zipped as `source.zip`
//! beside the package. Publishing stores the zip by content, `sources/<project>/<sha256>.zip`, and
//! the deployment's record names it. Deployments made before that have none; an API's package holds
//! its files as they are (under `app/`, beside its dependencies), so those are read from there, and a
//! framework's package holds only its build output, so there's nothing to show.
//!
//! The zips are read in ranges from S3, never whole: the central directory to list the files, and
//! one entry when a file is opened. Only a name the listing has can be read.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use aws_sdk_s3::error::DisplayErrorContext;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use crate::buildlog::BuildLog;
use crate::deploy::{Ctx, record};
use crate::framework;
use crate::package;
use crate::state::{DeploymentRecord, GitCommit};

/// The build folder's copy of the project's files.
pub const ARCHIVE_FILE: &str = "source.zip";
/// More than this isn't kept: the deploy goes ahead without its source.
const MAX_FILES: usize = 20_000;
const MAX_BYTES: u64 = 100 << 20;
/// Names never kept, wherever they are: version control, dependencies, build output, caches.
const SKIPPED: [&str; 19] = [
    ".git",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".DS_Store",
    ".vercel",
    ".output",
    ".nitro",
    ".tanstack",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".cache",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".parcel-cache",
];
/// The most of one file shown, in bytes.
const MAX_SHOWN: usize = 1 << 20;
/// The most of one compressed entry read to show its start.
const MAX_COMPRESSED_READ: u64 = 8 << 20;
/// The end of a zip: its end record, a comment of up to 64 KB, and Zip64's locator.
const TAIL: u64 = 22 + 65_535 + 20;

/// The source kept with a deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Archive {
    /// Hex SHA-256 of the zip, its name in S3.
    pub sha256: String,
    pub files: usize,
    /// The files' total size, unzipped.
    pub bytes: u64,
}

/// Where a project's kept source is, by its hash: a project deployed again unchanged keeps one copy.
pub fn archive_key(project: &str, sha256: &str) -> String {
    format!("sources/{project}/{sha256}.zip")
}

/// Zip the project's own files in `dir` into `out/source.zip`. `None`, with a note in the build
/// log, if there are too many to keep: the deploy doesn't depend on it.
pub fn keep(dir: &Path, out: &Path, log: &dyn BuildLog) -> Result<Option<Archive>> {
    let files = collect(dir)?;
    let bytes: u64 = files.values().map(|p| std::fs::metadata(p).map_or(0, |m| m.len())).sum();
    if files.len() > MAX_FILES || bytes > MAX_BYTES {
        log.info(&format!(
            "source: not kept ({} files, {:.0} MB; at most {MAX_FILES} files and {} MB), so the dashboard won't show it",
            files.len(),
            bytes as f64 / 1e6,
            MAX_BYTES >> 20
        ));
        return Ok(None);
    }
    let path = out.join(ARCHIVE_FILE);
    let bytes = package::write_zip(&path, &files)?;
    let sha256 = forkrail_common::hex(&package::file_sha256(&path)?);
    log.info(&format!("source: {} files ({:.1} MB) kept for the dashboard", files.len(), bytes as f64 / 1e6));
    Ok(Some(Archive { sha256, files: files.len(), bytes }))
}

/// A remote build's source at most: Vercel Pro's 1 GB.
const MAX_UPLOAD_FILES: usize = 100_000;
const MAX_UPLOAD_BYTES: u64 = 1 << 30;

/// Zip what a remote build of the project in `dir` gets into `path`: the files a git checkout of
/// it would have, as its ignore files say, without dependencies, build output and caches, and
/// without `.env.local` and `.env.*.local`, which Vercel never uploads either. Unlike the copy kept
/// for the dashboard, files whose names say they may hold secrets go too (a committed `.env`, an
/// `.npmrc`): the build may need them, as it would from git.
pub fn upload_archive(dir: &Path, path: &Path) -> Result<Archive> {
    let files = collect_files(dir, |name| {
        let name = name.to_ascii_lowercase();
        name == ".env.local" || (name.starts_with(".env.") && name.ends_with(".local"))
    })?;
    let bytes: u64 = files.values().map(|p| std::fs::metadata(p).map_or(0, |m| m.len())).sum();
    if files.len() > MAX_UPLOAD_FILES || bytes > MAX_UPLOAD_BYTES {
        bail!(
            "{} files, {:.0} MB: a remote build takes at most {MAX_UPLOAD_FILES} files and {} MB; add what it doesn't need to .vercelignore, or build with --local",
            files.len(),
            bytes as f64 / 1e6,
            MAX_UPLOAD_BYTES >> 20
        );
    }
    if files.is_empty() {
        bail!("{} has no files to build", dir.display());
    }
    let bytes = package::write_zip(path, &files)?;
    let sha256 = forkrail_common::hex(&package::file_sha256(path)?);
    Ok(Archive { sha256, files: files.len(), bytes })
}

/// The project's own files, by their paths in it.
fn collect(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    collect_files(dir, withheld)
}

/// The project's files as its ignore files leave them, less `SKIPPED` and files whose name `skip`
/// is true for.
fn collect_files(dir: &Path, skip: impl Fn(&str) -> bool + Send + Sync + 'static) -> Result<BTreeMap<String, PathBuf>> {
    let walker = WalkBuilder::new(dir)
        .standard_filters(false)
        // The project's own ignore files, as git reads them, even outside a repository; not its
        // parents', which belong to whatever folder it sits in.
        .git_ignore(true)
        .git_exclude(true)
        .ignore(true)
        .require_git(false)
        .parents(false)
        .add_custom_ignore_filename(".vercelignore")
        .follow_links(false)
        .filter_entry(move |entry| {
            let name = entry.file_name().to_string_lossy();
            let file = entry.file_type().is_some_and(|t| t.is_file());
            !SKIPPED.contains(&name.as_ref()) && !(file && skip(&name))
        })
        .build();
    let mut files = BTreeMap::new();
    for entry in walker {
        let entry = entry.context("listing the project's files")?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            let rel = entry.path().strip_prefix(dir).expect("walked from dir").to_string_lossy().replace('\\', "/");
            files.insert(rel, entry.path().to_path_buf());
        }
    }
    Ok(files)
}

/// Where a deployment's source comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Kept when it was deployed.
    Kept,
    /// Its package, which holds an API's files as they are.
    Package,
    /// Not kept: a framework's build output, deployed before source was kept.
    None,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    pub id: String,
    pub project: String,
    pub origin: Origin,
    /// The project's files, by their paths in its folder.
    pub files: Vec<Entry>,
    pub git: Option<GitCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct FileView {
    pub path: String,
    pub size: u64,
    /// Its text, up to 1 MB; none for a binary file or a withheld one.
    pub text: Option<String>,
    pub truncated: bool,
    pub binary: bool,
    /// Not shown, because its name says it may hold secrets.
    pub withheld: bool,
}

/// Where to read a deployment's source.
enum Place {
    Archive(String),
    Package(String),
    Nothing,
}

fn place(rec: &DeploymentRecord) -> Place {
    match (&rec.kept_source, &rec.framework) {
        (Some(archive), _) => Place::Archive(archive_key(&rec.project, &archive.sha256)),
        (None, Some(f)) if framework::builds(f) => Place::Nothing,
        // Never published (still building, or failed): no package either.
        (None, _) if rec.package_key.is_empty() => Place::Nothing,
        (None, _) => Place::Package(rec.package_key.clone()),
    }
}

/// A package entry's path in the project, if it's one of the project's own files: under `app/`,
/// and not dependencies or bytecode.
fn project_path(name: &str) -> Option<&str> {
    let rel = name.strip_prefix("app/").filter(|r| !r.is_empty() && !r.ends_with('/'))?;
    let ours = !rel.split('/').any(|part| SKIPPED.contains(&part)) && !rel.ends_with(".pyc");
    ours.then_some(rel)
}

pub async fn list(ctx: &Ctx, id: &str) -> Result<Listing> {
    let rec = record(ctx, id).await?;
    let (origin, files) = match place(&rec) {
        Place::Archive(key) => {
            let zip = Zip::open(ctx, &key).await?;
            (Origin::Kept, zip.entries.into_iter().filter(|e| !e.name.ends_with('/')).map(|e| Entry { path: e.name, size: e.size }).collect())
        }
        Place::Package(key) => {
            let zip = Zip::open(ctx, &key).await?;
            let files = zip.entries.iter().filter_map(|e| project_path(&e.name).map(|path| Entry { path: path.to_owned(), size: e.size })).collect();
            (Origin::Package, files)
        }
        Place::Nothing => (Origin::None, Vec::new()),
    };
    Ok(Listing { id: rec.id, project: rec.project, origin, files, git: rec.git })
}

/// One of the deployment's source files, by its path in the project.
pub async fn file(ctx: &Ctx, id: &str, path: &str) -> Result<FileView> {
    let rec = record(ctx, id).await?;
    let (zip, name) = match place(&rec) {
        Place::Archive(key) => (Zip::open(ctx, &key).await?, path.to_owned()),
        Place::Package(key) => (Zip::open(ctx, &key).await?, format!("app/{path}")),
        Place::Nothing => bail!("deployment {id} has no source kept"),
    };
    let entry = zip
        .entries
        .iter()
        .find(|e| e.name == name && (name == path || project_path(&e.name).is_some()))
        .with_context(|| format!("deployment {id}'s source has no {path}"))?;
    if withheld(path) {
        return Ok(FileView { path: path.to_owned(), size: entry.size, text: None, truncated: false, binary: false, withheld: true });
    }
    let (head, truncated) = zip.read(entry).await?;
    let (text, binary) = as_text(&head[..head.len().min(MAX_SHOWN)], truncated);
    Ok(FileView { path: path.to_owned(), size: entry.size, text, truncated, binary, withheld: false })
}

/// Names that may hold secrets: never kept, and never shown from an older package.
fn withheld(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let example = [".example", ".sample", ".template", ".dist"].iter().any(|s| name.ends_with(s));
    (name.starts_with(".env") && !example)
        || [".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"].iter().any(|s| name.ends_with(s))
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || [".npmrc", ".pypirc", ".netrc", ".git-credentials", "credentials", "credentials.json"].contains(&name.as_str())
}

/// The text of a file's first bytes, or none if it's binary. A file cut short may end inside a
/// character, which is dropped.
fn as_text(head: &[u8], truncated: bool) -> (Option<String>, bool) {
    if head[..head.len().min(8192)].contains(&0) {
        return (None, true);
    }
    match std::str::from_utf8(head) {
        Ok(text) => (Some(text.to_owned()), false),
        Err(e) if truncated && e.error_len().is_none() => (Some(String::from_utf8_lossy(&head[..e.valid_up_to()]).into_owned()), false),
        Err(_) => (None, true),
    }
}

/// Bytes `start..end` of an object in the install's bucket.
async fn range(ctx: &Ctx, key: &str, start: u64, end: u64) -> Result<Vec<u8>> {
    if end <= start {
        return Ok(Vec::new());
    }
    let out = ctx
        .store
        .s3
        .get_object()
        .bucket(&ctx.store.bucket)
        .key(key)
        .range(format!("bytes={start}-{}", end - 1))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("reading s3://{}/{key}: {}", ctx.store.bucket, DisplayErrorContext(&e)))?;
    Ok(out.body.collect().await.with_context(|| format!("reading s3://{}/{key}", ctx.store.bucket))?.into_bytes().to_vec())
}

/// One entry of a zip's central directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ZipEntry {
    name: String,
    /// Uncompressed.
    size: u64,
    compressed: u64,
    /// 0 stored, 8 deflated.
    method: u16,
    /// Where its local header starts.
    offset: u64,
}

/// A zip in S3, by its central directory.
struct Zip<'a> {
    ctx: &'a Ctx,
    key: String,
    entries: Vec<ZipEntry>,
}

impl<'a> Zip<'a> {
    async fn open(ctx: &'a Ctx, key: &str) -> Result<Self> {
        let head = ctx
            .store
            .s3
            .head_object()
            .bucket(&ctx.store.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("s3://{}/{key}: {}", ctx.store.bucket, DisplayErrorContext(&e)))?;
        let len = head.content_length().and_then(|l| u64::try_from(l).ok()).context("the zip has no length")?;
        let tail = range(ctx, key, len.saturating_sub(TAIL), len).await?;
        let (cd_offset, cd_size) = match central_directory(&tail)? {
            Directory::At { offset, size } => (offset, size),
            // The Zip64 end record is 56 bytes where the locator says.
            Directory::Zip64 { locator } => zip64_directory(&range(ctx, key, locator, locator + 56).await?)?,
        };
        if cd_offset.checked_add(cd_size).is_none_or(|end| end > len) {
            bail!("{key}: the zip's central directory is past its end");
        }
        let cd = range(ctx, key, cd_offset, cd_offset + cd_size).await?;
        Ok(Self { ctx, key: key.to_owned(), entries: entries(&cd)? })
    }

    /// The start of an entry, uncompressed, up to `MAX_SHOWN + 1` bytes; and whether there's more.
    async fn read(&self, entry: &ZipEntry) -> Result<(Vec<u8>, bool)> {
        let header = range(self.ctx, &self.key, entry.offset, entry.offset + 30).await?;
        if header.len() < 30 || u32_at(&header, 0) != 0x0403_4b50 {
            bail!("{}: no local header for {}", self.key, entry.name);
        }
        let data = entry.offset + 30 + u64::from(u16_at(&header, 26)) + u64::from(u16_at(&header, 28));
        let want = MAX_SHOWN as u64 + 1;
        let out = match entry.method {
            0 => range(self.ctx, &self.key, data, data + entry.compressed.min(want)).await?,
            8 => {
                let compressed = range(self.ctx, &self.key, data, data + entry.compressed.min(MAX_COMPRESSED_READ)).await?;
                let mut out = Vec::new();
                let mut decoder = flate2::read::DeflateDecoder::new(compressed.as_slice()).take(want);
                // A stream cut short to `MAX_COMPRESSED_READ` ends in an error; what came before it stands.
                if let Err(e) = decoder.read_to_end(&mut out)
                    && entry.compressed <= MAX_COMPRESSED_READ
                {
                    bail!("{}: {} doesn't inflate: {e}", self.key, entry.name);
                }
                out
            }
            m => bail!("{}: {} is compressed with method {m}, which forkrail doesn't read", self.key, entry.name),
        };
        let truncated = out.len() > MAX_SHOWN || (out.len() as u64) < entry.size;
        Ok((out, truncated))
    }
}

enum Directory {
    At { offset: u64, size: u64 },
    /// Zip64: its end record is at `locator`.
    Zip64 { locator: u64 },
}

/// Find the central directory from the zip's last bytes.
fn central_directory(tail: &[u8]) -> Result<Directory> {
    let end = (0..tail.len().saturating_sub(21)).rev().find(|&i| u32_at(tail, i) == 0x0605_4b50).context("not a zip: no end of central directory")?;
    let (entries, size, offset) = (u16_at(tail, end + 10), u32_at(tail, end + 12), u32_at(tail, end + 16));
    if entries == 0xFFFF || size == 0xFFFF_FFFF || offset == 0xFFFF_FFFF {
        let at = end.checked_sub(20).filter(|&i| u32_at(tail, i) == 0x0706_4b50).context("a Zip64 archive without its locator")?;
        return Ok(Directory::Zip64 { locator: u64_at(tail, at + 8) });
    }
    Ok(Directory::At { offset: u64::from(offset), size: u64::from(size) })
}

fn zip64_directory(record: &[u8]) -> Result<(u64, u64)> {
    if record.len() < 56 || u32_at(record, 0) != 0x0606_4b50 {
        bail!("not a Zip64 end of central directory");
    }
    Ok((u64_at(record, 48), u64_at(record, 40)))
}

fn entries(cd: &[u8]) -> Result<Vec<ZipEntry>> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i + 46 <= cd.len() && u32_at(cd, i) == 0x0201_4b50 {
        let method = u16_at(cd, i + 10);
        let mut compressed = u64::from(u32_at(cd, i + 20));
        let mut size = u64::from(u32_at(cd, i + 24));
        let (name_len, extra_len, comment_len) = (usize::from(u16_at(cd, i + 28)), usize::from(u16_at(cd, i + 30)), usize::from(u16_at(cd, i + 32)));
        let mut offset = u64::from(u32_at(cd, i + 42));
        let name_end = i + 46 + name_len;
        let extra_end = name_end + extra_len;
        if extra_end + comment_len > cd.len() {
            bail!("the zip's central directory is cut short");
        }
        let name = String::from_utf8_lossy(&cd[i + 46..name_end]).into_owned();
        // Zip64's extra field holds, in this order, whichever of the three didn't fit.
        let mut j = name_end;
        while j + 4 <= extra_end {
            let (id, len) = (u16_at(cd, j), usize::from(u16_at(cd, j + 2)));
            if id == 0x0001 {
                let mut k = j + 4;
                for field in [&mut size, &mut compressed, &mut offset] {
                    if *field == 0xFFFF_FFFF && k + 8 <= j + 4 + len {
                        *field = u64_at(cd, k);
                        k += 8;
                    }
                }
            }
            j += 4 + len;
        }
        entries.push(ZipEntry { name, size, compressed, method, offset });
        i = extra_end + comment_len;
    }
    Ok(entries)
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    b.get(i..i + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    b.get(i..i + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    b.get(i..i + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().expect("8 bytes")))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use super::*;
    use crate::buildlog::Terminal;

    fn zip(files: &[(&str, &[u8], zip::CompressionMethod)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body, method) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default().compression_method(*method)).unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// The parts of `Zip::read` that don't touch S3, on a zip in memory.
    fn read(bytes: &[u8], entry: &ZipEntry) -> Vec<u8> {
        let data = entry.offset as usize + 30 + usize::from(u16_at(bytes, entry.offset as usize + 26)) + usize::from(u16_at(bytes, entry.offset as usize + 28));
        let raw = &bytes[data..data + entry.compressed as usize];
        match entry.method {
            0 => raw.to_vec(),
            _ => {
                let mut out = Vec::new();
                flate2::read::DeflateDecoder::new(raw).read_to_end(&mut out).unwrap();
                out
            }
        }
    }

    fn listing(bytes: &[u8]) -> Vec<ZipEntry> {
        let tail_start = bytes.len().saturating_sub(TAIL as usize);
        let Directory::At { offset, size } = central_directory(&bytes[tail_start..]).unwrap() else { panic!("not zip64") };
        entries(&bytes[offset as usize..(offset + size) as usize]).unwrap()
    }

    #[test]
    fn reads_a_zip_by_its_central_directory() {
        let text = "print('hello')\n".repeat(500);
        let bytes = zip(&[
            ("bootstrap", b"\x7fELF\0\0", zip::CompressionMethod::Stored),
            ("app/api/main.py", text.as_bytes(), zip::CompressionMethod::Deflated),
            ("python/x/__init__.py", b"", zip::CompressionMethod::Stored),
        ]);
        let entries = listing(&bytes);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["bootstrap", "app/api/main.py", "python/x/__init__.py"]);
        assert_eq!(entries[1].size, text.len() as u64);
        assert!(entries[1].compressed < entries[1].size);
        assert_eq!(read(&bytes, &entries[1]), text.as_bytes());
        assert_eq!(as_text(&read(&bytes, &entries[0]), false), (None, true));
    }

    #[test]
    fn a_package_shows_only_the_projects_own_files() {
        assert_eq!(project_path("app/api/main.py"), Some("api/main.py"));
        assert_eq!(project_path("app/requirements.txt"), Some("requirements.txt"));
        for not_ours in ["app/api/__pycache__/main.cpython-313.pyc", "app/node_modules/x/index.js", "python/fastapi/__init__.py", "bootstrap", "app/", "app/x.pyc"] {
            assert_eq!(project_path(not_ours), None, "{not_ours}");
        }
    }

    #[test]
    fn keeps_the_folder_as_the_laptop_has_it() {
        let dir = std::env::temp_dir().join(format!("forkrail-source-{}", forkrail_common::random_hex(4)));
        let write = |rel: &str, body: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write("src/app.tsx", "export {}");
        write("package.json", "{}");
        write(".gitignore", "dist/\n*.log\n");
        write(".env", "SECRET=1");
        write(".env.example", "SECRET=");
        write("certs/server.pem", "-----");
        write("node_modules/react/index.js", "x");
        write(".output/server/index.mjs", "x");
        write("dist/bundle.js", "x");
        write("debug.log", "x");
        write(".vercelignore", "scripts/\n");
        write("scripts/seed.py", "x");
        let kept: Vec<String> = collect(&dir).unwrap().into_keys().collect();
        assert_eq!(kept, [".env.example", ".gitignore", ".vercelignore", "package.json", "src/app.tsx"]);

        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let archive = keep(&dir.join("src"), &out, &Terminal).unwrap().unwrap();
        assert_eq!((archive.files, archive.bytes), (1, 9));
        let again = keep(&dir.join("src"), &dir, &Terminal).unwrap().unwrap();
        assert_eq!(again.sha256, archive.sha256, "the same files make the same zip");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn text_cut_inside_a_character_keeps_what_came_before() {
        let text = "héllo".as_bytes();
        assert_eq!(as_text(&text[..2], true), (Some("h".to_owned()), false));
        assert_eq!(as_text(&text[..2], false), (None, true));
        assert_eq!(as_text(b"plain", false), (Some("plain".to_owned()), false));
    }

    #[test]
    fn secret_looking_names_are_withheld() {
        for name in [".env", ".env.production", "certs/server.pem", "tls.key", ".npmrc", "id_rsa", "credentials.json"] {
            assert!(withheld(name), "{name}");
        }
        for name in [".env.example", "api/main.py", "keys.py", "package.json"] {
            assert!(!withheld(name), "{name}");
        }
    }
}
