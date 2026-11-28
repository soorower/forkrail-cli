//! Next.js's shared cache (N2): one cache for a Next.js deployment, held by the router and reached
//! from every worker over its tunnel. Without it each Lambda environment keeps its own (three
//! environments served three versions of one ISR page; `revalidateTag` reached only the
//! environment that ran it). Next's side is `crates/runtime/src/next-cache.mjs`; the frames and
//! the entry layout are in `forkrail_common::nextcache`.
//!
//! **Channel.** A worker that announces the `cache` feature gets one long request opened on its
//! tunnel (`follow`, as the log stream is), frames both ways: `GET`, `SET`, `TAGS`, `SYNC` from
//! the worker, answers by id from here, and two things sent unasked: tag marks as they're made
//! and a note for every entry written (`CHANGED`), so a worker's own small memory of entries and
//! its copy of the marks stay right without asking. No connection is made for a read: it's a
//! frame each way on the tunnel (1.5 ms kept-alive against 7-9 ms dialed, 24-28 ms from S3; N0).
//! Work still running after the worker was retired (`after()`, `waitUntil`) dials in instead
//! (`dialed`, `FORKRAIL-CACHE/1`): a worker's token is remembered for 15 minutes for that, since a
//! lost `revalidateTag` would be stale content for good.
//!
//! **Store: C16's, not a second one.** An entry is a page of the response cache (`super::Entry`):
//! on every router's disk (`disk.rs`), the most used in the page memory, pushed over the peer link
//! (`peers.rs`). Its status and headers are the page's own, its tags are in `vercel-cache-tag`
//! (Next's explicit and soft tags alike), so the purge marks of `tags.rs` judge it as they judge
//! any page, and its body is Next's parts end to end (`html`, `rsc`, `seg:…`, `postponed`, …)
//! behind a line of meta. The router can serve a page from it without a worker (N3): the `html`
//! part for a document, `rsc` for an RSC request.
//! A write at home is kept and pushed to every router. A write at an edge is kept there and sent
//! to home (`Link::send_entry`), which keeps it if it's the newest and pushes it on. The newest
//! `stored_ms` wins everywhere. This router's clock stamps entries and marks.
//!
//! **Scopes.** Pages, route handlers' answers, images and `'use cache'` entries are the
//! deployment's own (Vercel's ISR cache is "scoped to a specific deployment"; Next puts the build
//! id in every `'use cache'` key). The data cache (`fetch`, `unstable_cache`) is the project and
//! environment's, kept across deployments, as Vercel's is; here it's one for all regions, where
//! Vercel's is one a region. Tags are the project and environment's, Vercel's scope, in the same
//! map as C16's cache tags: `forkrail cache invalidate --tag x` and `revalidateTag('x')` are the
//! same mark. `revalidateTag(t)` and `updateTag(t)` delete; with a profile that has `expire`, a
//! delete with that deadline (stale until then); a profile without, an invalidation.
//!
//! **One regeneration.** A stale entry (its time passed, or a tag invalidated with a profile) is
//! reported stale to one reader (`Verdict::Lead`), whose Next serves it and makes it again; the
//! others get it reported fresh (`Wait`) until the new one is written or 60 s pass. A router
//! decides at once for its own workers and tells the others (an edge through home, `peers.rs`),
//! so only two regions reading the same stale entry within one trip between them (tens of ms)
//! each make it once. A deleted entry is a miss for everyone, as in Next: readers
//! aren't made to wait for the first, since one that never writes back would hold the rest.
//!
//! **Limits.** A page up to 20 MB (C16's and Vercel's), a data or `'use cache'` entry 2 MB
//! (Vercel's item size), 128 tags of 256 bytes. Pages share C16's disk caps and its 31-day rule;
//! the data cache has `FORKRAIL_NEXT_DATA_MB` (default 256) for each project and environment,
//! least used out first, so one project's data can't push out another's pages.
//!
//! **Seed.** The build's prerenders are in S3 among the static files (`static/next-seed/<deployment>/`,
//! written at publish with the build's time as their age). The index is read when a deployment's
//! first cache worker connects; an entry is taken from S3 the first time it's asked for and kept
//! like any other. Nothing is copied to the routers at publish.
//!
//! **Failure.** Anything that fails here is an answer the worker turns into a miss (a read) or
//! drops (a write). Marks are kept and sent to home when it's back (`peers.rs`).

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use forkrail_common::nextcache::{self as wire, Class, Frame, kind};
use forkrail_common::purge::{self, PurgeOp};
use http_body_util::BodyExt;
use hyper::client::conn::http2::SendRequest;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Request, StatusCode, Version};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tracing::{debug, info, warn};

use super::tags::Mark;
use super::{CACHE_TAG, Entry, ResponseCache, TagMark, now_ms};
use crate::BoxError;
use crate::proxy::ProxyBody;
use crate::statics::Statics;

const MARK: HeaderName = HeaderName::from_static(wire::MARK_HEADER);
/// Frames waiting for a worker that reads slowly; past it its stream is cut, and it asks again.
const OUT_QUEUE: usize = 256;
/// Requests of one worker handled at once.
const AT_ONCE: usize = 64;
/// Marks in one frame.
const MARKS_PER_FRAME: usize = 500;
/// A worker that asks for the marks since a time gets those changed this long before it too
/// (marks from the other region arrive a little after they were made).
const SYNC_MARGIN_MS: u64 = 60_000;
/// A deployment without a seed is asked about again after this.
const NO_SEED_FOR: Duration = Duration::from_secs(600);
/// A dialed connection with nothing on it is closed after this.
const DIALED_IDLE: Duration = Duration::from_secs(60);
/// Tags in one `revalidateTag`.
const MAX_TAGS_PER_CALL: usize = 1024;

static NEXT: OnceLock<Arc<NextCache>> = OnceLock::new();

/// This router's Next.js cache, once `start` has made it.
pub fn get() -> Option<&'static Arc<NextCache>> {
    NEXT.get()
}

/// Whose worker a stream is: what its entries and marks are of.
#[derive(Debug, Clone)]
pub struct Ident {
    pub worker: String,
    pub project: String,
    pub environment: String,
    pub deployment: String,
}

impl Ident {
    fn scope(&self, class: Class) -> String {
        wire::scope(class, &self.project, &self.environment, &self.deployment)
    }

    fn tags_key(&self) -> String {
        tags_key(&self.project, &self.environment)
    }
}

fn tags_key(project: &str, environment: &str) -> String {
    format!("tags\n{project}\n{}", purge::environment(environment))
}

/// One worker's stream, for what's sent unasked.
struct Sub {
    id: u64,
    tx: mpsc::Sender<Bytes>,
    /// Told when the stream fell behind and was cut.
    cut: Arc<Notify>,
}

/// A cache worker seen lately: its dialed connections are taken as its own.
struct Recent {
    token: [u8; 32],
    ident: Ident,
    until: u64,
}

#[derive(Clone)]
enum Seed {
    Has(Arc<HashSet<String>>),
    None(std::time::Instant),
}

pub struct NextCache {
    cache: Arc<ResponseCache>,
    /// Where seeds are: the deployments' static files' bucket (or directory).
    seeds: Option<Arc<Statics>>,
    seed_index: Mutex<HashMap<String, Seed>>,
    /// Entries being made again by one reader, until when (Unix ms).
    leases: Mutex<HashMap<String, u64>>,
    /// Streams by what they're told of: a scope's entries, a project and environment's marks.
    subs: Mutex<HashMap<String, Vec<Sub>>>,
    recent: Mutex<HashMap<String, Recent>>,
    /// Bytes of data cache a project and environment may keep.
    data_max: u64,
    ids: AtomicU64,
    /// Scopes whose deployment or project wasn't in the table at the last sweep.
    gone: Mutex<HashSet<String>>,
}

/// Make this router's Next.js cache on `cache`, with seeds read through `seeds`.
pub fn start(cache: Arc<ResponseCache>, seeds: Option<Arc<Statics>>, data_max: u64) -> Arc<NextCache> {
    let next = Arc::new(NextCache {
        cache: cache.clone(),
        seeds,
        seed_index: Mutex::default(),
        leases: Mutex::default(),
        subs: Mutex::default(),
        recent: Mutex::default(),
        data_max,
        ids: AtomicU64::new(1),
        gone: Mutex::default(),
    });
    let weak: Weak<NextCache> = Arc::downgrade(&next);
    cache.tags.watch({
        let weak = weak.clone();
        move |changed| {
            if let Some(next) = weak.upgrade() {
                next.marks_changed(changed);
            }
        }
    });
    cache.watch_kept({
        let weak = weak.clone();
        move |hash, entry| {
            if let Some(next) = weak.upgrade() {
                next.kept(hash, entry);
            }
        }
    });
    cache.watch_leases(move |hash, until| {
        if let Some(next) = weak.upgrade() {
            next.lease_heard(hash, until);
        }
    });
    let _ = NEXT.set(next.clone());
    next
}

fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A mark as a worker is sent it.
fn wire_mark(tag: &str, mark: &Mark) -> wire::TagMark {
    wire::TagMark { tag: tag.to_owned(), s: mark.stale_before, d: mark.deleted_before, p: mark.pending.clone() }
}

/// The mark Next's `revalidateTag` makes at `at`: deleted now without a profile; with one, stale
/// now, and deleted after its `expire` if it has one.
fn mark_of(asked: &wire::Tags, at: u64) -> Mark {
    match (asked.profile, asked.expire.filter(|e| e.is_finite())) {
        (false, _) => Mark::of(PurgeOp::Delete, at, None),
        (true, None) => Mark::of(PurgeOp::Invalidate, at, None),
        (true, Some(expire)) if expire <= 0.0 => Mark::of(PurgeOp::Delete, at, None),
        (true, Some(expire)) => Mark { pending: vec![(at, at.saturating_add((expire * 1000.0) as u64))], ..Mark::default() },
    }
}

/// An entry's own tags.
fn tags_of(headers: &HeaderMap) -> impl Iterator<Item = &str> {
    headers.get_all(CACHE_TAG).iter().filter_map(|v| std::str::from_utf8(v.as_bytes()).ok()).flat_map(purge::split_tags)
}

fn error<T: Default + serde::Serialize>(id: u32, set: impl FnOnce(&mut T, String), message: impl Into<String>) -> Bytes {
    let mut head = T::default();
    set(&mut head, message.into());
    Frame::new(kind::REPLY, id, &head, b"").encode()
}

impl NextCache {
    // ---- What workers ask ----

    /// One frame from a worker, answered: the frames to send back.
    pub async fn handle(self: &Arc<Self>, ident: &Ident, frame: Frame) -> Vec<Bytes> {
        match frame.kind {
            kind::GET => match frame.open::<wire::Get>() {
                Some((asked, _)) => vec![self.get(ident, frame.id, asked).await],
                None => vec![error::<wire::GetReply>(frame.id, |h, e| h.error = Some(e), "not a get")],
            },
            kind::SET => match frame.open::<wire::Set>() {
                Some((asked, body)) => vec![self.set(ident, frame.id, asked, body).await],
                None => vec![error::<wire::Done>(frame.id, |h, e| h.error = Some(e), "not a set")],
            },
            kind::TAGS => match frame.open::<wire::Tags>() {
                Some((asked, _)) => vec![self.tags(ident, frame.id, asked)],
                None => vec![error::<wire::Done>(frame.id, |h, e| h.error = Some(e), "not tags")],
            },
            kind::SYNC => match frame.open::<wire::Sync>() {
                Some((asked, _)) => self.sync(ident, frame.id, asked),
                None => vec![error::<wire::Done>(frame.id, |h, e| h.error = Some(e), "not a sync")],
            },
            _ => vec![error::<wire::Done>(frame.id, |h, e| h.error = Some(e), "unknown request")],
        }
    }

    async fn get(&self, ident: &Ident, id: u32, asked: wire::Get) -> Bytes {
        let now = now_ms();
        let miss = |error: Option<&str>| Frame::new(kind::REPLY, id, &wire::GetReply { now, error: error.map(str::to_owned), ..wire::GetReply::default() }, b"").encode();
        if !is_hash(&asked.key_hash) {
            return miss(Some("not a key"));
        }
        let class = asked.class.unwrap_or(Class::Deployment);
        let scope = ident.scope(class);
        let hash = super::hash(&wire::hash_input(&scope, &asked.key_hash));
        let mut entry = self.cache.get(&scope, &hash, true).await;
        if entry.is_none() && class == Class::Deployment {
            entry = self.seeded(&ident.deployment, &asked.key_hash, &hash).await;
        }
        let Some(entry) = entry.filter(|e| e.headers.contains_key(&MARK)) else { return miss(None) };
        let Some((meta, _)) = wire::Meta::of(&entry.body) else { return miss(None) };
        // A `'use cache'` entry is judged by when its making began, as Next judges it; the others
        // by when they were written.
        let made = meta.ts.filter(|t| t.is_finite() && *t > 0.0).map_or(entry.stored_ms, |t| t as u64);
        let tags = tags_of(&entry.headers).chain(asked.soft.iter().map(String::as_str)).take(2 * wire::MAX_TAGS);
        let by_tags = self.cache.tags.judge_tags(&ident.project, &ident.environment, tags, made, now);
        if by_tags == Some(super::Verdict::Deleted) {
            return miss(None);
        }
        let rev = asked.rev.or(meta.rev).filter(|r| r.is_finite());
        let by_time = rev.is_some_and(|r| now > made.saturating_add((r.max(0.0) * 1000.0) as u64));
        let verdict = if !(by_time || by_tags.is_some()) {
            wire::Verdict::Fresh
        } else if self.lease(&hash, now) {
            wire::Verdict::Lead
        } else {
            wire::Verdict::Wait
        };
        let same = asked.have == Some(entry.stored_ms);
        let reply = wire::GetReply {
            hit: true,
            same,
            verdict: Some(verdict),
            t: entry.stored_ms,
            status: entry.status.as_u16(),
            headers: if same { Vec::new() } else { own_headers(&entry.headers) },
            tags: if same { Vec::new() } else { tags_of(&entry.headers).map(str::to_owned).collect() },
            now,
            error: None,
        };
        Frame::new(kind::REPLY, id, &reply, if same { b"" } else { &entry.body }).encode()
    }

    /// Whether this reader is the one to make a stale entry again: nobody is, or whoever was has
    /// had its time.
    fn lease(&self, hash: &str, now: u64) -> bool {
        let mut leases = self.leases.lock().unwrap();
        if leases.len() > 10_000 {
            leases.retain(|_, until| *until > now);
        }
        match leases.get(hash) {
            Some(until) if *until > now => false,
            _ => {
                let until = now + wire::LEASE_MS;
                leases.insert(hash.to_owned(), until);
                drop(leases);
                // The other routers are told, so their readers are served it meanwhile too: by
                // home at once, by an edge through home (behind this answer).
                if let Some(link) = self.cache.link.clone() {
                    if link.is_home() {
                        link.push_lease(hash, until, None);
                    } else if tokio::runtime::Handle::try_current().is_ok() {
                        let hash = hash.to_owned();
                        tokio::spawn(async move { link.send_lease(hash, until).await });
                    }
                }
                true
            }
        }
    }

    /// Whether someone, here or on another router, is making the entry again now (the router then
    /// sends no regeneration of its own, N3).
    pub(crate) fn leased(&self, hash: &str) -> bool {
        self.leases.lock().unwrap().get(hash).is_some_and(|until| *until > now_ms())
    }

    /// Another router's worker is making this entry again until `until` (Unix ms).
    fn lease_heard(&self, hash: &str, until: u64) {
        let mut leases = self.leases.lock().unwrap();
        let known = leases.entry(hash.to_owned()).or_default();
        *known = (*known).max(until.min(now_ms() + wire::LEASE_MS));
    }

    async fn set(self: &Arc<Self>, ident: &Ident, id: u32, asked: wire::Set, body: Bytes) -> Bytes {
        let now = now_ms();
        let refuse = |why: &str| Frame::new(kind::REPLY, id, &wire::Done { now, error: Some(why.to_owned()), ..wire::Done::default() }, b"").encode();
        if !is_hash(&asked.key_hash) {
            return refuse("not a key");
        }
        let Some((meta, _)) = wire::Meta::of(&body).filter(|(m, _)| m.key_hash == asked.key_hash) else { return refuse("not an entry") };
        let class = asked.class.unwrap_or(Class::Deployment);
        let max = if class == Class::Data || meta.kind == "USE_CACHE" { wire::MAX_DATA_BYTES } else { wire::MAX_PAGE_BYTES };
        if body.len() > max {
            return refuse("too large to keep");
        }
        let scope = ident.scope(class);
        let hash = super::hash(&wire::hash_input(&scope, &asked.key_hash));
        let mut headers = HeaderMap::new();
        for (name, value) in &asked.headers {
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_bytes(value.as_bytes()))
                && name != MARK
                && name != CACHE_TAG
            {
                headers.append(name, value);
            }
        }
        headers.insert(MARK, HeaderValue::from_static("1"));
        let mut tags: Vec<&str> = Vec::new();
        for tag in asked.tags.iter().filter_map(|t| wire::storable_tag(t)) {
            if tags.len() < wire::MAX_TAGS && !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        if !tags.is_empty()
            && let Ok(value) = HeaderValue::from_bytes(tags.join(",").as_bytes())
        {
            headers.insert(CACHE_TAG, value);
        }
        // Later than the copy it replaces, whatever the clock did: the newest wins on every router.
        let stored_ms = self.cache.current(&hash).map_or(now, |(current, _)| now.max(current + 1));
        let made = meta.ts.filter(|t| t.is_finite() && *t > 0.0).map_or(stored_ms, |t| t as u64);
        let entry = Entry {
            status: StatusCode::from_u16(asked.status).unwrap_or(StatusCode::OK),
            headers,
            body,
            stored_ms,
            fresh_until: wire::fresh_until(made, asked.rev.or(meta.rev)),
            stale_until: None,
            deployment: scope.clone(),
            group: None,
            page: true,
            local: false,
        };
        let size = entry.body.len() as u64 + 512;
        let entry = self.cache.put(&hash, entry, None);
        // An edge's write goes to home, which keeps it and pushes it to the other routers.
        if self.cache.is_edge()
            && let Some(link) = self.cache.link.clone()
        {
            let encoded = Bytes::from(super::encode(&entry, &hash));
            tokio::spawn(async move {
                if let Err(e) = link.send_entry(encoded).await {
                    debug!(error = %e, "home couldn't take a Next.js cache entry; it's kept here");
                }
            });
        }
        if class == Class::Data
            && let Some(disk) = self.cache.disk()
        {
            for gone in disk.limit_scope(&scope, self.data_max, size, &hash).await {
                self.cache.forget(&gone);
            }
        }
        Frame::new(kind::REPLY, id, &wire::Done { ok: true, t: stored_ms, now, error: None }, b"").encode()
    }

    fn tags(&self, ident: &Ident, id: u32, asked: wire::Tags) -> Bytes {
        let now = now_ms();
        let mark = mark_of(&asked, now);
        let environment = purge::environment(&ident.environment);
        let mut marks: Vec<TagMark> = Vec::new();
        for tag in asked.tags.iter().filter_map(|t| wire::storable_tag(t)).take(MAX_TAGS_PER_CALL) {
            marks.push(TagMark { project: ident.project.clone(), environment: environment.to_owned(), tag: tag.to_owned(), mark: mark.clone() });
        }
        // Kept here (and pushed to every router from home); an edge sends them to home behind the
        // answer, and keeps them to send again if home can't take them now.
        self.cache.take_marks(&marks, None);
        if self.cache.is_edge()
            && let Some(link) = self.cache.link.clone()
        {
            let marks = marks.clone();
            tokio::spawn(async move { link.send_tags(marks).await });
        }
        debug!(worker = %ident.worker, project = %ident.project, tags = marks.len(), profile = asked.profile, "Next.js tags revalidated");
        Frame::new(kind::REPLY, id, &wire::Done { ok: true, t: now, now, error: None }, b"").encode()
    }

    /// The worker's project and environment's marks changed since it last heard, then the answer.
    fn sync(&self, ident: &Ident, id: u32, asked: wire::Sync) -> Vec<Bytes> {
        let now = now_ms();
        let since = asked.since.saturating_sub(SYNC_MARGIN_MS);
        let marks: Vec<wire::TagMark> = self.cache.tags.of(&ident.project, &ident.environment).iter().filter(|(_, m)| asked.since == 0 || m.latest() >= since).map(|(t, m)| wire_mark(t, m)).collect();
        let mut frames: Vec<Bytes> = Vec::new();
        let chunks: Vec<&[wire::TagMark]> = if marks.is_empty() { vec![&[]] } else { marks.chunks(MARKS_PER_FRAME).collect() };
        let last = chunks.len() - 1;
        for (i, chunk) in chunks.into_iter().enumerate() {
            frames.push(Frame::new(kind::MARKS, 0, &wire::Marks { marks: chunk.to_vec(), synced: i == last, now }, b"").encode());
        }
        frames.push(Frame::new(kind::REPLY, id, &wire::Done { ok: true, t: now, now, error: None }, b"").encode());
        frames
    }

    // ---- What workers are told unasked ----

    fn push(&self, key: &str, frame: &Bytes) {
        let mut subs = self.subs.lock().unwrap();
        let Some(list) = subs.get_mut(key) else { return };
        list.retain(|sub| match sub.tx.try_send(frame.clone()) {
            Ok(()) => true,
            // It can't keep up: cut, so it stops trusting what it has; it asks again on its next.
            Err(mpsc::error::TrySendError::Full(_)) => {
                sub.cut.notify_one();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
        if list.is_empty() {
            subs.remove(key);
        }
    }

    /// Marks changed here, wherever they came from: to the workers of their project.
    fn marks_changed(&self, changed: &[TagMark]) {
        if self.subs.lock().unwrap().is_empty() {
            return;
        }
        let now = now_ms();
        let mut by_scope: HashMap<String, Vec<wire::TagMark>> = HashMap::new();
        for m in changed {
            // What's sent is the change; the worker merges it into what it has, as `Mark::merge` does.
            by_scope.entry(tags_key(&m.project, &m.environment)).or_default().push(wire_mark(&m.tag, &m.mark));
        }
        for (key, marks) in by_scope {
            for chunk in marks.chunks(MARKS_PER_FRAME) {
                self.push(&key, &Frame::new(kind::MARKS, 0, &wire::Marks { marks: chunk.to_vec(), synced: false, now }, b"").encode());
            }
        }
    }

    /// A page was kept here (written by a worker of this router, or pushed by another router):
    /// if it's one of Next's, its scope's workers are told, and nobody need make it again.
    fn kept(&self, hash: &str, entry: &Arc<Entry>) {
        if !entry.headers.contains_key(&MARK) {
            return;
        }
        self.leases.lock().unwrap().remove(hash);
        if !self.subs.lock().unwrap().contains_key(&entry.deployment) {
            return;
        }
        let Some((meta, _)) = wire::Meta::of(&entry.body) else { return };
        let class = if entry.deployment.contains(".data.") { Class::Data } else { Class::Deployment };
        self.push(&entry.deployment, &Frame::new(kind::CHANGED, 0, &wire::Changed { class, key_hash: meta.key_hash, t: entry.stored_ms }, b"").encode());
    }

    fn subscribe(self: &Arc<Self>, ident: &Ident, tx: mpsc::Sender<Bytes>) -> Subscription {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let cut = Arc::new(Notify::new());
        let keys = vec![ident.scope(Class::Deployment), ident.scope(Class::Data), ident.tags_key()];
        let mut subs = self.subs.lock().unwrap();
        for key in &keys {
            subs.entry(key.clone()).or_default().push(Sub { id, tx: tx.clone(), cut: cut.clone() });
        }
        Subscription { next: Arc::downgrade(self), id, keys, cut }
    }

    // ---- The build's seed ----

    /// What `deployment`'s seed holds, read once (and again after a while if there was none).
    async fn seed_index(&self, deployment: &str) -> Option<Arc<HashSet<String>>> {
        let statics = self.seeds.as_ref()?;
        match self.seed_index.lock().unwrap().get(deployment) {
            Some(Seed::Has(index)) => return Some(index.clone()),
            Some(Seed::None(at)) if at.elapsed() < NO_SEED_FOR => return None,
            _ => {}
        }
        let read = statics.object(&wire::seed_index_key(deployment)).await.ok().and_then(|b| serde_json::from_slice::<wire::SeedIndex>(&b).ok());
        let mut index = self.seed_index.lock().unwrap();
        if index.len() > 4096 {
            index.clear();
        }
        match read {
            Some(seed) => {
                info!(%deployment, entries = seed.entries.len(), "Next.js cache seed");
                let entries: Arc<HashSet<String>> = Arc::new(seed.entries.into_iter().collect());
                index.insert(deployment.to_owned(), Seed::Has(entries.clone()));
                Some(entries)
            }
            None => {
                index.insert(deployment.to_owned(), Seed::None(std::time::Instant::now()));
                None
            }
        }
    }

    /// The build's entry for a key the store has none of, kept from now on.
    pub(crate) async fn seeded(&self, deployment: &str, key_hash: &str, hash: &str) -> Option<Arc<Entry>> {
        if !self.seed_index(deployment).await?.contains(key_hash) {
            return None;
        }
        let bytes = self.seeds.as_ref()?.object(&wire::seed_entry_key(deployment, key_hash)).await.ok()?;
        // The build wrote it, so it's held to what a worker's write is: a page's size, made no
        // later than now (a time ahead would hold back every router's replication), and no
        // router's own.
        if bytes.len() > wire::MAX_FRAME {
            warn!(%deployment, key = %key_hash, bytes = bytes.len(), "a Next.js seed entry is too large; skipped");
            return None;
        }
        let (found, mut entry) = super::decode(bytes)?;
        if found != hash || entry.deployment != deployment || !entry.headers.contains_key(&MARK) || entry.body.len() > wire::MAX_PAGE_BYTES {
            warn!(%deployment, key = %key_hash, "a Next.js seed entry isn't what its name says; skipped");
            return None;
        }
        entry.stored_ms = entry.stored_ms.min(now_ms());
        entry.group = None;
        entry.local = false;
        // Another reader may have brought it meanwhile, or a worker written a newer one.
        if let Some(current) = self.cache.get(deployment, hash, true).await {
            return Some(current);
        }
        Some(self.cache.put(hash, entry, None))
    }

    // ---- Streams ----

    fn remember(&self, ident: &Ident, token: &str) {
        let now = now_ms();
        let mut recent = self.recent.lock().unwrap();
        if recent.len() > 1024 {
            recent.retain(|_, r| r.until > now);
        }
        recent.insert(ident.worker.clone(), Recent { token: digest(token), ident: ident.clone(), until: now + wire::REMEMBER_WORKER_S * 1000 });
    }

    /// Open the cache stream on a worker's tunnel and answer what comes, until the `Follow` drops
    /// or the worker ends it.
    pub fn follow(self: &Arc<Self>, sender: &SendRequest<ProxyBody>, token: &str, ident: Ident) -> Follow {
        self.remember(&ident, token);
        let (out, out_rx) = mpsc::channel::<Bytes>(OUT_QUEUE);
        let (stop_tx, mut stop) = oneshot::channel::<()>();
        let request = Request::post(format!("http://forkrail-worker{}", wire::PATH))
            .version(Version::HTTP_2)
            .header(wire::TOKEN_HEADER, token)
            .body(Outgoing(out_rx).map_err(|never| match never {}).boxed())
            .expect("a valid request");
        let mut sender = sender.clone();
        let this = self.clone();
        tokio::spawn(async move {
            // The response doesn't need the handle; holding it would keep the tunnel open.
            let response = sender.send_request(request);
            drop(sender);
            let response = tokio::select! {
                response = response => response,
                _ = &mut stop => return,
            };
            let mut body = match response {
                Ok(response) if response.status().is_success() => response.into_body(),
                Ok(response) => {
                    warn!(worker = %ident.worker, status = response.status().as_u16(), "cache stream refused");
                    return;
                }
                Err(e) => {
                    debug!(worker = %ident.worker, error = %e, "cache stream failed");
                    return;
                }
            };
            let subscription = this.subscribe(&ident, out.clone());
            // Its index is read while the worker starts, so its first read doesn't wait for S3.
            tokio::spawn({
                let (this, deployment) = (this.clone(), ident.deployment.clone());
                async move {
                    this.seed_index(&deployment).await;
                }
            });
            let at_once = Arc::new(Semaphore::new(AT_ONCE));
            let mut buf = BytesMut::new();
            loop {
                let frame = tokio::select! {
                    frame = body.frame() => frame,
                    // Dropping the bodies resets the stream.
                    _ = &mut stop => break,
                    _ = subscription.cut.notified() => {
                        warn!(worker = %ident.worker, "a Next.js worker fell behind on its cache stream: cut");
                        break;
                    }
                };
                let Some(Ok(frame)) = frame else { break };
                let Ok(data) = frame.into_data() else { continue };
                buf.extend_from_slice(&data);
                loop {
                    match wire::take_frame(&mut buf) {
                        Ok(Some(frame)) => this.answer(&ident, frame, &out, &at_once).await,
                        Ok(None) => break,
                        Err(e) => {
                            warn!(worker = %ident.worker, error = e, "cache stream broken");
                            return;
                        }
                    }
                }
            }
        });
        Follow { _stop: stop_tx }
    }

    /// Answer one frame in a task of its own (a read may wait for disk or S3), at most `AT_ONCE`
    /// of a worker's at a time.
    async fn answer(self: &Arc<Self>, ident: &Ident, frame: Frame, out: &mpsc::Sender<Bytes>, at_once: &Arc<Semaphore>) {
        let Ok(permit) = at_once.clone().acquire_owned().await else { return };
        let (this, ident, out) = (self.clone(), ident.clone(), out.clone());
        tokio::spawn(async move {
            for reply in this.handle(&ident, frame).await {
                if out.send(reply).await.is_err() {
                    break;
                }
            }
            drop(permit);
        });
    }

    /// A worker's dialed connection (`FORKRAIL-CACHE/1`): its tunnel is gone, and its app's
    /// background work still reads and writes. The same frames, answers only.
    pub async fn dialed<S>(self: &Arc<Self>, mut stream: S, worker_id: &str, token: &str) -> Result<(), BoxError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let ident = {
            let recent = self.recent.lock().unwrap();
            recent.get(worker_id).filter(|r| r.until > now_ms() && same(&r.token, &digest(token))).map(|r| r.ident.clone())
        };
        let Some(ident) = ident else { return Err(format!("cache connection from {worker_id}: unknown worker or bad token").into()) };
        forkrail_common::ack_preface(&mut stream).await?;
        let (mut read, mut write) = tokio::io::split(stream);
        let (out, mut out_rx) = mpsc::channel::<Bytes>(OUT_QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(bytes) = out_rx.recv().await {
                if write.write_all(&bytes).await.is_err() || write.flush().await.is_err() {
                    break;
                }
            }
            let _ = write.shutdown().await;
        });
        let at_once = Arc::new(Semaphore::new(AT_ONCE));
        let mut buf = BytesMut::with_capacity(16 << 10);
        'conn: loop {
            match tokio::time::timeout(DIALED_IDLE, read.read_buf(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {}
                _ => break,
            }
            loop {
                match wire::take_frame(&mut buf) {
                    Ok(Some(frame)) => self.answer(&ident, frame, &out, &at_once).await,
                    Ok(None) => break,
                    Err(_) => break 'conn,
                }
            }
        }
        // What's being answered still goes out.
        let _ = at_once.acquire_many(AT_ONCE as u32).await;
        drop(out);
        let _ = writer.await;
        Ok(())
    }

    // ---- Cleanup ----

    /// Delete what's kept for deployments and projects that have left the routing table: a
    /// deployment's pages and entries, a project's data cache. One that's missing must be missing
    /// at two sweeps running (an hour apart), and nothing is deleted on an empty table.
    pub async fn sweep(&self, deployments: &HashSet<String>, projects: &HashSet<String>) -> usize {
        let Some(disk) = self.cache.disk() else { return 0 };
        if deployments.is_empty() {
            return 0;
        }
        let mut removed = 0;
        for (scope, bytes) in disk.scopes() {
            let live = match scope.split_once(".data.") {
                Some((project, _)) => projects.contains(project),
                None => deployments.contains(&scope),
            };
            if live {
                self.gone.lock().unwrap().remove(&scope);
                continue;
            }
            // Missing once: it may be a deployment this router's table hasn't got yet.
            if self.gone.lock().unwrap().insert(scope.clone()) {
                continue;
            }
            let names = disk.remove_scope(&scope).await;
            for name in &names {
                self.cache.forget(name);
            }
            self.gone.lock().unwrap().remove(&scope);
            self.seed_index.lock().unwrap().remove(&scope);
            info!(%scope, pages = names.len(), kb = bytes >> 10, "pages of a deployment or project that's gone deleted");
            removed += names.len();
        }
        removed
    }
}

/// An entry's headers as a worker gets them: its own, without the store's two.
fn own_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers.iter().filter(|(name, _)| **name != MARK && **name != CACHE_TAG).filter_map(|(name, value)| Some((name.as_str().to_owned(), std::str::from_utf8(value.as_bytes()).ok()?.to_owned()))).collect()
}

/// A stream's place among those told of changes; taken out when it drops.
struct Subscription {
    next: Weak<NextCache>,
    id: u64,
    keys: Vec<String>,
    cut: Arc<Notify>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let Some(next) = self.next.upgrade() else { return };
        let mut subs = next.subs.lock().unwrap();
        for key in &self.keys {
            if let Some(list) = subs.get_mut(key) {
                list.retain(|sub| sub.id != self.id);
                if list.is_empty() {
                    subs.remove(key);
                }
            }
        }
    }
}

/// Ends the worker's cache stream when dropped. The `Worker` holds it.
pub struct Follow {
    _stop: oneshot::Sender<()>,
}

/// The request's body on a worker's cache stream: this router's frames, until every sender drops.
struct Outgoing(mpsc::Receiver<Bytes>);

impl hyper::body::Body for Outgoing {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        self.0.poll_recv(cx).map(|bytes| bytes.map(|b| Ok(hyper::body::Frame::data(b))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Disk;

    fn ident(worker: &str, deployment: &str) -> Ident {
        Ident { worker: worker.into(), project: "shop".into(), environment: "production".into(), deployment: deployment.into() }
    }

    fn key(name: &str) -> String {
        forkrail_common::hex(&Sha256::digest(name.as_bytes()))
    }

    fn next_on(cache: Arc<ResponseCache>, seeds: Option<Arc<Statics>>, data_max: u64) -> Arc<NextCache> {
        // Not `start`: the global is one a process, and each test has its own cache.
        let next = Arc::new(NextCache { cache: cache.clone(), seeds, seed_index: Mutex::default(), leases: Mutex::default(), subs: Mutex::default(), recent: Mutex::default(), data_max, ids: AtomicU64::new(1), gone: Mutex::default() });
        let weak = Arc::downgrade(&next);
        cache.tags.watch({
            let weak = weak.clone();
            move |changed| {
                if let Some(next) = weak.upgrade() {
                    next.marks_changed(changed);
                }
            }
        });
        cache.watch_kept({
            let weak = weak.clone();
            move |hash, entry| {
                if let Some(next) = weak.upgrade() {
                    next.kept(hash, entry);
                }
            }
        });
        cache.watch_leases(move |hash, until| {
            if let Some(next) = weak.upgrade() {
                next.lease_heard(hash, until);
            }
        });
        next
    }

    fn memory() -> Arc<NextCache> {
        next_on(Arc::new(ResponseCache::new(1 << 20, 8 << 20, None, None, true)), None, 1 << 30)
    }

    fn page(name: &str, html: &str, rev: Option<f64>) -> (wire::Set, Bytes) {
        let meta = wire::Meta { kind: "APP_PAGE".into(), key_hash: key(name), key: name.into(), parts: vec![("html".into(), html.len() as u64), ("rsc".into(), 3)], rev, ..wire::Meta::default() };
        let set = wire::Set { class: Some(Class::Deployment), key_hash: key(name), status: 200, headers: vec![("content-type".into(), "text/html".into())], tags: vec!["posts".into(), "_N_T_/blog/layout".into(), "a,b".into()], rev };
        (set, Bytes::from(meta.body(&[html.as_bytes(), b"1:a"])))
    }

    async fn set(next: &Arc<NextCache>, who: &Ident, set: wire::Set, body: Bytes) -> wire::Done {
        let frames = next.handle(who, Frame::new(kind::SET, 1, &set, &body)).await;
        open::<wire::Done>(&frames[0]).0
    }

    async fn get(next: &Arc<NextCache>, who: &Ident, asked: wire::Get) -> (wire::GetReply, Bytes) {
        let frames = next.handle(who, Frame::new(kind::GET, 2, &asked, b"")).await;
        open::<wire::GetReply>(&frames[0])
    }

    fn open<H: for<'de> serde::Deserialize<'de>>(bytes: &Bytes) -> (H, Bytes) {
        let mut buf = BytesMut::from(&bytes[..]);
        wire::take_frame(&mut buf).unwrap().unwrap().open::<H>().unwrap()
    }

    fn asking(name: &str) -> wire::Get {
        wire::Get { class: Some(Class::Deployment), key_hash: key(name), ..wire::Get::default() }
    }

    async fn tags(next: &Arc<NextCache>, who: &Ident, tags: &[&str], profile: bool, expire: Option<f64>) -> wire::Done {
        let asked = wire::Tags { tags: tags.iter().map(|t| (*t).to_owned()).collect(), profile, expire };
        open::<wire::Done>(&next.handle(who, Frame::new(kind::TAGS, 3, &asked, b"")).await[0]).0
    }

    #[tokio::test]
    async fn one_workers_entry_is_every_workers() {
        let next = memory();
        let (a, b, other) = (ident("dep1.aa", "dep1"), ident("dep1.bb", "dep1"), ident("dep2.cc", "dep2"));
        assert!(!get(&next, &a, asking("/blog/a")).await.0.hit, "nothing kept yet");
        let (asked, body) = page("/blog/a", "<p>one</p>", Some(60.0));
        let done = set(&next, &a, asked, body.clone()).await;
        assert!(done.ok && done.t > 0);
        // Another environment of the same deployment reads what the first wrote, as it wrote it.
        let (reply, got) = get(&next, &b, asking("/blog/a")).await;
        assert!(reply.hit && !reply.same);
        assert_eq!((reply.verdict, reply.t, reply.status), (Some(wire::Verdict::Fresh), done.t, 200));
        assert_eq!(got, body);
        assert_eq!(reply.headers, [("content-type".to_owned(), "text/html".to_owned())], "the page's own headers, without the store's");
        assert_eq!(reply.tags, ["posts", "_N_T_/blog/layout"], "a tag with a comma can't be kept");
        let (meta, at) = wire::Meta::of(&got).unwrap();
        assert_eq!(&got[meta.part("html", at).unwrap()], b"<p>one</p>");
        // The copy it has is still the one: no body again.
        let (reply, got) = get(&next, &b, wire::Get { have: Some(done.t), ..asking("/blog/a") }).await;
        assert!(reply.hit && reply.same && got.is_empty());
        // Pages are a deployment's own.
        assert!(!get(&next, &other, asking("/blog/a")).await.0.hit);
        // A second write is later than the first, whatever the clock says.
        let (asked, body) = page("/blog/a", "<p>two</p>", Some(60.0));
        assert!(set(&next, &b, asked, body).await.t > done.t);
        // Nothing that isn't an entry is kept.
        let (asked, _) = page("/x", "", None);
        assert!(set(&next, &a, asked, Bytes::from_static(b"junk")).await.error.is_some());
        let frames = next.handle(&a, Frame { kind: kind::GET, id: 9, payload: Bytes::from_static(b"\0\0\0\x02{}") }).await;
        assert!(open::<wire::GetReply>(&frames[0]).0.error.is_some());
    }

    #[tokio::test]
    async fn the_data_cache_is_the_projects_across_deployments() {
        let next = memory();
        let (old, new) = (ident("dep1.aa", "dep1"), ident("dep2.bb", "dep2"));
        let preview = Ident { environment: "preview".into(), ..ident("dep3.cc", "dep3") };
        let meta = wire::Meta { kind: "FETCH".into(), key_hash: key("f1"), parts: vec![("body".into(), 4)], rev: Some(30.0), ..wire::Meta::default() };
        let asked = wire::Set { class: Some(Class::Data), key_hash: key("f1"), tags: vec!["products".into()], rev: Some(30.0), ..wire::Set::default() };
        assert!(set(&next, &old, asked, Bytes::from(meta.body(&[b"data"]))).await.ok);
        let data = |name: &str| wire::Get { class: Some(Class::Data), key_hash: key(name), ..wire::Get::default() };
        assert!(get(&next, &new, data("f1")).await.0.hit, "a new deployment reads the old one's data");
        assert!(!get(&next, &preview, data("f1")).await.0.hit, "another environment doesn't");
        assert!(!get(&next, &new, asking("f1")).await.0.hit, "and it isn't among the deployment's own");
        // Over 2 MB isn't kept.
        let big = vec![0u8; wire::MAX_DATA_BYTES];
        let meta = wire::Meta { kind: "FETCH".into(), key_hash: key("big"), parts: vec![("body".into(), big.len() as u64)], ..wire::Meta::default() };
        let asked = wire::Set { class: Some(Class::Data), key_hash: key("big"), ..wire::Set::default() };
        assert_eq!(set(&next, &old, asked, Bytes::from(meta.body(&[&big]))).await.error.as_deref(), Some("too large to keep"));
    }

    #[tokio::test]
    async fn tags_reach_every_worker_and_one_reader_makes_a_stale_entry_again() {
        let next = memory();
        let (a, b) = (ident("dep1.aa", "dep1"), ident("dep1.bb", "dep1"));
        let (asked, body) = page("/blog/a", "<p>one</p>", None);
        let first = set(&next, &a, asked, body).await.t;
        tokio::time::sleep(Duration::from_millis(3)).await;
        // With a profile: stale, for one reader to make again while the others are served it.
        assert!(tags(&next, &b, &["posts"], true, None).await.ok);
        assert_eq!(get(&next, &a, asking("/blog/a")).await.0.verdict, Some(wire::Verdict::Lead));
        assert_eq!(get(&next, &b, asking("/blog/a")).await.0.verdict, Some(wire::Verdict::Wait));
        assert_eq!(get(&next, &a, asking("/blog/a")).await.0.verdict, Some(wire::Verdict::Wait), "the one making it reads it again");
        // Written again: fresh for everyone, and the next time it's stale someone leads again.
        let (asked, body) = page("/blog/a", "<p>two</p>", None);
        let second = set(&next, &a, asked, body).await.t;
        assert!(second > first);
        assert_eq!(get(&next, &b, asking("/blog/a")).await.0.verdict, Some(wire::Verdict::Fresh));
        // A soft tag (`revalidatePath`) the entry carries, without a profile: gone for everyone.
        tokio::time::sleep(Duration::from_millis(3)).await;
        assert!(tags(&next, &a, &["_N_T_/blog/layout"], false, None).await.ok);
        assert!(!get(&next, &b, asking("/blog/a")).await.0.hit);
        // A tag of the reader's route, which the entry doesn't carry, judges it too.
        let (asked, body) = page("/blog/b", "<p>b</p>", None);
        set(&next, &a, asked, body).await;
        tokio::time::sleep(Duration::from_millis(3)).await;
        tags(&next, &a, &["_N_T_/other"], false, None).await;
        assert!(get(&next, &b, asking("/blog/b")).await.0.hit);
        assert!(!get(&next, &b, wire::Get { soft: vec!["_N_T_/other".into()], ..asking("/blog/b") }).await.0.hit);
        // The same marks C16's cache tags use, in the worker's project and environment only.
        assert!(next.cache.tags.of("shop", "production").iter().any(|(t, m)| t == "posts" && m.stale_before > 0));
        assert!(next.cache.tags.of("shop", "preview").is_empty() && next.cache.tags.of("blog", "production").is_empty());
        // With a profile that expires: stale until then, deleted after.
        let (asked, body) = page("/blog/c", "<p>c</p>", None);
        set(&next, &a, asked, body).await;
        tokio::time::sleep(Duration::from_millis(3)).await;
        tags(&next, &a, &["posts"], true, Some(0.05)).await;
        assert_eq!(get(&next, &b, asking("/blog/c")).await.0.verdict, Some(wire::Verdict::Lead));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!get(&next, &b, asking("/blog/c")).await.0.hit);
    }

    #[tokio::test]
    async fn an_entry_goes_stale_by_its_own_time_or_the_readers() {
        let next = memory();
        let a = ident("dep1.aa", "dep1");
        let (asked, body) = page("/t", "<p>t</p>", Some(0.02));
        set(&next, &a, asked, body).await;
        assert_eq!(get(&next, &a, asking("/t")).await.0.verdict, Some(wire::Verdict::Fresh));
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(get(&next, &a, asking("/t")).await.0.verdict, Some(wire::Verdict::Lead));
        // A reader with a longer time of its own (`fetch`'s `next.revalidate`) still finds it fresh.
        assert_eq!(get(&next, &a, wire::Get { rev: Some(60.0), ..asking("/t") }).await.0.verdict, Some(wire::Verdict::Fresh));
        // `revalidate: false` never goes stale by time.
        let (asked, body) = page("/never", "<p>n</p>", None);
        set(&next, &a, asked, body).await;
        assert_eq!(get(&next, &a, asking("/never")).await.0.verdict, Some(wire::Verdict::Fresh));
    }

    #[tokio::test]
    async fn workers_are_told_of_marks_and_writes_unasked() {
        let next = memory();
        let (a, b) = (ident("dep1.aa", "dep1"), ident("dep1.bb", "dep1"));
        let elsewhere = Ident { project: "blog".into(), ..ident("dep9.zz", "dep9") };
        let (tx_b, mut rx_b) = mpsc::channel(16);
        let (tx_e, mut rx_e) = mpsc::channel(16);
        let sub_b = next.subscribe(&b, tx_b);
        let _sub_e = next.subscribe(&elsewhere, tx_e);
        // A mark made by `a` reaches `b` as the mark now stands.
        tags(&next, &a, &["posts"], false, None).await;
        let (marks, _) = open::<wire::Marks>(&rx_b.try_recv().unwrap());
        assert_eq!((marks.marks.len(), marks.marks[0].tag.as_str(), marks.synced), (1, "posts", false));
        assert!(marks.marks[0].d > 0 && marks.now >= marks.marks[0].d);
        // And a write by `a`, by its key.
        let (asked, body) = page("/blog/a", "<p>one</p>", None);
        let t = set(&next, &a, asked, body).await.t;
        let (changed, _) = open::<wire::Changed>(&rx_b.try_recv().unwrap());
        assert_eq!((changed.class, changed.key_hash, changed.t), (Class::Deployment, key("/blog/a"), t));
        assert!(rx_e.try_recv().is_err(), "another project's worker hears neither");
        // A purge from the CLI (the routing table) is the same mark, and is sent the same way.
        let purge = forkrail_common::purge::Purge { op: PurgeOp::Invalidate, tags: vec!["posts".into()], src_images: false, target: Some("production".into()), deadline_s: None, at_ms: now_ms(), by: None };
        next.cache.table_purges(&[("shop".to_owned(), vec![purge])].into());
        let (marks, _) = open::<wire::Marks>(&rx_b.try_recv().unwrap());
        assert!(marks.marks[0].s > 0 && marks.marks[0].d == 0, "the change, for the worker to merge into what it has");
        // A new stream catches up: everything, or what changed since it last heard.
        let frames = next.handle(&b, Frame::new(kind::SYNC, 5, &wire::Sync { since: 0 }, b"")).await;
        let (all, _) = open::<wire::Marks>(&frames[0]);
        assert!(all.synced && all.marks.len() == 1 && frames.len() == 2);
        let frames = next.handle(&b, Frame::new(kind::SYNC, 5, &wire::Sync { since: now_ms() + 10 * SYNC_MARGIN_MS }, b"")).await;
        let (none, _) = open::<wire::Marks>(&frames[0]);
        assert!(none.synced && none.marks.is_empty(), "nothing new: one empty frame that says so");
        // Gone when its stream is.
        drop(sub_b);
        assert!(!next.subs.lock().unwrap().contains_key("dep1"));
    }

    #[tokio::test]
    async fn the_builds_seed_is_read_when_first_asked_for() {
        let dir = std::env::temp_dir().join(format!("forkrail-seed-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(dir.join(format!("{}dep1", wire::SEED_PREFIX))).unwrap();
        let name = "/blog/seeded";
        let meta = wire::Meta { kind: "APP_PAGE".into(), key_hash: key(name), key: name.into(), parts: vec![("html".into(), 11)], rev: Some(3600.0), ..wire::Meta::default() };
        let hash = crate::cache::hash(&wire::hash_input("dep1", &key(name)));
        let built = now_ms() - 5_000;
        let headers = vec![("content-type".to_owned(), "text/html".to_owned()), (wire::MARK_HEADER.to_owned(), "1".to_owned()), ("vercel-cache-tag".to_owned(), "posts".to_owned())];
        let encoded = wire::encode_page("dep1", &hash, 200, built, wire::fresh_until(built, Some(3600.0)), &headers, &meta.body(&[b"<p>seed</p>"]));
        std::fs::write(dir.join(wire::seed_entry_key("dep1", &key(name))), encoded).unwrap();
        let index = wire::SeedIndex { v: 1, built_ms: built, entries: vec![key(name)] };
        std::fs::write(dir.join(wire::seed_index_key("dep1")), serde_json::to_vec(&index).unwrap()).unwrap();
        let statics = Arc::new(Statics::new(crate::statics::Source::Dir(dir.clone()), 0));
        let next = next_on(Arc::new(ResponseCache::new(1 << 20, 8 << 20, None, None, true)), Some(statics), 1 << 30);
        let a = ident("dep1.aa", "dep1");
        // What the CLI wrote is what the router's own decoder reads, with the build's time.
        let (reply, body) = get(&next, &a, asking(name)).await;
        assert!(reply.hit && reply.t == built && reply.verdict == Some(wire::Verdict::Fresh));
        assert_eq!(reply.tags, ["posts"]);
        let (meta, at) = wire::Meta::of(&body).unwrap();
        assert_eq!(&body[meta.part("html", at).unwrap()], b"<p>seed</p>");
        // Kept from then on: the file can go.
        std::fs::remove_dir_all(dir.join(format!("{}dep1", wire::SEED_PREFIX))).unwrap();
        assert!(get(&next, &a, asking(name)).await.0.hit);
        // Not in the seed, another deployment: a miss without asking the bucket for the entry.
        assert!(!get(&next, &a, asking("/blog/other")).await.0.hit);
        assert!(!get(&next, &ident("dep2.bb", "dep2"), asking(name)).await.0.hit);
        // A tag invalidated since the build judges the seed as it judges any entry.
        tags(&next, &a, &["posts"], false, None).await;
        assert!(!get(&next, &a, asking(name)).await.0.hit);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn the_data_cache_has_its_own_size_and_gone_deployments_are_swept() {
        let dir = std::env::temp_dir().join(format!("forkrail-next-{}", forkrail_common::random_hex(6)));
        let cache = Arc::new(ResponseCache::new(1 << 20, 8 << 20, Some(Disk::open(dir.clone(), 64 << 20, 0).unwrap()), None, true));
        let next = next_on(cache.clone(), None, 40_000);
        let a = ident("dep1.aa", "dep1");
        let written = || async {
            for _ in 0..400 {
                if cache.writing.lock().unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let (asked, body) = page("/kept", "<p>page</p>", None);
        set(&next, &a, asked, body).await;
        for i in 0..8 {
            let name = format!("f{i}");
            let meta = wire::Meta { kind: "FETCH".into(), key_hash: key(&name), parts: vec![("body".into(), 10_000)], ..wire::Meta::default() };
            let asked = wire::Set { class: Some(Class::Data), key_hash: key(&name), ..wire::Set::default() };
            assert!(set(&next, &a, asked, Bytes::from(meta.body(&[&vec![b'x'; 10_000]]))).await.ok);
            written().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let scopes: HashMap<String, u64> = cache.disk().unwrap().scopes().into_iter().collect();
        assert!(scopes["shop.data.production"] <= 40_000, "the data cache stays within its size: {scopes:?}");
        assert!(scopes.contains_key("dep1"), "and never at a page's cost");
        let data = |name: &str| wire::Get { class: Some(Class::Data), key_hash: key(name), ..wire::Get::default() };
        assert!(get(&next, &a, data("f7")).await.0.hit && !get(&next, &a, data("f0")).await.0.hit, "the least used went first");
        // Swept: a deployment or project missing from the table at two sweeps running.
        let live: HashSet<String> = ["dep1".to_owned()].into();
        let projects: HashSet<String> = ["shop".to_owned()].into();
        assert_eq!(next.sweep(&live, &projects).await, 0);
        assert_eq!(next.sweep(&HashSet::new(), &HashSet::new()).await, 0, "never on an empty table");
        let others: HashSet<String> = ["dep2".to_owned()].into();
        assert_eq!(next.sweep(&others, &projects).await, 0, "missing once: left");
        assert_eq!(next.sweep(&others, &projects).await, 1, "missing twice: deleted");
        assert!(!get(&next, &a, asking("/kept")).await.0.hit);
        assert!(get(&next, &a, data("f7")).await.0.hit, "its project's data stays");
        let none: HashSet<String> = HashSet::new();
        next.sweep(&others, &none).await;
        assert!(next.sweep(&others, &none).await > 0);
        assert!(!get(&next, &a, data("f7")).await.0.hit && cache.disk().unwrap().scopes().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_retired_workers_dialed_connection_is_its_own() {
        let next = memory();
        let a = ident("dep1.aa", "dep1");
        next.remember(&a, "tok");
        let (asked, body) = page("/blog/a", "<p>one</p>", None);
        set(&next, &a, asked, body).await;
        // With its token: acked, then frames answered.
        let (mut worker, router) = tokio::io::duplex(1 << 16);
        let task = tokio::spawn({
            let next = next.clone();
            async move { next.dialed(router, "dep1.aa", "tok").await.map_err(|e| e.to_string()) }
        });
        let mut ack = [0u8; 3];
        worker.read_exact(&mut ack).await.unwrap();
        assert_eq!(&ack, b"OK\n");
        worker.write_all(&Frame::new(kind::GET, 7, &asking("/blog/a"), b"").encode()).await.unwrap();
        worker.write_all(&Frame::new(kind::TAGS, 8, &wire::Tags { tags: vec!["posts".into()], profile: false, expire: None }, b"").encode()).await.unwrap();
        let mut buf = BytesMut::new();
        let mut frames = Vec::new();
        while frames.len() < 2 {
            worker.read_buf(&mut buf).await.unwrap();
            while let Some(frame) = wire::take_frame(&mut buf).unwrap() {
                frames.push(frame);
            }
        }
        frames.sort_by_key(|f| f.id);
        assert!(frames[0].open::<wire::GetReply>().unwrap().0.hit && frames[1].open::<wire::Done>().unwrap().0.ok);
        drop(worker);
        task.await.unwrap().unwrap();
        // Another's token, or a worker never seen: refused before anything is read.
        let (_w, router) = tokio::io::duplex(1024);
        assert!(next.dialed(router, "dep1.aa", "other").await.is_err());
        let (_w, router) = tokio::io::duplex(1024);
        assert!(next.dialed(router, "dep1.zz", "tok").await.is_err());
    }

    /// Two routers in one process, linked as `peers.rs` links them: what a worker of either
    /// writes or revalidates is the other's too, and a stale entry is made again by one.
    #[tokio::test]
    async fn two_regions_share_entries_marks_and_regenerations() {
        use crate::cache::peers::{Answer, Ask, Link, Renders};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peers: std::collections::BTreeMap<String, String> = [("us-east-1".to_owned(), listener.local_addr().unwrap().to_string()), ("us-west-1".to_owned(), "127.0.0.1:9".to_owned())].into();
        let home_link = Arc::new(Link::new("us-east-1".into(), "us-east-1".into(), peers.clone(), &[5u8; 32]).unwrap());
        let home_cache = Arc::new(ResponseCache::new(1 << 20, 8 << 20, None, Some(home_link.clone()), true));
        let home = next_on(home_cache.clone(), None, 1 << 30);
        let renders: Renders = Arc::new(|_: Ask| Box::pin(async { Answer::Unknown }));
        tokio::spawn(home_link.clone().serve(listener, home_cache.clone(), renders, crate::cache::peers::tests::no_runs()));
        let edge_link = Arc::new(Link::new("us-west-1".into(), "us-east-1".into(), peers, &[5u8; 32]).unwrap());
        let edge_cache = Arc::new(ResponseCache::new(1 << 20, 8 << 20, None, Some(edge_link.clone()), true));
        let edge = next_on(edge_cache.clone(), None, 1 << 30);
        tokio::spawn(edge_link.clone().run(edge_cache.clone(), Vec::new));
        let (east, west) = (ident("dep1.east", "dep1"), ident("dep1.west", "dep1"));
        async fn until<F: std::future::Future<Output = bool>>(what: impl Fn() -> F) -> bool {
            for _ in 0..400 {
                if what().await {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            false
        }
        assert!(until(|| async { edge_link.up() }).await, "the edge follows home");
        // Written by the edge's worker: home's worker reads it. Written by home's: the edge's does,
        // and the edge's workers are told.
        let (asked, body) = page("/from-west", "<p>west</p>", Some(0.05));
        set(&edge, &west, asked, body).await;
        assert!(until(|| async { get(&home, &east, asking("/from-west")).await.0.hit }).await, "an edge's write reaches home");
        let (tx, mut told) = mpsc::channel(16);
        let _sub = edge.subscribe(&west, tx);
        let (asked, body) = page("/from-east", "<p>east</p>", None);
        let t = set(&home, &east, asked, body).await.t;
        assert!(until(|| async { get(&edge, &west, asking("/from-east")).await.0.t == t }).await, "home's write is pushed to the edge");
        let (changed, _) = open::<wire::Changed>(&told.recv().await.unwrap());
        assert_eq!((changed.key_hash, changed.t), (key("/from-east"), t));
        // Stale by now: the edge's reader leads, and home's readers are served it meanwhile.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(get(&edge, &west, asking("/from-west")).await.0.verdict, Some(wire::Verdict::Lead));
        let hash = crate::cache::hash(&wire::hash_input("dep1", &key("/from-west")));
        assert!(until(|| async { home.leases.lock().unwrap().contains_key(&hash) }).await, "home hears who is making it");
        assert_eq!(get(&home, &east, asking("/from-west")).await.0.verdict, Some(wire::Verdict::Wait));
        // Made again at the edge: fresh in both, and home's readers may lead the next time.
        let (asked, body) = page("/from-west", "<p>west 2</p>", Some(60.0));
        let t = set(&edge, &west, asked, body).await.t;
        assert!(until(|| async { get(&home, &east, asking("/from-west")).await.0.t == t }).await);
        assert_eq!(get(&home, &east, asking("/from-west")).await.0.verdict, Some(wire::Verdict::Fresh));
        assert!(!home.leases.lock().unwrap().contains_key(&hash));
        // Revalidated at the edge: gone at home too, and the edge's own worker never read it again.
        tokio::time::sleep(Duration::from_millis(3)).await;
        tags(&edge, &west, &["posts"], false, None).await;
        assert!(!get(&edge, &west, asking("/from-east")).await.0.hit, "at once where it was called");
        assert!(until(|| async { !get(&home, &east, asking("/from-east")).await.0.hit }).await, "and in the other region");
    }

    #[test]
    fn nexts_revalidations_as_marks() {
        let asked = |profile, expire| wire::Tags { tags: vec![], profile, expire };
        assert_eq!(mark_of(&asked(false, None), 100), Mark { deleted_before: 100, ..Mark::default() });
        assert_eq!(mark_of(&asked(false, Some(60.0)), 100), Mark { deleted_before: 100, ..Mark::default() }, "no profile: now, whatever else is said");
        assert_eq!(mark_of(&asked(true, None), 100), Mark { stale_before: 100, ..Mark::default() });
        assert_eq!(mark_of(&asked(true, Some(1.5)), 100), Mark { pending: vec![(100, 1_600)], ..Mark::default() });
        assert_eq!(mark_of(&asked(true, Some(0.0)), 100), Mark { deleted_before: 100, ..Mark::default() });
    }
}
