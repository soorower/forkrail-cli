//! Cache tags on the router (docs/c16-frameworks.md, F4c; Vercel's rules in
//! `forkrail_common::purge`): what's been purged, by tag, in each project and environment.
//!
//! A kept answer or page keeps its own `Vercel-Cache-Tag` header (taken off only what the client
//! gets), so there's no index from tags to entries: a purge is a mark, "made before this time",
//! per (project, environment, tag), and each lookup reads the entry's tags against the marks of
//! its deployment's project and environment (`judge`). `*` marks everything. Invalidating makes
//! what was made before stale (served once more while it's made again); deleting makes it be
//! made again while the visitor waits, or stale until a revalidation deadline, then deleted.
//!
//! Marks are the same on every router: a purge from a function goes to home, which keeps it and
//! pushes it to the others (`peers.rs`); purges from the CLI and the dashboard are in the routing
//! table, which every router reads. Merging marks keeps the later times, so a mark heard twice, or
//! from two routers, comes to the same. They're kept on disk beside the pages (`tags.json`,
//! written at most once a second), and a mark older than 32 days (past which an unused page is
//! gone) is folded into its project's `*`: what was made before it is then stale once, whatever
//! its tags, which costs at most a background render each.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use forkrail_common::purge::{self, ALL, Purge, PurgeOp};
use hyper::HeaderMap;
use hyper::header::HeaderName;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::now_ms;

const CACHE_TAG: HeaderName = HeaderName::from_static(purge::CACHE_TAG_HEADER);
/// A mark this old is folded into its project's `*` (an unused page is deleted after 31 days).
pub const KEEP_MS: u64 = 32 * 86_400_000;
/// Marks per project and environment; past it, the oldest are folded into `*` the same way.
pub const MAX_MARKS: usize = 20_000;
/// Deletes still within their revalidation deadline, per mark; past it the two soonest are taken
/// as one (deleting a little more, a little sooner).
const MAX_PENDING: usize = 8;
const FILE: &str = "tags.json";
const SAVE_AFTER: Duration = Duration::from_secs(1);

/// What the marks say of a kept entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Invalidated (or deleted, within its deadline): served once more while made again.
    Stale,
    /// Deleted: made again while the visitor waits (`REVALIDATED`).
    Deleted,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What's been purged of one tag (or `*`) in one project and environment, in Unix milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// Made before: stale.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stale_before: u64,
    /// Made before: deleted.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub deleted_before: u64,
    /// Deletes with a revalidation deadline, `(when, until)`: made before `when`, stale until
    /// `until`, deleted after.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<(u64, u64)>,
}

impl Mark {
    pub fn of(op: PurgeOp, at_ms: u64, deadline_s: Option<u64>) -> Mark {
        match (op, deadline_s.filter(|&d| d > 0)) {
            (PurgeOp::Invalidate, _) => Mark { stale_before: at_ms, ..Mark::default() },
            (PurgeOp::Delete, None) => Mark { deleted_before: at_ms, ..Mark::default() },
            (PurgeOp::Delete, Some(d)) => Mark { pending: vec![(at_ms, at_ms + d.saturating_mul(1000))], ..Mark::default() },
        }
    }

    pub fn judge(&self, stored_ms: u64, now: u64) -> Option<Verdict> {
        if stored_ms < self.deleted_before {
            return Some(Verdict::Deleted);
        }
        let mut verdict = (stored_ms < self.stale_before).then_some(Verdict::Stale);
        for &(at, until) in &self.pending {
            if stored_ms < at {
                if now >= until {
                    return Some(Verdict::Deleted);
                }
                verdict = Some(Verdict::Stale);
            }
        }
        verdict
    }

    /// When it last marked anything.
    pub fn latest(&self) -> u64 {
        self.pending.iter().map(|&(at, _)| at).fold(self.stale_before.max(self.deleted_before), u64::max)
    }

    /// Take in `other` (the same purge again, or another router's copy): the later times win, so
    /// merging in any order, any number of times, comes to the same. Whether it changed.
    pub fn merge(&mut self, other: &Mark, now: u64) -> bool {
        let before = self.clone();
        self.stale_before = self.stale_before.max(other.stale_before);
        self.deleted_before = self.deleted_before.max(other.deleted_before);
        self.pending.extend_from_slice(&other.pending);
        self.tidy(now);
        *self != before
    }

    fn tidy(&mut self, now: u64) {
        let mut deleted = self.deleted_before;
        self.pending.retain(|&(at, until)| {
            if until <= now {
                deleted = deleted.max(at);
                false
            } else {
                true
            }
        });
        self.deleted_before = deleted;
        self.pending.retain(|&(at, _)| at > deleted);
        self.pending.sort_unstable();
        self.pending.dedup();
        while self.pending.len() > MAX_PENDING {
            let (at, until) = self.pending.remove(0);
            let next = &mut self.pending[0];
            *next = (next.0.max(at), next.1.min(until));
        }
    }
}

/// One mark, as kept on disk and sent between routers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagMark {
    pub project: String,
    /// `production` or `preview`.
    pub environment: String,
    pub tag: String,
    #[serde(flatten)]
    pub mark: Mark,
}

/// A project's marks in each environment, production first.
type Project = [HashMap<String, Mark>; 2];

fn slot(environment: &str) -> usize {
    usize::from(purge::environment(environment) == "preview")
}

/// Told of every mark that changed anything, wherever it came from (`TagMarks::watch`).
type Watcher = Box<dyn Fn(&[TagMark]) + Send + Sync>;

pub struct TagMarks {
    /// Next.js workers are sent the marks of their project as they change (`next.rs`).
    watcher: std::sync::OnceLock<Watcher>,
    marks: RwLock<HashMap<String, Project>>,
    /// Whether there's any mark at all: until there is, lookups don't look.
    any: AtomicBool,
    file: Option<PathBuf>,
    saving: Arc<AtomicBool>,
}

impl TagMarks {
    /// The marks kept in `dir` (beside the pages), if any; none without a store.
    pub fn open(dir: Option<PathBuf>) -> Arc<TagMarks> {
        let file = dir.map(|d| d.join(FILE));
        let found: Vec<TagMark> = file.as_ref().and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let marks = Arc::new(TagMarks { watcher: std::sync::OnceLock::new(), marks: RwLock::default(), any: AtomicBool::new(false), file, saving: Arc::default() });
        marks.merge_in(&found, now_ms());
        marks
    }

    /// What the marks of `project` in `environment` say of an entry made at `stored_ms` with these
    /// headers (its `Vercel-Cache-Tag`).
    pub fn judge(&self, project: &str, environment: &str, headers: &HeaderMap, stored_ms: u64, now: u64) -> Option<Verdict> {
        let tags = headers.get_all(CACHE_TAG).iter().filter_map(|v| std::str::from_utf8(v.as_bytes()).ok()).flat_map(purge::split_tags);
        self.judge_tags(project, environment, tags.take(purge::MAX_TAGS_PER_ANSWER), stored_ms, now)
    }

    /// As `judge`, of an entry with these tags (Next.js's entries are also judged by the tags of
    /// the route reading them, which no header of theirs holds).
    pub fn judge_tags<'a>(&self, project: &str, environment: &str, tags: impl Iterator<Item = &'a str>, stored_ms: u64, now: u64) -> Option<Verdict> {
        if !self.any.load(Ordering::Relaxed) {
            return None;
        }
        let marks = self.marks.read().unwrap();
        let env = &marks.get(project)?[slot(environment)];
        if env.is_empty() {
            return None;
        }
        let mut verdict = env.get(ALL).and_then(|m| m.judge(stored_ms, now));
        if verdict == Some(Verdict::Deleted) {
            return verdict;
        }
        for tag in tags {
            match env.get(tag).and_then(|m| m.judge(stored_ms, now)) {
                Some(Verdict::Deleted) => return Some(Verdict::Deleted),
                Some(Verdict::Stale) => verdict = Some(Verdict::Stale),
                None => {}
            }
        }
        verdict
    }

    /// A purge of `project` as marks: one per environment and tag.
    pub fn marks_of(project: &str, purge: &Purge) -> Vec<TagMark> {
        let mark = Mark::of(purge.op, purge.at_ms, purge.deadline_s);
        purge
            .environments()
            .iter()
            .flat_map(|env| purge.tags.iter().map(move |tag| (env, tag)))
            .map(|(env, tag)| TagMark { project: project.to_owned(), environment: (*env).to_owned(), tag: tag.clone(), mark: mark.clone() })
            .collect()
    }

    /// Take marks in; the ones that changed anything, to be passed on.
    pub fn merge_in(self: &Arc<Self>, marks: &[TagMark], now: u64) -> Vec<TagMark> {
        if marks.is_empty() {
            return Vec::new();
        }
        let mut changed = Vec::new();
        {
            let mut all = self.marks.write().unwrap();
            for m in marks {
                if m.project.is_empty() || m.tag.is_empty() {
                    continue;
                }
                let env = &mut all.entry(m.project.clone()).or_default()[slot(&m.environment)];
                if env.entry(m.tag.clone()).or_default().merge(&m.mark, now) {
                    changed.push(m.clone());
                }
            }
            self.any.store(!all.is_empty(), Ordering::Relaxed);
        }
        if !changed.is_empty() {
            self.save_soon();
            if let Some(watcher) = self.watcher.get() {
                watcher(&changed);
            }
        }
        changed
    }

    /// Call `watcher` with every mark that changes anything from now on, whether made here, pushed
    /// by home, sent by an edge or read from the routing table. One watcher, set once.
    pub fn watch(&self, watcher: impl Fn(&[TagMark]) + Send + Sync + 'static) {
        let _ = self.watcher.set(Box::new(watcher));
    }

    /// The marks of one project and environment, by tag.
    pub fn of(&self, project: &str, environment: &str) -> Vec<(String, Mark)> {
        let marks = self.marks.read().unwrap();
        marks.get(project).map(|envs| envs[slot(environment)].iter().map(|(tag, mark)| (tag.clone(), mark.clone())).collect()).unwrap_or_default()
    }

    /// Every mark, for a router that joins.
    pub fn all(&self) -> Vec<TagMark> {
        let marks = self.marks.read().unwrap();
        let mut out = Vec::new();
        for (project, envs) in marks.iter() {
            for (env, tags) in purge::ENVIRONMENTS.iter().zip(envs) {
                for (tag, mark) in tags {
                    out.push(TagMark { project: project.clone(), environment: (*env).to_owned(), tag: tag.clone(), mark: mark.clone() });
                }
            }
        }
        out
    }

    /// How many marks there are.
    pub fn len(&self) -> usize {
        self.marks.read().unwrap().values().flat_map(|e| e.iter()).map(HashMap::len).sum()
    }

    /// Fold marks older than `KEEP_MS`, and the oldest past `MAX_MARKS` a project and environment,
    /// into `*`: what was made before them is then stale once, whatever its tags, and what they
    /// deleted stays deleted (a page judged deleted can stay on disk, read on every request).
    pub fn prune(&self, now: u64) -> usize {
        let mut folded = 0;
        let mut marks = self.marks.write().unwrap();
        for envs in marks.values_mut() {
            for env in envs.iter_mut() {
                let mut old: Vec<(u64, String)> = env.iter().filter(|(t, _)| *t != ALL).map(|(t, m)| (m.latest(), t.clone())).collect();
                old.sort_unstable();
                let over = env.len().saturating_sub(MAX_MARKS);
                let mut into_all = Mark::default();
                for (i, (latest, tag)) in old.into_iter().enumerate() {
                    if i >= over && latest + KEEP_MS >= now {
                        break;
                    }
                    if let Some(mark) = env.remove(&tag) {
                        into_all.stale_before = into_all.stale_before.max(latest);
                        into_all.deleted_before = into_all.deleted_before.max(mark.deleted_before);
                        into_all.pending.extend_from_slice(&mark.pending);
                    }
                    folded += 1;
                }
                if into_all.latest() > 0 {
                    env.entry(ALL.to_owned()).or_default().merge(&into_all, now);
                }
            }
        }
        marks.retain(|_, envs| envs.iter().any(|e| !e.is_empty()));
        self.any.store(!marks.is_empty(), Ordering::Relaxed);
        folded
    }

    /// Written within a second, once for however many changes come meanwhile.
    fn save_soon(self: &Arc<Self>) {
        let Some(file) = self.file.clone() else { return };
        if self.saving.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.saving.store(false, Ordering::Release);
            self.save(&file);
            return;
        };
        let this = self.clone();
        handle.spawn(async move {
            tokio::time::sleep(SAVE_AFTER).await;
            this.saving.store(false, Ordering::Release);
            this.save(&file);
        });
    }

    fn save(&self, file: &PathBuf) {
        self.prune(now_ms());
        write(file, &self.all());
    }
}

fn write(file: &PathBuf, marks: &[TagMark]) {
    let bytes = serde_json::to_vec(marks).expect("plain fields");
    let tmp = file.with_extension(format!("tmp{}", forkrail_common::random_hex(4)));
    if let Err(e) = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, file)) {
        warn!(path = %file.display(), error = %e, "saving the cache tags failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    fn tagged(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(CACHE_TAG, HeaderValue::from_str(value).unwrap());
        h
    }

    fn purge(op: PurgeOp, tags: &[&str], at_ms: u64, target: Option<&str>, deadline_s: Option<u64>) -> Purge {
        Purge { op, tags: tags.iter().map(|t| (*t).to_owned()).collect(), src_images: false, target: target.map(str::to_owned), deadline_s, at_ms, by: None }
    }

    #[test]
    fn marks_judge_entries_by_when_they_were_made() {
        let m = Mark::of(PurgeOp::Invalidate, 100, None);
        assert_eq!((m.judge(99, 1_000), m.judge(100, 1_000)), (Some(Verdict::Stale), None));
        let d = Mark::of(PurgeOp::Delete, 100, None);
        assert_eq!(d.judge(50, 100), Some(Verdict::Deleted));
        let late = Mark::of(PurgeOp::Delete, 100, Some(10));
        assert_eq!((late.judge(50, 5_000), late.judge(50, 10_100), late.judge(150, 20_000)), (Some(Verdict::Stale), Some(Verdict::Deleted), None));
        // Merging: later times win, the same purge twice is once, in any order.
        let mut a = Mark::of(PurgeOp::Invalidate, 100, None);
        assert!(a.merge(&Mark::of(PurgeOp::Invalidate, 300, None), 0));
        assert!(!a.merge(&Mark::of(PurgeOp::Invalidate, 200, None), 0));
        assert_eq!(a.stale_before, 300);
        let mut b = Mark::of(PurgeOp::Delete, 400, Some(10));
        b.merge(&Mark::of(PurgeOp::Delete, 400, Some(10)), 0);
        assert_eq!(b.pending, [(400, 10_400)]);
        // A deadline passed is a plain delete; a plain delete later covers it.
        b.merge(&Mark::default(), 20_000);
        assert_eq!((b.deleted_before, b.pending.len()), (400, 0));
        let mut many = Mark::default();
        for i in 0..20 {
            many.merge(&Mark::of(PurgeOp::Delete, 1_000 + i, Some(100)), 0);
        }
        assert_eq!(many.pending.len(), MAX_PENDING);
        assert_eq!(many.judge(1_015, 1_000), Some(Verdict::Stale), "folded deadlines still mark what they marked");
    }

    #[test]
    fn purges_apply_to_their_project_environment_and_tags() {
        let marks = TagMarks::open(None);
        let (now, made) = (10_000, 5_000);
        assert_eq!(marks.judge("shop", "production", &tagged("a"), made, now), None, "nothing purged: not even looked at");
        marks.merge_in(&TagMarks::marks_of("shop", &purge(PurgeOp::Invalidate, &["a"], 6_000, Some("production"), None)), now);
        assert_eq!(marks.judge("shop", "production", &tagged("x, a"), made, now), Some(Verdict::Stale));
        assert_eq!(marks.judge("shop", "production", &tagged("A"), made, now), None, "tags are case-sensitive");
        assert_eq!(marks.judge("shop", "production", &tagged("a"), 7_000, now), None, "made after: fresh");
        assert_eq!(marks.judge("shop", "preview", &tagged("a"), made, now), None, "another environment");
        assert_eq!(marks.judge("blog", "production", &tagged("a"), made, now), None, "another project");
        assert_eq!(marks.judge("shop", "production", &HeaderMap::new(), made, now), None, "untagged");
        // Both environments without a target; a delete wins over a stale; `*` is everything.
        marks.merge_in(&TagMarks::marks_of("shop", &purge(PurgeOp::Delete, &["b"], 6_000, None, None)), now);
        assert_eq!(marks.judge("shop", "preview", &tagged("a,b"), made, now), Some(Verdict::Deleted));
        assert_eq!(marks.judge("shop", "", &tagged("a,b"), made, now), Some(Verdict::Deleted), "no environment recorded: production");
        marks.merge_in(&TagMarks::marks_of("shop", &purge(PurgeOp::Invalidate, &["*"], 8_000, Some("preview"), None)), now);
        assert_eq!(marks.judge("shop", "preview", &HeaderMap::new(), 7_000, now), Some(Verdict::Stale));
        assert_eq!(marks.len(), 4);
    }

    #[test]
    fn old_marks_fold_into_everything() {
        let marks = TagMarks::open(None);
        let now = KEEP_MS + 100_000;
        marks.merge_in(&TagMarks::marks_of("shop", &purge(PurgeOp::Invalidate, &["old"], 50_000, Some("production"), None)), now);
        marks.merge_in(&TagMarks::marks_of("shop", &purge(PurgeOp::Invalidate, &["new"], now - 1_000, Some("production"), None)), now);
        assert_eq!(marks.prune(now), 1);
        assert_eq!(marks.judge("shop", "production", &tagged("other"), 40_000, now), Some(Verdict::Stale), "made before the folded mark: stale, whatever its tags");
        assert_eq!(marks.judge("shop", "production", &tagged("other"), 60_000, now), None);
        assert_eq!(marks.judge("shop", "production", &tagged("new"), 60_000, now), Some(Verdict::Stale));

        // A folded delete stays a delete.
        marks.merge_in(&TagMarks::marks_of("shop", &purge(PurgeOp::Delete, &["gone"], 70_000, Some("production"), None)), now);
        assert_eq!(marks.prune(now), 1);
        assert_eq!(marks.judge("shop", "production", &tagged("gone"), 65_000, now), Some(Verdict::Deleted));
    }

    #[test]
    fn marks_are_kept_on_disk() {
        let dir = std::env::temp_dir().join(format!("forkrail-tags-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let marks = TagMarks::open(Some(dir.clone()));
        // Recent times: opening prunes by the clock.
        let now = now_ms();
        marks.merge_in(&TagMarks::marks_of("shop", &purge(PurgeOp::Delete, &["a"], now - 1_000, Some("preview"), Some(60))), now);
        let again = TagMarks::open(Some(dir.clone()));
        assert_eq!(again.all(), marks.all());
        assert_eq!(again.judge("shop", "preview", &tagged("a"), now - 2_000, now), Some(Verdict::Stale));
        let _ = std::fs::remove_dir_all(dir);
    }
}
