//! A project's build cache: what a build in the builder keeps for the project's next one
//! (builder.rs), so it doesn't download and install the same dependencies every time. As on
//! Vercel, a framework's build keeps every `node_modules` folder of the project, restored before
//! the install command runs, which then has little or nothing to do. A Python project keeps its
//! installed, compiled requirements, used again while `requirements.txt` says the same.
//!
//! A cache is one object, `cache/<project>/<target>.tar.zst`: a zstd-compressed tar whose first
//! entry says what it holds (`Manifest`). The builder reads and writes it through URLs presigned
//! for that project alone (remote.rs). Production builds use production's only; a preview build
//! uses the previews', or production's while there's none, and writes the previews': code that only
//! ever ran in a preview build never reaches a production build through it.
//!
//! It's written after a build that succeeded, when what it would hold has changed (`Plan::key`),
//! and not used once it's `MAX_AGE` old, so nothing in it outlives a month.
//!
//! A Next.js build also keeps its dist folder's `cache` (`.next/cache`) in an archive of its own,
//! `cache/<project>/<target>.next.tar.zst`, written after every build that succeeded, as Vercel
//! keeps it (`Plan::split_next`). It is what the next build's compiler starts from, and it holds
//! the key Next encrypts Server Actions' bound arguments with (`.rscinfo`, renewed by Next every
//! 14 days): kept, a deploy doesn't change it, so pages opened before it still call their actions.
//! Apart, it costs a build only its own packing (a second or so), not the dependencies' again.
//! Only the target's own is read, never production's for a preview: that key, and what production's
//! build fetched with production's env vars, are production's alone.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::package;
use crate::state::now_ms;
use crate::vercel::{self, Language, Spec};

/// The version of the archive's layout. One a builder doesn't know isn't restored.
pub const FORMAT: u32 = 1;
/// The archive's first entry.
const MANIFEST_NAME: &str = "forkrail-cache.json";
/// A cache older than this isn't used: Vercel keeps a build cache a month.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// A bigger archive isn't kept: Vercel's limit on its standard build machines.
pub const MAX_BYTES: u64 = 1 << 30;
const ZSTD_LEVEL: i32 = 3;
/// Files up to this size are written by the worker threads; bigger ones as they're read.
const SMALL_FILE: u64 = 256 * 1024;
/// How deep in a project a `node_modules` folder is looked for: a monorepo's packages.
const MAX_DEPTH: usize = 6;
/// Files whose contents decide what a Node project's install does, wherever they are in it.
const DEPENDENCY_FILES: [&str; 12] = [
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    ".pnpmfile.cjs",
    "yarn.lock",
    ".yarnrc",
    ".yarnrc.yml",
    "bun.lock",
    "bun.lockb",
    ".npmrc",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Every `node_modules` folder of the project.
    Node,
    /// The installed requirements, under their key (package.rs).
    Python,
    /// Go's module and build caches (native.rs, functions.rs).
    Go,
    /// Cargo's registry and target folder: compiling dominates a Rust build.
    Rust,
    /// Next.js's `.next/cache`, kept after every build.
    Next,
}

impl Kind {
    /// Whether its files go back with the times they had: Cargo reruns a build script whose
    /// sources look newer than its output, and everything after it.
    pub fn keeps_times(self) -> bool {
        matches!(self, Kind::Go | Kind::Rust)
    }
}

/// What an archive holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub kind: Kind,
    pub key: String,
    /// The builder that wrote it: its system, Node and Python.
    pub platform: String,
    pub saved_ms: u64,
    /// The folders in it, relative to where they're restored.
    pub dirs: Vec<String>,
}

/// What a build of one project would keep, known before it builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub kind: Kind,
    /// Changes when what's kept would: with a Node project's lockfile, `package.json` files and
    /// install command, with a Python project's requirements.
    pub key: String,
    /// Where the folders are restored to and saved from: the project (Node), the folder of
    /// installed requirements (Python).
    pub base: PathBuf,
    /// A framework's build output (its top folder), which isn't searched for `node_modules`; a
    /// Nitro framework's two, since its preset decides which it writes (`.vercel`, `.output`).
    output: Vec<String>,
    /// Folders kept besides `node_modules`, relative to `base`: a framework's own caches (Vercel's
    /// `cachePattern`: Gatsby's `.cache` and `public`, Eleventy's `.cache`, Nuxt's `.nuxt`, Next's
    /// `.next/cache` until `split_next` takes it) and Hugo's download.
    extra: Vec<String>,
    /// For a Next.js project, the dist folder `vercel.json`'s `outputDirectory` names (`.next`
    /// without one): where its `cache` is looked for first.
    next: Option<String>,
}

/// The plan for the project in `dir`, or `None` when it has nothing worth keeping (a Node server
/// without a build) or can't be read (the build will say why).
pub fn plan(dir: &Path) -> Option<Plan> {
    plan_as(dir, None)
}

/// `plan`, for a project whose settings choose its framework preset (vercel::read_as).
pub fn plan_as(dir: &Path, preset: Option<&str>) -> Option<Plan> {
    let spec = vercel::read_as(dir, preset).ok()?;
    plan_for(dir, &spec).ok().flatten()
}

fn plan_for(dir: &Path, spec: &Spec) -> Result<Option<Plan>> {
    // Go and Rust keep their tools' caches, whatever the site beside them keeps: compiling is most
    // of their build.
    if matches!(spec.language, Language::Go | Language::Rust) && !spec.static_only() {
        let (kind, lock) = if spec.language == Language::Go { (Kind::Go, ["go.sum", "go.mod"]) } else { (Kind::Rust, ["Cargo.lock", "Cargo.toml"]) };
        let mut h = Sha256::new();
        for file in lock {
            if let Ok(bytes) = std::fs::read(dir.join(file)) {
                h.update(file.as_bytes());
                h.update(&bytes);
            }
        }
        let key = forkrail_common::hex(&h.finalize())[..16].to_owned();
        return Ok(Some(Plan { kind, key, base: package::native_cache()?, output: Vec::new(), extra: Vec::new(), next: None }));
    }
    if let Some(framework) = &spec.framework {
        // A site with nothing to build installs nothing, so there's nothing to keep, unless its
        // framework keeps something of its own (Hugo, without a package.json).
        if framework.build.is_none() || (!dir.join("package.json").is_file() && framework.cache.is_empty()) {
            return Ok(None);
        }
        let key = node_key(dir, &framework.install)?;
        let mut output: Vec<String> = framework.output.split('/').next().filter(|o| !o.is_empty() && *o != ".").map(str::to_owned).into_iter().collect();
        if framework.nitro() && !output.iter().any(|o| o == ".output") {
            output.push(".output".to_owned());
        }
        let extra = framework.cache.iter().map(|d| (*d).to_owned()).collect();
        let next = framework.next().then(|| framework.output.clone());
        return Ok(Some(Plan { kind: Kind::Node, key, base: dir.to_path_buf(), output, extra, next }));
    }
    if spec.language == Language::Python {
        let (base, key) = package::python_deps_cache(dir, spec)?;
        return Ok(Some(Plan { kind: Kind::Python, key, base, output: Vec::new(), extra: Vec::new(), next: None }));
    }
    // A Node backend preset installs its dependencies too (nodeapp.rs).
    if spec.language == Language::Node && spec.backend.is_some() && dir.join("package.json").is_file() {
        let key = node_key(dir, &crate::framework::flat_install_command(dir))?;
        return Ok(Some(Plan { kind: Kind::Node, key, base: dir.to_path_buf(), output: Vec::new(), extra: Vec::new(), next: None }));
    }
    Ok(None)
}

/// A hash of the install command and of every file that decides what it installs.
fn node_key(dir: &Path, install: &str) -> Result<String> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(dir)
        .standard_filters(false)
        .follow_links(false)
        .max_depth(Some(MAX_DEPTH + 1))
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("node_modules" | ".git")))
        .build();
    for entry in walker {
        let entry = entry.context("listing the project's files")?;
        let name = entry.file_name().to_string_lossy();
        if DEPENDENCY_FILES.contains(&name.as_ref()) && entry.path().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    let mut h = Sha256::new();
    h.update(install.as_bytes());
    h.update([0]);
    for path in files {
        h.update(path.strip_prefix(dir).expect("walked from dir").to_string_lossy().as_bytes());
        h.update([0]);
        h.update(package::file_sha256(&path)?);
    }
    Ok(forkrail_common::hex(&h.finalize())[..16].to_owned())
}

impl Plan {
    /// For a Next.js project: the plan for its `.next/cache` alone, which is then left out of this
    /// one (its dependencies'). None for any other project, whose plan stays as it was.
    pub fn split_next(&mut self) -> Option<Plan> {
        let named = self.next.clone()?;
        self.extra.retain(|e| e != crate::next::CACHE_DIR);
        Some(Plan { kind: Kind::Next, key: self.key.clone(), base: self.base.clone(), output: Vec::new(), extra: Vec::new(), next: Some(named) })
    }

    /// The folders to keep, as they are after a build, relative to `base`.
    pub fn dirs(&self) -> Result<Vec<String>> {
        match self.kind {
            Kind::Next => {
                // Wherever the build put its dist folder: where vercel.json says, `.next`, or the
                // `distDir` next.config names. A link isn't followed: only the build's own folder.
                let named = self.next.as_deref().unwrap_or(crate::next::DIST);
                let dist = crate::next::find_dist(&self.base, named).unwrap_or_else(|| self.base.join(named));
                let cache = dist.join("cache");
                if !std::fs::symlink_metadata(&cache).is_ok_and(|m| m.is_dir()) {
                    return Ok(Vec::new());
                }
                let rel = cache.strip_prefix(&self.base).context("the dist folder is outside the project")?;
                Ok(vec![rel.to_string_lossy().replace('\\', "/")])
            }
            Kind::Python => Ok(if self.base.join(&self.key).join(package::PYTHON_DEPS_DONE).is_file() { vec![self.key.clone()] } else { Vec::new() }),
            Kind::Go => Ok(["go-mod", "go-build"].iter().filter(|d| self.base.join(d).is_dir()).map(|d| (*d).to_owned()).collect()),
            Kind::Rust => Ok(["cargo-home/registry", "cargo-home/git", "cargo-target"].iter().filter(|d| self.base.join(d).is_dir()).map(|d| (*d).to_owned()).collect()),
            Kind::Node => {
                let mut found = Vec::new();
                let mut stack = vec![(self.base.clone(), 0usize)];
                while let Some((dir, depth)) = stack.pop() {
                    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
                        let entry = entry?;
                        if !entry.file_type()?.is_dir() {
                            continue;
                        }
                        let (path, name) = (entry.path(), entry.file_name());
                        let rel = path.strip_prefix(&self.base).expect("under base").to_string_lossy().replace('\\', "/");
                        // A kept folder goes whole; the build's output isn't searched.
                        let whole = self.extra.iter().any(|e| rel == *e || e.starts_with(&format!("{rel}/")));
                        if name == "node_modules" {
                            found.push(rel);
                        } else if depth < MAX_DEPTH && name != ".git" && !whole && !(depth == 0 && self.output.iter().any(|o| name == o.as_str())) {
                            stack.push((path, depth + 1));
                        }
                    }
                }
                found.extend(self.extra.iter().filter(|e| self.base.join(e).is_dir()).cloned());
                found.sort();
                Ok(found)
            }
        }
    }

    /// Whether this build can use an archive that says `manifest`, or why not.
    pub fn accepts(&self, manifest: &Manifest, platform: &str) -> Result<(), String> {
        if manifest.format != FORMAT {
            return Err(format!("it's format {}", manifest.format));
        }
        if manifest.kind != self.kind {
            return Err("it's another kind of project's".to_owned());
        }
        // Next checks its own cache against its version and config, and nothing in it is native
        // code: another builder's is still a start, and keeps the Server Actions key.
        if manifest.platform != platform && self.kind != Kind::Next {
            return Err(format!("it's from another builder ({})", manifest.platform));
        }
        let age = Duration::from_millis(now_ms().saturating_sub(manifest.saved_ms));
        if age > MAX_AGE {
            return Err(format!("it's {} days old", age.as_secs() / 86_400));
        }
        // A `node_modules` of other versions is still most of the way there, and the install
        // command makes it right. Requirements are used whole or not at all.
        if self.kind == Kind::Python && manifest.key != self.key {
            return Err("the requirements have changed".to_owned());
        }
        Ok(())
    }

    /// Whether a kept `.next/cache` holds Next's Server Actions key (`.rscinfo`), which the next
    /// build then uses again.
    pub fn holds_actions_key(&self, dirs: &[String]) -> bool {
        self.kind == Kind::Next && dirs.iter().any(|d| self.base.join(d).join(crate::next::ACTIONS_KEY_FILE).is_file())
    }

    /// Whether what a restore would bring is here already: a builder that built this project
    /// just before still has its requirements.
    pub fn present(&self) -> bool {
        self.kind == Kind::Python && self.base.join(&self.key).join(package::PYTHON_DEPS_DONE).is_file()
    }

    pub fn manifest(&self, platform: &str, dirs: Vec<String>) -> Manifest {
        Manifest { format: FORMAT, kind: self.kind, key: self.key.clone(), platform: platform.to_owned(), saved_ms: now_ms(), dirs }
    }
}

/// What went into or came out of an archive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub files: u64,
    /// The files' own sizes.
    pub bytes: u64,
}

/// Write the folders `manifest` lists, found under `base`, into the archive `path`. Symlinks are
/// kept as links; anything that isn't a file, a folder or a link is left out.
pub fn save(path: &Path, manifest: &Manifest, base: &Path) -> Result<Counts> {
    let file = BufWriter::with_capacity(1 << 20, File::create(path).with_context(|| format!("creating {}", path.display()))?);
    let mut encoder = zstd::Encoder::new(file, ZSTD_LEVEL)?;
    // Compression on other threads, beside the one that reads the files.
    let _ = encoder.multithread(std::thread::available_parallelism().map_or(1, |n| n.get().min(4)) as u32);
    let mut tar = tar::Builder::new(encoder);
    // Cargo and Go judge what's fresh by when files changed: theirs keep their times, which a
    // restore puts back (`keeps_times`). The rest are written the same way every time.
    tar.mode(if manifest.kind.keeps_times() { tar::HeaderMode::Complete } else { tar::HeaderMode::Deterministic });
    tar.follow_symlinks(false);

    let json = serde_json::to_vec(manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(json.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, MANIFEST_NAME, json.as_slice())?;

    let mut counts = Counts::default();
    for dir in &manifest.dirs {
        let rel = safe(Path::new(dir)).with_context(|| format!("{dir:?} isn't a folder to keep"))?;
        tar.append_dir(&rel, base.join(&rel)).with_context(|| format!("adding {dir}"))?;
        let mut stack = vec![rel];
        while let Some(rel) = stack.pop() {
            let dir = base.join(&rel);
            for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
                let entry = entry?;
                let (kind, name) = (entry.file_type()?, rel.join(entry.file_name()));
                if kind.is_dir() {
                    tar.append_dir(&name, entry.path()).with_context(|| format!("adding {}", name.display()))?;
                    stack.push(name);
                } else if kind.is_file() || kind.is_symlink() {
                    if kind.is_file() {
                        counts.files += 1;
                        counts.bytes += entry.metadata()?.len();
                    }
                    tar.append_path_with_name(entry.path(), &name).with_context(|| format!("adding {}", name.display()))?;
                }
            }
        }
    }
    tar.into_inner()?.finish()?.into_inner().map_err(|e| e.into_error())?.sync_data()?;
    Ok(counts)
}

/// What `restore` did with an archive.
#[derive(Debug)]
pub enum Restored {
    /// Its folders are under `base` now.
    Done(Manifest, Counts),
    /// Not used, and why.
    Refused(Manifest, String),
}

/// Restore the archive `path` under `base` if `accept` says yes to its manifest. The folders are
/// unpacked in `staging` (an empty or missing folder on `base`'s filesystem, removed afterwards)
/// and moved into place whole, so a restore that fails leaves nothing half there. A folder that
/// exists already is left as it is.
pub fn restore(path: &Path, base: &Path, staging: &Path, accept: impl FnOnce(&Manifest) -> Result<(), String>) -> Result<Restored> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive = tar::Archive::new(zstd::Decoder::with_buffer(BufReader::with_capacity(1 << 20, file))?);
    let mut entries = archive.entries()?;
    let manifest: Manifest = {
        let mut first = entries.next().context("the archive is empty")??;
        if first.path()?.as_ref() != Path::new(MANIFEST_NAME) || first.size() > 1 << 20 {
            bail!("the archive doesn't start with {MANIFEST_NAME}");
        }
        let mut json = Vec::new();
        first.read_to_end(&mut json)?;
        serde_json::from_slice(&json).with_context(|| format!("reading {MANIFEST_NAME}"))?
    };
    if let Err(why) = accept(&manifest) {
        return Ok(Restored::Refused(manifest, why));
    }
    let dirs: Vec<PathBuf> = manifest.dirs.iter().map(|d| safe(Path::new(d)).with_context(|| format!("{d:?} isn't a folder to restore"))).collect::<Result<_>>()?;
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging).with_context(|| format!("creating {}", staging.display()))?;
    let unpacked = unpack(entries, staging, &dirs, manifest.kind.keeps_times()).and_then(|counts| {
        for dir in &dirs {
            let (from, to) = (staging.join(dir), base.join(dir));
            if !from.is_dir() || to.exists() {
                continue;
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&from, &to).with_context(|| format!("moving {} into place", dir.display()))?;
        }
        Ok(counts)
    });
    let _ = std::fs::remove_dir_all(staging);
    Ok(Restored::Done(manifest, unpacked?))
}

/// A small file read from the archive, for a worker thread to write.
struct Small {
    path: PathBuf,
    mode: u32,
    data: Vec<u8>,
    /// The time to give it, when the archive keeps times.
    modified: Option<std::time::SystemTime>,
}

/// Unpack the rest of an archive into `out`: only what's under one of `dirs`, and nothing through a
/// link the archive itself made. Most of the time is in creating tens of thousands of small files,
/// so those are written on a few threads while this one reads on.
fn unpack<R: Read>(entries: tar::Entries<'_, R>, out: &Path, dirs: &[PathBuf], times: bool) -> Result<Counts> {
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
    let failed = AtomicBool::new(false);
    let error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let fail = |e: anyhow::Error| {
        failed.store(true, Ordering::Relaxed);
        error.lock().expect("unpack lock").get_or_insert(e);
    };
    let counts = std::thread::scope(|scope| {
        let mut senders = Vec::new();
        for _ in 0..workers {
            let (tx, rx) = sync_channel::<Small>(64);
            senders.push(tx);
            let fail = &fail;
            scope.spawn(move || write_small(rx, fail));
        }
        let read = || -> Result<Counts> {
            let mut counts = Counts::default();
            let mut made: HashSet<PathBuf> = HashSet::new();
            let mut links: HashSet<PathBuf> = HashSet::new();
            let mut next = 0usize;
            for entry in entries {
                if failed.load(Ordering::Relaxed) {
                    break;
                }
                let mut entry = entry.context("reading the archive")?;
                let rel = safe(&entry.path()?).context("an entry's path")?;
                if !dirs.iter().any(|d| rel.starts_with(d)) {
                    bail!("{} isn't in a folder the archive lists", rel.display());
                }
                // Behind a link the archive made, or that link itself named again (a file entry at
                // a link's path would be written through it).
                if rel.ancestors().any(|a| links.contains(a)) {
                    bail!("{} is behind a link", rel.display());
                }
                let path = out.join(&rel);
                let mut parent = |dir: &Path| -> Result<()> {
                    if made.insert(dir.to_path_buf()) {
                        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                    }
                    Ok(())
                };
                let kind = entry.header().entry_type();
                if kind.is_dir() {
                    parent(&path)?;
                } else if kind.is_file() {
                    parent(path.parent().expect("under out"))?;
                    let (mode, size) = (entry.header().mode().unwrap_or(0o644) & 0o777, entry.size());
                    let modified = if times { entry.header().mtime().ok().map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s)) } else { None };
                    counts.files += 1;
                    counts.bytes += size;
                    if size <= SMALL_FILE {
                        let mut data = Vec::with_capacity(size as usize);
                        entry.read_to_end(&mut data)?;
                        // A send fails only when its worker has stopped, on an error it recorded.
                        let _ = senders[next % workers].send(Small { path, mode, data, modified });
                        next += 1;
                    } else {
                        let mut file = create(&path, mode)?;
                        std::io::copy(&mut entry, &mut file).with_context(|| format!("writing {}", path.display()))?;
                        if let Some(at) = modified {
                            file.set_modified(at).with_context(|| format!("dating {}", path.display()))?;
                        }
                    }
                } else if kind.is_symlink() {
                    parent(path.parent().expect("under out"))?;
                    let target = entry.link_name()?.context("a link without a target")?;
                    symlink(&target, &path).with_context(|| format!("linking {}", path.display()))?;
                    links.insert(rel);
                }
            }
            Ok(counts)
        };
        let counts = read();
        drop(senders);
        counts
    });
    match error.into_inner().expect("unpack lock") {
        Some(e) => Err(e),
        None => counts,
    }
}

fn write_small(jobs: Receiver<Small>, fail: &(impl Fn(anyhow::Error) + Sync)) {
    for job in jobs {
        let written = create(&job.path, job.mode).and_then(|mut file| {
            file.write_all(&job.data).with_context(|| format!("writing {}", job.path.display()))?;
            if let Some(at) = job.modified {
                file.set_modified(at).with_context(|| format!("dating {}", job.path.display()))?;
            }
            Ok(())
        });
        if let Err(e) = written {
            fail(e);
            return;
        }
    }
}

/// A new file with the archive's permissions.
fn create(path: &Path, mode: u32) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    options.open(path).with_context(|| format!("creating {}", path.display()))
}

#[cfg(unix)]
fn symlink(target: &Path, path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn symlink(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("links aren't restored on this system"))
}

/// `path` if it stays inside the folder it's joined to: relative, with no `..`.
fn safe(path: &Path) -> Result<PathBuf> {
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => clean.push(name),
            Component::CurDir => {}
            _ => bail!("{} would land outside the folder", path.display()),
        }
    }
    if clean.as_os_str().is_empty() {
        bail!("an empty path");
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::TempDir;

    fn write(root: &Path, files: &[(&str, &str)]) {
        for (name, text) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    const SITE: &[(&str, &str)] = &[
        ("package.json", r#"{"scripts": {"build": "vite build"}, "devDependencies": {"@tanstack/react-start": "1", "nitro": "3"}}"#),
        ("package-lock.json", "{}"),
        ("src/app.tsx", "export {}"),
    ];

    fn node_plan(dir: &Path) -> Plan {
        let plan = plan(dir).expect("a framework project has a plan");
        assert_eq!(plan.kind, Kind::Node);
        plan
    }

    #[test]
    fn the_key_follows_what_install_reads() {
        let tmp = TempDir::new("cache-key").unwrap();
        let dir = tmp.path();
        write(dir, SITE);
        let key = node_plan(dir).key;
        // The app's own files don't change it, nor does anything inside node_modules.
        write(dir, &[("src/app.tsx", "export const a = 1"), ("node_modules/x/package.json", "{}")]);
        assert_eq!(node_plan(dir).key, key);
        // The lockfile does, and so does a workspace package's package.json, and the install command.
        write(dir, &[("package-lock.json", r#"{"lockfileVersion": 3}"#)]);
        let locked = node_plan(dir).key;
        assert_ne!(locked, key);
        write(dir, &[("packages/ui/package.json", "{}")]);
        let workspace = node_plan(dir).key;
        assert_ne!(workspace, locked);
        write(dir, &[("vercel.json", r#"{"installCommand": "npm install --no-audit"}"#)]);
        assert_ne!(node_plan(dir).key, workspace);
    }

    #[test]
    fn a_server_without_a_build_keeps_nothing() {
        let tmp = TempDir::new("cache-none").unwrap();
        write(tmp.path(), &[("server.mjs", "export default 1\n")]);
        assert!(plan(tmp.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn framework_caches_are_kept_too() {
        // Gatsby keeps `.cache` and its output `public` whole (Vercel's `cachePattern`), not searched
        // for node_modules.
        let tmp = TempDir::new("cache-framework").unwrap();
        let dir = tmp.path().join("gatsby");
        write(
            &dir,
            &[
                ("package.json", r#"{"scripts": {"build": "gatsby build"}, "dependencies": {"gatsby": "5"}}"#),
                ("node_modules/gatsby/package.json", "{}"),
                (".cache/redux.rest.state", "x"),
                (".cache/node_modules/x/index.js", ""),
                ("public/index.html", "<p>"),
            ],
        );
        let plan = node_plan(&dir);
        assert_eq!(plan.dirs().unwrap(), [".cache", "node_modules", "public"]);
        // Hugo, with no package.json, keeps its download, and a fresh checkout gets it back.
        let hugo = tmp.path().join("hugo");
        write(&hugo, &[("config.toml", "baseURL = '/'\n"), (".vercel/cache/hugo-v0.58.2-linux-arm64/hugo", "bin")]);
        let plan = node_plan(&hugo);
        let dirs = plan.dirs().unwrap();
        assert_eq!(dirs, [".vercel/cache"]);
        let archive = tmp.path().join("hugo.tar.zst");
        save(&archive, &plan.manifest("test", dirs), &hugo).unwrap();
        let fresh = tmp.path().join("hugo-fresh");
        write(&fresh, &[("config.toml", "baseURL = '/'\n")]);
        let Restored::Done(..) = restore(&archive, &fresh, &tmp.path().join("staging"), |m| plan.accepts(m, "test")).unwrap() else { panic!("refused") };
        assert_eq!(std::fs::read_to_string(fresh.join(".vercel/cache/hugo-v0.58.2-linux-arm64/hugo")).unwrap(), "bin");
        // A plain site with nothing to build keeps nothing.
        let plain = tmp.path().join("plain");
        write(&plain, &[("index.html", "<p>")]);
        assert!(plan_as(&plain, None).is_none());
    }

    #[test]
    fn next_keeps_its_compilers_cache() {
        // Next.js: `.next/cache` beside `node_modules` (Vercel's `cachePattern`); the rest of the
        // dist folder, the standalone folder's own `node_modules` in it, is a build's output.
        let tmp = TempDir::new("cache-next").unwrap();
        let dir = tmp.path();
        write(
            dir,
            &[
                ("package.json", r#"{"scripts": {"build": "next build"}, "dependencies": {"next": "16.4.6"}}"#),
                ("node_modules/next/package.json", "{}"),
                (".next/cache/webpack/x.pack", "x"),
                (".next/standalone/node_modules/next/package.json", "{}"),
                (".next/server/app/page.js", ""),
            ],
        );
        let plan = plan(dir).unwrap();
        assert_eq!(plan.kind, Kind::Node);
        assert_eq!(plan.dirs().unwrap(), [".next/cache", "node_modules"]);
        // The next build, in a fresh checkout, finds it where Next looks for it.
        let archive = tmp.path().join("next.tar.zst");
        save(&archive, &plan.manifest("test", plan.dirs().unwrap()), dir).unwrap();
        let fresh = tmp.path().join("fresh");
        write(&fresh, &[("package.json", r#"{"scripts": {"build": "next build"}, "dependencies": {"next": "16.4.6"}}"#)]);
        let again = super::plan(&fresh).unwrap();
        let Restored::Done(..) = restore(&archive, &fresh, &tmp.path().join("staging"), |m| again.accepts(m, "test")).unwrap() else { panic!("refused") };
        assert_eq!(std::fs::read_to_string(fresh.join(".next/cache/webpack/x.pack")).unwrap(), "x");
        assert!(fresh.join("node_modules/next/package.json").is_file() && !fresh.join(".next/standalone").exists() && !fresh.join(".next/server").exists());
    }

    const NEXT_PACKAGE: &str = r#"{"scripts": {"build": "next build"}, "dependencies": {"next": "16.4.6"}}"#;

    /// What `next build` leaves in `dist`: enough for `find_dist` to know it.
    fn next_built(dir: &Path, dist: &str, cache: &[(&str, &str)]) {
        write(dir, &[(&format!("{dist}/BUILD_ID"), "b1"), (&format!("{dist}/required-server-files.json"), "{}"), (&format!("{dist}/server/app/page.js"), "")]);
        for (name, text) in cache {
            write(dir, &[(&format!("{dist}/cache/{name}"), text)]);
        }
    }

    #[test]
    fn a_next_build_keeps_its_cache_in_an_archive_of_its_own() {
        let tmp = TempDir::new("cache-next-apart").unwrap();
        let dir = tmp.path().join("app");
        write(&dir, &[("package.json", NEXT_PACKAGE), ("node_modules/next/package.json", "{}")]);
        next_built(&dir, ".next", &[(".rscinfo", r#"{"encryption.key":"k1"}"#), ("webpack/x.pack", "x")]);
        let mut deps = plan(&dir).unwrap();
        let next = deps.split_next().expect("a Next project's cache is split off");
        // The dependencies' archive keeps node_modules alone; the other, `.next/cache` alone.
        assert_eq!((deps.kind, deps.dirs().unwrap()), (Kind::Node, vec!["node_modules".to_owned()]));
        assert_eq!((next.kind, next.dirs().unwrap()), (Kind::Next, vec![".next/cache".to_owned()]));
        assert!(next.holds_actions_key(&next.dirs().unwrap()) && !deps.holds_actions_key(&deps.dirs().unwrap()));

        // The next build, in a fresh checkout, gets it back before it builds: the same key.
        let archive = tmp.path().join("next.tar.zst");
        let saved = save(&archive, &next.manifest("linux aarch64, Node 24, Python 3.13", next.dirs().unwrap()), &dir).unwrap();
        assert_eq!(saved.files, 2);
        let fresh = tmp.path().join("fresh");
        write(&fresh, &[("package.json", NEXT_PACKAGE)]);
        let again = plan(&fresh).unwrap().split_next().unwrap();
        // From another builder too: Next judges its own cache, and the key is the point of it.
        let another = "linux aarch64, Node 26, Python 3.14";
        let Restored::Done(manifest, counts) = restore(&archive, &fresh, &tmp.path().join("staging"), |m| again.accepts(m, another)).unwrap() else { panic!("refused") };
        assert_eq!((manifest.kind, counts), (Kind::Next, saved));
        assert_eq!(std::fs::read_to_string(fresh.join(".next/cache/.rscinfo")).unwrap(), r#"{"encryption.key":"k1"}"#);
        assert!(!fresh.join("node_modules").exists() && !fresh.join(".next/server").exists());

        // A month-old one isn't used, nor an archive of the dependencies' kind.
        let mut old = again.manifest("test", vec![".next/cache".into()]);
        old.saved_ms -= MAX_AGE.as_millis() as u64 + 86_400_000;
        assert!(again.accepts(&old, "test").unwrap_err().contains("31 days"));
        assert!(again.accepts(&deps.manifest("test", vec![]), "test").is_err());
    }

    #[test]
    fn a_next_build_without_a_cache_keeps_nothing_of_it() {
        let tmp = TempDir::new("cache-next-none").unwrap();
        let dir = tmp.path();
        write(dir, &[("package.json", NEXT_PACKAGE), ("node_modules/next/package.json", "{}")]);
        next_built(dir, ".next", &[]);
        let next = plan(dir).unwrap().split_next().unwrap();
        assert!(next.dirs().unwrap().is_empty());
        // A link where the cache would be isn't followed out of the build.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("node_modules"), dir.join(".next/cache")).unwrap();
            assert!(next.dirs().unwrap().is_empty());
        }
        // Not Next: nothing is split off, and the plan stays as it was.
        let gatsby = tmp.path().join("gatsby");
        write(&gatsby, &[("package.json", r#"{"scripts": {"build": "gatsby build"}, "dependencies": {"gatsby": "5"}}"#), (".cache/x", "x")]);
        let mut whole = plan(&gatsby).unwrap();
        let before = whole.clone();
        assert!(whole.split_next().is_none());
        assert_eq!(whole, before);
    }

    #[test]
    fn a_next_cache_is_found_where_the_build_put_its_dist_folder() {
        // `distDir: 'build/next'` in next.config, which the plan can't know before the build.
        let tmp = TempDir::new("cache-next-dist").unwrap();
        let dir = tmp.path().join("custom");
        write(&dir, &[("package.json", NEXT_PACKAGE)]);
        next_built(&dir, "build/next", &[(".rscinfo", "{}")]);
        assert_eq!(plan(&dir).unwrap().split_next().unwrap().dirs().unwrap(), ["build/next/cache"]);
        // vercel.json's outputDirectory names it.
        let named = tmp.path().join("named");
        write(&named, &[("package.json", NEXT_PACKAGE), ("vercel.json", r#"{"outputDirectory": "apps/web/.next"}"#)]);
        next_built(&named, "apps/web/.next", &[("images/a.webp", "img")]);
        let next = plan(&named).unwrap().split_next().unwrap();
        assert_eq!(next.dirs().unwrap(), ["apps/web/.next/cache"]);
        assert!(!next.holds_actions_key(&next.dirs().unwrap()));
    }

    #[test]
    fn a_kept_next_cache_wins_over_one_kept_with_the_dependencies_before() {
        // The dependencies' archive from before the split holds `.next/cache` too: the builder
        // restores the newer, apart one first, and the older is then left out.
        let tmp = TempDir::new("cache-next-both").unwrap();
        let dir = tmp.path().join("app");
        write(&dir, &[("package.json", NEXT_PACKAGE), ("node_modules/next/package.json", "{}")]);
        next_built(&dir, ".next", &[(".rscinfo", "old")]);
        let whole = plan(&dir).unwrap();
        assert_eq!(whole.dirs().unwrap(), [".next/cache", "node_modules"]);
        let deps_archive = tmp.path().join("deps.tar.zst");
        save(&deps_archive, &whole.manifest("test", whole.dirs().unwrap()), &dir).unwrap();
        std::fs::write(dir.join(".next/cache/.rscinfo"), "new").unwrap();
        let next = whole.clone().split_next().unwrap();
        let next_archive = tmp.path().join("next.tar.zst");
        save(&next_archive, &next.manifest("test", next.dirs().unwrap()), &dir).unwrap();

        let fresh = tmp.path().join("fresh");
        write(&fresh, &[("package.json", NEXT_PACKAGE)]);
        let mut deps = plan(&fresh).unwrap();
        let next = deps.split_next().unwrap();
        let staging = tmp.path().join("staging");
        let Restored::Done(..) = restore(&next_archive, &fresh, &staging, |m| next.accepts(m, "test")).unwrap() else { panic!("refused") };
        let Restored::Done(..) = restore(&deps_archive, &fresh, &staging, |m| deps.accepts(m, "test")).unwrap() else { panic!("refused") };
        assert_eq!(std::fs::read_to_string(fresh.join(".next/cache/.rscinfo")).unwrap(), "new");
        assert!(fresh.join("node_modules/next/package.json").is_file());
    }

    #[test]
    fn node_modules_come_back_whole() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("cache-roundtrip").unwrap();
        let dir = tmp.path().join("project");
        write(&dir, SITE);
        write(
            &dir,
            &[
                ("node_modules/vite/package.json", r#"{"name": "vite"}"#),
                ("node_modules/vite/bin/vite.js", "#!/usr/bin/env node\n"),
                ("node_modules/.package-lock.json", "{}"),
                ("packages/ui/package.json", "{}"),
                ("packages/ui/node_modules/left-pad/index.js", "module.exports = 1\n"),
                // Build output isn't dependencies: Nitro's Build Output, nor its node-server output.
                (".output/server/node_modules/h3/index.mjs", "export {}\n"),
                (".vercel/output/functions/__fallback.func/node_modules/h3/index.mjs", "export {}\n"),
            ],
        );
        let big = "x".repeat(SMALL_FILE as usize + 10);
        write(&dir, &[("node_modules/esbuild/bin/esbuild", &big)]);
        std::fs::create_dir_all(dir.join("node_modules/empty")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/.bin")).unwrap();
        std::fs::set_permissions(dir.join("node_modules/vite/bin/vite.js"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("../vite/bin/vite.js", dir.join("node_modules/.bin/vite")).unwrap();

        let plan = node_plan(&dir);
        let dirs = plan.dirs().unwrap();
        assert_eq!(dirs, ["node_modules", "packages/ui/node_modules"]);
        let archive = tmp.path().join("cache.tar.zst");
        let saved = save(&archive, &plan.manifest("test", dirs), &dir).unwrap();
        assert_eq!(saved.files, 5);

        // A fresh checkout of the same project.
        let fresh = tmp.path().join("fresh");
        write(&fresh, SITE);
        write(&fresh, &[("packages/ui/package.json", "{}")]);
        let staging = tmp.path().join("staging");
        let plan = node_plan(&fresh);
        let Restored::Done(manifest, counts) = restore(&archive, &fresh, &staging, |m| plan.accepts(m, "test")).unwrap() else { panic!("refused") };
        assert_eq!((manifest.key.as_str(), counts), (plan.key.as_str(), saved));
        assert!(!staging.exists());
        assert_eq!(std::fs::read_to_string(fresh.join("node_modules/vite/package.json")).unwrap(), r#"{"name": "vite"}"#);
        assert_eq!(std::fs::read_to_string(fresh.join("packages/ui/node_modules/left-pad/index.js")).unwrap(), "module.exports = 1\n");
        assert_eq!(std::fs::metadata(fresh.join("node_modules/esbuild/bin/esbuild")).unwrap().len(), big.len() as u64);
        assert!(fresh.join("node_modules/empty").is_dir());
        assert_eq!(std::fs::metadata(fresh.join("node_modules/vite/bin/vite.js")).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(std::fs::read_link(fresh.join("node_modules/.bin/vite")).unwrap(), Path::new("../vite/bin/vite.js"));
        assert_eq!(std::fs::read_to_string(fresh.join("node_modules/.bin/vite")).unwrap(), "#!/usr/bin/env node\n");
        assert!(!fresh.join(".output").exists() && !fresh.join(".vercel").exists());

        // Another builder's, or an old one, is refused before anything is unpacked.
        let other = tmp.path().join("other");
        write(&other, SITE);
        let refused = restore(&archive, &other, &staging, |m| plan.accepts(m, "another builder")).unwrap();
        assert!(matches!(refused, Restored::Refused(_, why) if why.contains("another builder")));
        assert!(!other.join("node_modules").exists());
        let mut old = plan.manifest("test", vec![]);
        old.saved_ms -= MAX_AGE.as_millis() as u64 + 86_400_000;
        assert!(plan.accepts(&old, "test").unwrap_err().contains("31 days"));
    }

    #[cfg(unix)]
    #[test]
    fn an_archive_cant_write_outside_its_folders() {
        let tmp = TempDir::new("cache-escape").unwrap();
        let (base, staging) = (tmp.path().join("base"), tmp.path().join("staging"));
        std::fs::create_dir_all(&base).unwrap();
        let manifest = Manifest { format: FORMAT, kind: Kind::Node, key: "k".into(), platform: "test".into(), saved_ms: now_ms(), dirs: vec!["node_modules".into()] };
        // One archive per trick: a file outside the listed folders, and a file through a link.
        let archive = |entries: &[(&str, Option<&str>)]| {
            let path = tmp.path().join("bad.tar.zst");
            let mut tar = tar::Builder::new(zstd::Encoder::new(File::create(&path).unwrap(), 1).unwrap());
            let json = serde_json::to_vec(&manifest).unwrap();
            let mut header = tar::Header::new_gnu();
            header.set_size(json.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, MANIFEST_NAME, json.as_slice()).unwrap();
            for (name, link) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o644);
                match link {
                    Some(target) => {
                        header.set_entry_type(tar::EntryType::Symlink);
                        header.set_size(0);
                        tar.append_link(&mut header, name, target).unwrap();
                    }
                    None => {
                        header.set_size(2);
                        header.set_cksum();
                        tar.append_data(&mut header, name, &b"hi"[..]).unwrap();
                    }
                }
            }
            tar.into_inner().unwrap().finish().unwrap();
            path
        };
        let outside = archive(&[("elsewhere/file", None)]);
        assert!(restore(&outside, &base, &staging, |_| Ok(())).unwrap_err().to_string().contains("isn't in a folder"));
        let through = archive(&[("node_modules/out", Some("/")), ("node_modules/out/tmp/forkrail-escaped", None)]);
        assert!(restore(&through, &base, &staging, |_| Ok(())).unwrap_err().to_string().contains("behind a link"));
        // A file entry at a link's own path would be written through the link.
        let escaped = tmp.path().join("escaped-through-link");
        let same = archive(&[("node_modules/x", Some(escaped.to_str().unwrap())), ("node_modules/x", None)]);
        assert!(restore(&same, &base, &staging, |_| Ok(())).unwrap_err().to_string().contains("behind a link"));
        assert!(!escaped.exists());
        // Neither left anything.
        assert!(!base.join("node_modules").exists() && !staging.exists());
        // A manifest that lists a folder outside is refused too.
        assert!(safe(Path::new("../x")).is_err() && safe(Path::new("/x")).is_err() && safe(Path::new("")).is_err());
    }
}
