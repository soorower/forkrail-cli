//! Pages on the router's own disk (docs/c16-frameworks.md, F4b): every router keeps the same pages,
//! so a page is served from the machine that got the request, as Vercel serves a page from each
//! region's copy. `<dir>/pages/<deployment>/<hash>`, each file a page as `super::encode` writes it.
//!
//! Capped twice: at most `max_bytes` of pages, and never so much that the filesystem has less than
//! `free_floor` left (the router's log shares the disk and must never run out of room). The least
//! recently used go first, and a page unused for 31 days is deleted (Vercel's rule for its ISR
//! cache). Linux's page cache keeps recently read files in RAM by itself, and gives that back under
//! pressure; `super::Memory` keeps the hottest pages decoded in front of it.
//!
//! The index of what's here is in memory, made by `scan` at start from the files (each one's
//! meta line, and its modified time as when it was last used), and kept by every read and write.
//! A file's time is brought up to date by `sweep` once an hour for pages used since, so a restart
//! knows what's been used. Two router processes share the directory during a restart: files are
//! written whole (to a temporary name, then renamed), and one deleted by the other reads as missing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// A page unused this long is deleted (Vercel deletes ISR pages not accessed for 31 days).
pub const UNUSED_MS: u64 = 31 * 24 * 3600 * 1000;
/// How often unused pages are deleted, files' times brought up to date, and the caps checked again.
pub const SWEEP_EVERY: Duration = Duration::from_secs(3600);
/// Past a cap, pages are deleted down to this share of it, so a full store doesn't look for the
/// least used one on every write.
const EVICT_TO_PCT: u64 = 90;
/// The most of a file read to index it: its format line and meta.
const HEAD_BYTES: usize = 4096;
const STATE_FILE: &str = "state.json";

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Deployment ids and page hashes name directories and files; anything else never does.
pub fn safe_name(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) && !id.starts_with('.')
}

/// What the index knows of one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub deployment: String,
    pub size: u64,
    /// When the page was made (Unix ms), from its meta.
    pub stored_ms: u64,
    /// Made by this router while the home region couldn't be reached (`peers.rs`).
    pub local: bool,
    pub used_ms: u64,
}

#[derive(Default)]
struct Index {
    files: HashMap<String, Info>,
    used: u64,
    /// Bytes kept under each deployment's (or data cache's) name.
    scopes: HashMap<String, u64>,
    /// Pages used since the last sweep: their files' times are set then.
    touched: HashSet<String>,
}

impl Index {
    fn insert(&mut self, hash: String, info: Info) {
        if let Some(old) = self.files.insert(hash, info.clone()) {
            self.used -= old.size;
            self.uncount(&old);
        }
        self.used += info.size;
        *self.scopes.entry(info.deployment).or_default() += info.size;
    }

    fn uncount(&mut self, info: &Info) {
        if let Some(used) = self.scopes.get_mut(&info.deployment) {
            *used = used.saturating_sub(info.size);
            if *used == 0 {
                self.scopes.remove(&info.deployment);
            }
        }
    }

    fn remove(&mut self, hash: &str) -> Option<Info> {
        let info = self.files.remove(hash)?;
        self.used -= info.size;
        self.uncount(&info);
        self.touched.remove(hash);
        Some(info)
    }

    /// Pages of `scope` to delete so it holds at most `max` bytes with `extra` more (a page being
    /// written), least recently used first, never `keep`. Taken out of the index.
    fn evict_scope(&mut self, scope: &str, max: u64, extra: u64, keep: &str) -> Vec<(String, Info)> {
        if self.scopes.get(scope).copied().unwrap_or(0) + extra <= max {
            return Vec::new();
        }
        let target = (max * EVICT_TO_PCT / 100).saturating_sub(extra);
        let mut order: Vec<(u64, String)> = self.files.iter().filter(|(h, i)| i.deployment == scope && h.as_str() != keep).map(|(h, i)| (i.used_ms, h.clone())).collect();
        order.sort_unstable();
        let mut out = Vec::new();
        for (_, hash) in order {
            if self.scopes.get(scope).copied().unwrap_or(0) <= target {
                break;
            }
            if let Some(info) = self.remove(&hash) {
                out.push((hash, info));
            }
        }
        out
    }

    /// Pages to delete so `extra` more bytes fit within `max` and the filesystem keeps `floor`
    /// free (it has `free` now), least recently used first, never `keep`. Taken out of the index.
    fn evict(&mut self, extra: u64, max: u64, free: Option<u64>, floor: u64, keep: &str) -> Vec<(String, Info)> {
        let over_max = |used: u64| used + extra > max;
        let short = |freed: u64| free.is_some_and(|f| f + freed < floor + extra);
        if !over_max(self.used) && !short(0) {
            return Vec::new();
        }
        // Down to a share of the cap, not just under it: the next writes don't search again.
        let target = max * EVICT_TO_PCT / 100;
        let mut order: Vec<(u64, String)> = self.files.iter().filter(|(h, _)| h.as_str() != keep).map(|(h, i)| (i.used_ms, h.clone())).collect();
        order.sort_unstable();
        let mut out = Vec::new();
        let mut freed = 0;
        for (_, hash) in order {
            let still = self.used + extra > target || short(freed);
            if !still {
                break;
            }
            if let Some(info) = self.remove(&hash) {
                freed += info.size;
                out.push((hash, info));
            }
        }
        out
    }
}

/// The store's own small file beside its pages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// When this store was made. A store without it is new: its pages, if any were made before,
    /// are copied from another router, or are lost (`Lost`).
    #[serde(default)]
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lost: Option<Lost>,
    /// When each deployment's group was last revalidated on demand: its pages made before then
    /// are stale. `(deployment, group, ms)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<(String, u64, u64)>,
    /// An edge router's place in what its home has pushed: pages made after this are asked for when
    /// it connects again (`peers.rs`).
    #[serde(default)]
    pub home_since_ms: u64,
}

/// Every copy of the pages was lost when this store was made (no router had any to copy): pages of
/// these deployments that aren't here may have been made before, so their build's fallback is not
/// to be trusted as the newest content (`ResponseCache::lost`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lost {
    pub at_ms: u64,
    pub deployments: Vec<String>,
}

pub struct Disk {
    root: PathBuf,
    max_bytes: u64,
    free_floor: u64,
    index: Mutex<Index>,
    state: Mutex<State>,
    /// The store had no state file: just made.
    new: bool,
    warned: AtomicBool,
    /// Held while a written page is put in place, so two writes of one page can't land out of
    /// order (the older over the newer).
    placing: Mutex<()>,
}

impl Disk {
    /// The store in `root`, made if missing.
    pub fn open(root: PathBuf, max_bytes: u64, free_floor: u64) -> std::io::Result<Disk> {
        std::fs::create_dir_all(root.join("pages"))?;
        let found = std::fs::read(root.join(STATE_FILE)).ok().and_then(|b| serde_json::from_slice::<State>(&b).ok());
        let new = found.as_ref().is_none_or(|s| s.created_ms == 0);
        let state = found.filter(|s| s.created_ms != 0).unwrap_or(State { created_ms: now_ms(), ..State::default() });
        let disk = Disk { root, max_bytes, free_floor, index: Mutex::default(), state: Mutex::new(state), new, warned: AtomicBool::new(false), placing: Mutex::default() };
        disk.save_state();
        Ok(disk)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the store was made by this process (nothing was here before it).
    pub fn is_new(&self) -> bool {
        self.new
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn update_state(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap());
        self.save_state();
    }

    fn save_state(&self) {
        let bytes = serde_json::to_vec(&*self.state.lock().unwrap()).expect("plain fields");
        let path = self.root.join(STATE_FILE);
        let tmp = path.with_extension(format!("tmp{}", forkrail_common::random_hex(4)));
        if let Err(e) = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &path)) {
            warn!(path = %path.display(), error = %e, "saving the page store's state failed");
        }
    }

    fn path(&self, deployment: &str, hash: &str) -> Option<PathBuf> {
        (safe_name(deployment) && safe_name(hash)).then(|| self.root.join("pages").join(deployment).join(hash))
    }

    /// Pages and the bytes they take.
    #[cfg(test)]
    pub fn usage(&self) -> (usize, u64) {
        let index = self.index.lock().unwrap();
        (index.files.len(), index.used)
    }

    pub fn info(&self, hash: &str) -> Option<Info> {
        self.index.lock().unwrap().files.get(hash).cloned()
    }

    /// A page was served (from memory too): it isn't the least used.
    pub fn used(&self, hash: &str) {
        let mut index = self.index.lock().unwrap();
        if let Some(info) = index.files.get_mut(hash) {
            info.used_ms = now_ms();
            index.touched.insert(hash.to_owned());
        }
    }

    /// A page's bytes, if it's here.
    pub async fn read(&self, deployment: &str, hash: &str) -> Option<Bytes> {
        let path = self.path(deployment, hash)?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let bytes = Bytes::from(bytes);
                let mut index = self.index.lock().unwrap();
                if !index.files.contains_key(hash)
                    && let Some((meta, _, _)) = super::decode_head(&bytes)
                {
                    // Not indexed yet (the scan at start is still going): it is now.
                    index.insert(hash.to_owned(), Info { deployment: deployment.to_owned(), size: bytes.len() as u64, stored_ms: meta.stored_ms, local: meta.local, used_ms: 0 });
                }
                drop(index);
                self.used(hash);
                Some(bytes)
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(path = %path.display(), error = %e, "reading a page failed");
                }
                // Gone (deleted by the other process during a restart, say): not here.
                self.index.lock().unwrap().remove(hash);
                None
            }
        }
    }

    /// Keep a page, making room for it first, unless `current` says by then that a newer one of
    /// it is being kept. `Err` when it can't be kept (too little room left even with every other
    /// page deleted, or the write failed): it's served from memory only.
    pub async fn write(&self, deployment: &str, hash: &str, stored_ms: u64, local: bool, bytes: Bytes, current: impl Fn() -> bool) -> Result<(), String> {
        let path = self.path(deployment, hash).ok_or("not a page's name")?;
        let size = bytes.len() as u64;
        if size > self.max_bytes {
            return Err("larger than the store".into());
        }
        let free = free_bytes(&self.root);
        let evicted = self.index.lock().unwrap().evict(size, self.max_bytes, free, self.free_floor, hash);
        let freed: u64 = evicted.iter().map(|(_, i)| i.size).sum();
        self.delete(evicted).await;
        if free.is_some_and(|f| f + freed < self.free_floor + size) {
            if !self.warned.swap(true, Ordering::Relaxed) {
                warn!(dir = %self.root.display(), free_mb = free.map(|f| f >> 20), floor_mb = self.free_floor >> 20, "the disk is too full to keep pages: they're kept in memory only until there's room");
            }
            return Err("no room".into());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension(format!("tmp{}", forkrail_common::random_hex(4)));
        tokio::fs::write(&tmp, &bytes).await.map_err(|e| e.to_string())?;
        // A rename is one quick call; done under the lock with the check, so the newest lands last.
        let placed = {
            let _turn = self.placing.lock().unwrap();
            if current() {
                std::fs::rename(&tmp, &path).map(|_| {
                    self.index.lock().unwrap().insert(hash.to_owned(), Info { deployment: deployment.to_owned(), size, stored_ms, local, used_ms: now_ms() });
                    true
                })
            } else {
                Ok(false)
            }
        };
        match placed {
            Ok(true) => {
                self.warned.store(false, Ordering::Relaxed);
                Ok(())
            }
            Ok(false) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Ok(())
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e.to_string())
            }
        }
    }

    async fn delete(&self, pages: Vec<(String, Info)>) {
        for (hash, info) in pages {
            if let Some(path) = self.path(&info.deployment, &hash) {
                let _ = tokio::fs::remove_file(path).await;
            }
        }
    }

    /// Keep `scope` within `max` bytes with `extra` more on their way, its least recently used pages deleted first (Next.js's
    /// data cache has a size of its own for each project and environment, `next.rs`). The names
    /// deleted.
    pub async fn limit_scope(&self, scope: &str, max: u64, extra: u64, keep: &str) -> Vec<String> {
        let evicted = self.index.lock().unwrap().evict_scope(scope, max, extra, keep);
        let names = evicted.iter().map(|(h, _)| h.clone()).collect();
        self.delete(evicted).await;
        names
    }

    /// The names pages are kept under (deployments, data caches), with the bytes each holds.
    pub fn scopes(&self) -> Vec<(String, u64)> {
        self.index.lock().unwrap().scopes.iter().map(|(s, n)| (s.clone(), *n)).collect()
    }

    /// Delete everything kept under `scope` (its deployment or project is gone). The names deleted.
    pub async fn remove_scope(&self, scope: &str) -> Vec<String> {
        let gone: Vec<(String, Info)> = {
            let mut index = self.index.lock().unwrap();
            let names: Vec<String> = index.files.iter().filter(|(_, i)| i.deployment == scope).map(|(h, _)| h.clone()).collect();
            names.into_iter().filter_map(|h| index.remove(&h).map(|i| (h, i))).collect()
        };
        let names = gone.iter().map(|(h, _)| h.clone()).collect();
        self.delete(gone).await;
        if safe_name(scope) {
            let _ = tokio::fs::remove_dir(self.root.join("pages").join(scope)).await;
        }
        names
    }

    /// Pages made after `since_ms`, oldest first: what another router asks for (`peers.rs`).
    /// Pages made here while the home region couldn't be reached aren't given: the home's are.
    pub fn since(&self, since_ms: u64) -> Vec<(String, String)> {
        let index = self.index.lock().unwrap();
        let mut found: Vec<(u64, String, String)> =
            index.files.iter().filter(|(_, i)| i.stored_ms > since_ms && !i.local).map(|(h, i)| (i.stored_ms, i.deployment.clone(), h.clone())).collect();
        found.sort_unstable();
        found.into_iter().map(|(_, d, h)| (d, h)).collect()
    }

    /// Index the pages here, from their files. Run once at start; reads meanwhile index what they
    /// find themselves.
    pub async fn scan(&self) {
        let root = self.root.join("pages");
        let started = std::time::Instant::now();
        let found = tokio::task::spawn_blocking(move || scan_dir(&root)).await.unwrap_or_default();
        let mut index = self.index.lock().unwrap();
        for (hash, info) in found {
            if !index.files.contains_key(&hash) {
                index.insert(hash, info);
            }
        }
        info!(dir = %self.root.display(), pages = index.files.len(), mb = index.used >> 20, ms = started.elapsed().as_millis() as u64, "pages on disk");
    }

    /// Delete pages unused for 31 days, bring used pages' file times up to date, and keep within
    /// the caps (the log may have grown since the last write). Runs for good, once an hour.
    pub async fn sweep_every(self: std::sync::Arc<Self>, every: Duration) {
        loop {
            tokio::time::sleep(every).await;
            self.sweep(now_ms()).await;
        }
    }

    pub async fn sweep(&self, now: u64) {
        let (old, touched) = {
            let mut index = self.index.lock().unwrap();
            let old: Vec<String> = index.files.iter().filter(|(_, i)| i.used_ms + UNUSED_MS < now).map(|(h, _)| h.clone()).collect();
            let old: Vec<(String, Info)> = old.into_iter().filter_map(|h| index.remove(&h).map(|i| (h, i))).collect();
            let touched: Vec<(String, Info)> = std::mem::take(&mut index.touched).into_iter().filter_map(|h| index.files.get(&h).cloned().map(|i| (h, i))).collect();
            (old, touched)
        };
        let deleted = old.len();
        self.delete(old).await;
        let paths: Vec<PathBuf> = touched.iter().filter_map(|(h, i)| self.path(&i.deployment, h)).collect();
        let pages = self.root.join("pages");
        tokio::task::spawn_blocking(move || {
            for path in paths {
                if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
                    let _ = file.set_modified(SystemTime::now());
                }
            }
            remove_leftovers(&pages);
        })
        .await
        .ok();
        let evicted = self.index.lock().unwrap().evict(0, self.max_bytes, free_bytes(&self.root), self.free_floor, "");
        let over = evicted.len();
        self.delete(evicted).await;
        if deleted + over > 0 {
            info!(unused = deleted, over_cap = over, "pages deleted from disk");
        }
    }
}

/// Free bytes on the filesystem holding `path`, for the user the router runs as.
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path, and `stat` is ours to fill.
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

fn scan_dir(root: &Path) -> Vec<(String, Info)> {
    let mut out = Vec::new();
    let Ok(deployments) = std::fs::read_dir(root) else { return out };
    for deployment in deployments.flatten() {
        let name = deployment.file_name().to_string_lossy().into_owned();
        if !safe_name(&name) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(deployment.path()) else { continue };
        for file in files.flatten() {
            let hash = file.file_name().to_string_lossy().into_owned();
            if !safe_name(&hash) || hash.contains(".tmp") {
                continue;
            }
            let Ok(meta) = file.metadata() else { continue };
            let Some(head) = read_head(&file.path()) else { continue };
            let Some(page) = super::decode_meta(&head) else { continue };
            let used_ms = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64);
            out.push((hash, Info { deployment: name.clone(), size: meta.len(), stored_ms: page.stored_ms, local: page.local, used_ms }));
        }
    }
    out
}

fn read_head(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0; HEAD_BYTES];
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return None,
        }
    }
    head.truncate(filled);
    Some(head)
}

/// Temporary files a write never renamed (the process stopped midway), an hour old or more, and
/// deployments' directories left empty.
fn remove_leftovers(root: &Path) {
    let Ok(deployments) = std::fs::read_dir(root) else { return };
    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    for deployment in deployments.flatten() {
        let Ok(files) = std::fs::read_dir(deployment.path()) else { continue };
        let mut any = false;
        for file in files.flatten() {
            let stale_tmp = file.file_name().to_string_lossy().contains(".tmp") && file.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < hour_ago);
            if stale_tmp {
                let _ = std::fs::remove_file(file.path());
            } else {
                any = true;
            }
        }
        if !any {
            let _ = std::fs::remove_dir(deployment.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-pages-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn page(stored_ms: u64, size: usize) -> Bytes {
        let entry = super::super::Entry {
            status: hyper::StatusCode::OK,
            headers: hyper::HeaderMap::new(),
            body: Bytes::from(vec![b'x'; size]),
            stored_ms,
            fresh_until: None,
            stale_until: None,
            deployment: "dep".into(),
            group: None,
            page: true,
            local: false,
        };
        Bytes::from(super::super::encode(&entry, "h"))
    }

    #[tokio::test]
    async fn keeps_reads_and_finds_pages_again_after_a_restart() {
        let root = dir();
        let disk = Disk::open(root.clone(), 1 << 20, 0).unwrap();
        assert!(disk.is_new());
        disk.write("dep", "a1", 5, false, page(5, 100), || true).await.unwrap();
        assert!(disk.read("dep", "a1").await.is_some());
        assert!(disk.read("dep", "zz").await.is_none());
        // A write a newer one has overtaken isn't put in place.
        disk.write("dep", "a1", 4, false, page(4, 100), || false).await.unwrap();
        assert_eq!(disk.info("a1").unwrap().stored_ms, 5);
        assert_eq!(std::fs::read_dir(root.join("pages/dep")).unwrap().count(), 1, "its temporary file is gone too");
        assert!(disk.write("../x", "a1", 5, false, page(5, 1), || true).await.is_err(), "never outside the store");
        disk.update_state(|s| s.home_since_ms = 77);
        // Another process, on the same directory: not new, and the scan finds the page.
        let again = Disk::open(root.clone(), 1 << 20, 0).unwrap();
        assert!(!again.is_new());
        assert_eq!(again.state().home_since_ms, 77);
        again.scan().await;
        let info = again.info("a1").unwrap();
        assert_eq!((info.deployment.as_str(), info.stored_ms, info.local), ("dep", 5, false));
        assert_eq!(again.since(4), [("dep".to_owned(), "a1".to_owned())]);
        assert!(again.since(5).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn the_least_used_go_first_and_unused_pages_go_after_31_days() {
        let root = dir();
        let one = page(1, 1000).len() as u64;
        let disk = Disk::open(root.clone(), one * 4, 0).unwrap();
        for h in ["a", "b", "c", "d"] {
            disk.write("dep", h, 1, false, page(1, 1000), || true).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        disk.used("a");
        disk.write("dep", "e", 1, false, page(1, 1000), || true).await.unwrap();
        let (pages, used) = disk.usage();
        assert!(used <= one * 4 && pages < 5);
        assert!(disk.info("a").is_some() && disk.info("e").is_some(), "used and new stay");
        assert!(disk.info("b").is_none(), "least used went first");
        assert!(!root.join("pages/dep/b").exists());
        // Far ahead: everything unused for 31 days goes.
        disk.sweep(now_ms() + UNUSED_MS + 60_000).await;
        assert_eq!(disk.usage().0, 0);
        assert!(disk.write("dep", "big", 1, false, page(1, 5000), || true).await.is_err(), "larger than the cap");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn eviction_keeps_the_floor_free() {
        let mut index = Index::default();
        for (i, h) in ["a", "b", "c"].iter().enumerate() {
            index.insert((*h).into(), Info { deployment: "d".into(), size: 100, stored_ms: 0, local: false, used_ms: i as u64 });
        }
        // Room under the cap, but the filesystem has 150 free and must keep 200: two go.
        let gone = index.evict(100, 10_000, Some(150), 200, "");
        assert_eq!(gone.iter().map(|(h, _)| h.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(index.used, 100);
        assert!(index.evict(0, 10_000, Some(10_000), 200, "").is_empty());
    }
}
