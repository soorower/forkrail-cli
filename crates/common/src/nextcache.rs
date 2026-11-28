//! Next.js's shared cache (N2): what the worker's runtime, the router and the CLI agree on.
//!
//! A Next.js deployment's caches (ISR pages, route handlers' answers, the data cache, `'use cache'`
//! entries, optimized images) are kept by the router, one copy for every worker and region, and
//! reached from the worker over its tunnel. Next's two interfaces (`cacheHandler`,
//! `cacheHandlers`) are implemented in the worker (`crates/runtime/src/next-cache.mjs`); the
//! store is C16's page store (`crates/router/src/cache/next.rs` has the design in full).
//!
//! **Frames.** The same on every hop (Next's process to its runtime on loopback, the runtime to
//! the router on the tunnel's cache stream or a dialed connection):
//!
//! ```text
//! [kind u8][id u32 BE][length u32 BE] [head length u32 BE][head: JSON][body: bytes]
//! ```
//!
//! `id` pairs an answer with its request (0 for what the router sends unasked). The head is one
//! of the structs below; the body is an entry's parts end to end, or empty.
//!
//! **Entries.** A kept entry is a page of C16's response cache: its status and headers are the
//! page's own, with `vercel-cache-tag` holding its tags and [`MARK_HEADER`] marking it as Next's;
//! its body is `[meta length u32 BE][meta: JSON, `Meta`][parts]`.

use serde::{Deserialize, Serialize};

/// The router opens one long request on this path to each worker that announced the `cache`
/// feature, with the worker's token in [`TOKEN_HEADER`] (as the log stream is opened): frames go
/// both ways in its bodies.
pub const PATH: &str = "/_forkrail/cache";
pub const TOKEN_HEADER: &str = "x-forkrail-cache-token";
/// On a kept entry: it's one of Next's, its body laid out as this module says.
pub const MARK_HEADER: &str = "x-forkrail-next";

/// The largest page entry (C16's and Vercel's largest page), and the largest data or
/// `'use cache'` entry (Vercel's "Item size: 2 MB").
pub const MAX_PAGE_BYTES: usize = 20 << 20;
pub const MAX_DATA_BYTES: usize = 2 << 20;
/// A frame is an entry and its head at most.
pub const MAX_FRAME: usize = MAX_PAGE_BYTES + (1 << 20);
/// A frame's head.
pub const MAX_HEAD: usize = 512 << 10;
/// Tags on one entry and a tag's length (Next's and Vercel's limits: 128 tags of 256 bytes).
pub const MAX_TAGS: usize = 128;
pub const MAX_TAG_BYTES: usize = 256;
/// A cache worker's token is remembered this long after it connected, so work still running
/// after the router retired it (`after()`, `waitUntil`) can reach the cache by dialing in.
pub const REMEMBER_WORKER_S: u64 = 15 * 60;
/// How long one reader's regeneration of a stale entry keeps the others from starting their own.
pub const LEASE_MS: u64 = 60_000;

/// Frame kinds. Requests are below 16, the router's answers and pushes from 16, and what the
/// runtime tells Next's process itself from 32.
pub mod kind {
    pub const GET: u8 = 1;
    pub const SET: u8 = 2;
    pub const TAGS: u8 = 3;
    pub const SYNC: u8 = 4;
    pub const REPLY: u8 = 16;
    /// Tag marks, unasked: as they're made, and the catch-up after a `SYNC`.
    pub const MARKS: u8 = 17;
    /// An entry was written (by anyone, in any region): copies kept in a process are out of date.
    pub const CHANGED: u8 = 18;
    /// From Next's process to its runtime, first: the loopback secret.
    pub const HELLO: u8 = 32;
    /// From the runtime: a tunnel's cache stream is up (a new invocation), or gone.
    pub const UP: u8 = 33;
    pub const DOWN: u8 = 34;
}

/// Where an entry lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Class {
    /// The deployment's own: pages, route handlers' answers, images, `'use cache'` entries.
    #[serde(rename = "d")]
    Deployment,
    /// The project and environment's data cache (`fetch`, `unstable_cache`), kept across
    /// deployments as Vercel's is.
    #[serde(rename = "p")]
    Data,
}

/// The name an entry's scope has in the store: a deployment's id, or `<project>.data.<environment>`
/// (a deployment's id has no dot, so the two never meet).
pub fn scope(class: Class, project: &str, environment: &str, deployment: &str) -> String {
    match class {
        Class::Deployment => deployment.to_owned(),
        Class::Data => data_scope(project, environment),
    }
}

pub fn data_scope(project: &str, environment: &str) -> String {
    format!("{project}.data.{}", crate::purge::environment(environment))
}

/// The text hashed (SHA-256, hex) into an entry's name in the store. `key_hash` is the hex
/// SHA-256 of Next's own key, made in the worker.
pub fn hash_input(scope: &str, key_hash: &str) -> String {
    format!("next\n{scope}\n{key_hash}")
}

/// `GET`: an entry, judged by the router.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Get {
    #[serde(rename = "c")]
    pub class: Option<Class>,
    /// Hex SHA-256 of Next's key.
    #[serde(rename = "k")]
    pub key_hash: String,
    /// The copy the process has (its `stored_ms`): if it's still the one, no body comes back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub have: Option<u64>,
    /// The reader's own revalidate time in seconds, when it has one (`fetch`'s
    /// `next.revalidate`); otherwise the entry's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<f64>,
    /// Tags of the reader's route (`_N_T_/…`) and the reader's own tags, judged with the entry's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub soft: Vec<String>,
}

/// What the router says of an entry it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Fresh,
    /// Stale, and this reader is the one to make it again.
    Lead,
    /// Stale, and another reader is making it again: served as it is meanwhile.
    Wait,
}

/// `REPLY` to a `GET`. With `hit` and without `same`, the body is the entry's body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetReply {
    #[serde(default)]
    pub hit: bool,
    /// The process's copy (`have`) is still the one: no body.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub same: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// When the entry was written, by the router's clock (Unix ms).
    #[serde(default)]
    pub t: u64,
    /// The entry's status and headers (a page's own), as pairs.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub status: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The router's clock now.
    #[serde(default)]
    pub now: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn is_zero_u16(n: &u16) -> bool {
    *n == 0
}

/// `SET`: the frame's body is the entry's body (`Meta` and parts).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Set {
    #[serde(rename = "c")]
    pub class: Option<Class>,
    #[serde(rename = "k")]
    pub key_hash: String,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub status: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Seconds until it's stale; none for never (`revalidate: false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<f64>,
}

/// `REPLY` to a `SET` or `TAGS`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Done {
    #[serde(default)]
    pub ok: bool,
    /// When the router wrote it (an entry's `stored_ms`, a mark's time).
    #[serde(default)]
    pub t: u64,
    #[serde(default)]
    pub now: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `TAGS`: Next's `revalidateTag` / `updateTag` / `revalidatePath` (as soft tags).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tags {
    pub tags: Vec<String>,
    /// Called with a profile (`durations` given): stale now, served while made again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub profile: bool,
    /// The profile's `expire` in seconds: deleted that long from now. None with a profile is
    /// stale only; without a profile the tags are deleted now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire: Option<f64>,
}

/// `SYNC`: send this worker its project and environment's marks changed since `since` (the
/// router's clock, from the last `MARKS` it had; 0 for all).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sync {
    #[serde(default)]
    pub since: u64,
}

/// One tag's mark, as `cache/tags.rs` keeps it: made before `s` is stale, before `d` deleted, and
/// for each `(at, until)` of `p`, made before `at` is stale until `until` and deleted after.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagMark {
    pub tag: String,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub s: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub d: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub p: Vec<(u64, u64)>,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

/// `MARKS`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Marks {
    pub marks: Vec<TagMark>,
    /// The catch-up a `SYNC` asked for ends with this one: the worker has every mark.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synced: bool,
    pub now: u64,
}

/// `CHANGED`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Changed {
    #[serde(rename = "c")]
    pub class: Class,
    #[serde(rename = "k")]
    pub key_hash: String,
    pub t: u64,
}

/// What an entry's body starts with (after its length): what it is and where its parts are.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// Next's kind (`APP_PAGE`, `APP_ROUTE`, `PAGES`, `FETCH`, `REDIRECT`, `IMAGE`), `USE_CACHE`,
    /// or `NULL` (Next kept "nothing here": a `notFound` page).
    pub kind: String,
    #[serde(rename = "k")]
    pub key_hash: String,
    /// Next's own key, for whoever reads the store (N3, a person).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// The parts after the meta, in order: name and length. `html`, `rsc`, `seg:<path>`,
    /// `postponed`, `data` (Pages Router's page data, JSON), `body`, `buffer`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<(String, u64)>,
    /// Seconds until stale, and until it may no longer be served (Next's `revalidate`, `expire`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<f64>,
    /// A `'use cache'` entry's own time (Unix ms, when its making began) and `stale` seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<f64>,
    /// The rest of the value, as the worker wrote it (a fetch's status and headers, a redirect's
    /// props, an image's etag): only the worker reads it.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub x: serde_json::Value,
}

impl Meta {
    /// An entry's body: the meta, then the parts as they are.
    pub fn body(&self, parts: &[&[u8]]) -> Vec<u8> {
        let meta = serde_json::to_vec(self).expect("plain fields");
        let mut out = Vec::with_capacity(4 + meta.len() + parts.iter().map(|p| p.len()).sum::<usize>());
        out.extend_from_slice(&(meta.len() as u32).to_be_bytes());
        out.extend_from_slice(&meta);
        for part in parts {
            out.extend_from_slice(part);
        }
        out
    }

    /// The meta of an entry's body, and where its parts start; `None` if it isn't one, or its
    /// parts aren't all there.
    pub fn of(body: &[u8]) -> Option<(Meta, usize)> {
        let len = u32::from_be_bytes(body.get(..4)?.try_into().ok()?) as usize;
        let meta: Meta = serde_json::from_slice(body.get(4..4 + len)?).ok()?;
        // The lengths come from a worker: added up so they can't wrap around to the body's.
        let parts = meta.parts.iter().try_fold(0u64, |sum, (_, n)| sum.checked_add(*n))?;
        (Some(body.len() as u64) == (4 + len as u64).checked_add(parts)).then_some((meta, 4 + len))
    }

    /// Where part `name` is in the body whose parts start at `at`.
    pub fn part(&self, name: &str, at: usize) -> Option<std::ops::Range<usize>> {
        let mut pos = at;
        for (part, len) in &self.parts {
            let end = pos.checked_add(usize::try_from(*len).ok()?)?;
            if part == name {
                return Some(pos..end);
            }
            pos = end;
        }
        None
    }
}

/// A tag as the store keeps it, or `None` for one it can't: empty, over 256 bytes, with a comma
/// (which separates tags wherever they're listed, Next's own `x-next-cache-tags` included) or a
/// character a header can't hold.
pub fn storable_tag(tag: &str) -> Option<&str> {
    let tag = tag.trim();
    let ok = !tag.is_empty() && crate::purge::tag_bytes(tag) <= MAX_TAG_BYTES && !tag.contains(',') && tag.chars().all(|c| !c.is_control());
    ok.then_some(tag)
}

// ---- Frames ----

/// One frame, read off a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub id: u32,
    /// Head length, head, body.
    pub payload: bytes::Bytes,
}

pub const FRAME_HEADER: usize = 9;

impl Frame {
    pub fn new<H: Serialize>(kind: u8, id: u32, head: &H, body: &[u8]) -> Frame {
        let head = serde_json::to_vec(head).expect("plain fields");
        let mut payload = Vec::with_capacity(4 + head.len() + body.len());
        payload.extend_from_slice(&(head.len() as u32).to_be_bytes());
        payload.extend_from_slice(&head);
        payload.extend_from_slice(body);
        Frame { kind, id, payload: payload.into() }
    }

    /// The frame as it goes on a connection.
    pub fn encode(&self) -> bytes::Bytes {
        let mut out = bytes::BytesMut::with_capacity(FRAME_HEADER + self.payload.len());
        out.extend_from_slice(&[self.kind]);
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out.freeze()
    }

    /// The head, read as `H`, and the body.
    pub fn open<H: for<'de> Deserialize<'de>>(&self) -> Option<(H, bytes::Bytes)> {
        let len = u32::from_be_bytes(self.payload.get(..4)?.try_into().ok()?) as usize;
        if len > MAX_HEAD {
            return None;
        }
        let head = serde_json::from_slice(self.payload.get(4..4 + len)?).ok()?;
        Some((head, self.payload.slice(4 + len..)))
    }
}

/// The next whole frame at the start of `buf`, taken out of it; `Ok(None)` until it has all come,
/// `Err` for one too large to be a frame (the connection is then no use).
pub fn take_frame(buf: &mut bytes::BytesMut) -> Result<Option<Frame>, &'static str> {
    if buf.len() < FRAME_HEADER {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize;
    if len > MAX_FRAME {
        return Err("a frame too large");
    }
    if buf.len() < FRAME_HEADER + len {
        return Ok(None);
    }
    let kind = buf[0];
    let id = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    bytes::Buf::advance(buf, FRAME_HEADER);
    Ok(Some(Frame { kind, id, payload: buf.split_to(len).freeze() }))
}

// ---- A page of C16's store, as bytes: what the CLI's seed writes and the router reads ----

const PAGE_MAGIC: &[u8] = b"forkrail-response 1\n";

/// An entry as the router keeps a page on disk (`crates/router/src/cache.rs`'s `encode`): the
/// format line, a line of JSON (when it was made, until when it's fresh, its scope and name),
/// its headers as `name\0value\n` and a blank line, then its body.
pub fn encode_page(scope: &str, hash: &str, status: u16, stored_ms: u64, fresh_until: Option<u64>, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let meta = serde_json::json!({ "status": status, "stored_ms": stored_ms, "fresh_until": fresh_until, "group": null, "deployment": scope, "hash": hash });
    let mut out = Vec::with_capacity(body.len() + 512);
    out.extend_from_slice(PAGE_MAGIC);
    out.extend_from_slice(&serde_json::to_vec(&meta).expect("plain fields"));
    out.push(b'\n');
    for (name, value) in headers {
        out.extend_from_slice(name.to_ascii_lowercase().as_bytes());
        out.push(0);
        out.extend_from_slice(value.as_bytes());
        out.push(b'\n');
    }
    out.push(b'\n');
    out.extend_from_slice(body);
    out
}

/// Whether a header can go in a kept page as it is (a name and value HTTP allows, and no line
/// break or NUL, which the page's encoding uses).
pub fn storable_header(name: &str, value: &str) -> bool {
    http::HeaderName::from_bytes(name.as_bytes()).is_ok() && http::HeaderValue::from_bytes(value.as_bytes()).is_ok() && !value.bytes().any(|b| b == b'\n' || b == 0)
}

/// The fresh-until time of an entry written at `stored_ms` that goes stale after `rev` seconds.
pub fn fresh_until(stored_ms: u64, rev: Option<f64>) -> Option<u64> {
    let rev = rev.filter(|r| r.is_finite())?;
    Some(stored_ms.saturating_add((rev.max(0.0) * 1000.0) as u64))
}

// ---- The build's seed ----

/// Where a deployment's seed is, among its static files (which the routers' role may read):
/// `<SEED_PREFIX><deployment>/index.json` (`SeedIndex`) and one object an entry,
/// `<SEED_PREFIX><deployment>/<key hash>`, each a page as `encode_page` writes it. Written by
/// `crates/cli/src/next-seed.mjs` after the build, uploaded with the deployment.
pub const SEED_PREFIX: &str = "static/next-seed/";

pub fn seed_index_key(deployment: &str) -> String {
    format!("{SEED_PREFIX}{deployment}/index.json")
}

pub fn seed_entry_key(deployment: &str, key_hash: &str) -> String {
    format!("{SEED_PREFIX}{deployment}/{key_hash}")
}

/// What a deployment's seed holds: the build's prerenders, by their keys' hashes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeedIndex {
    pub v: u32,
    /// When the build made them (Unix ms): their age.
    pub built_ms: u64,
    pub entries: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_wait_for_their_rest() {
        let get = Get { class: Some(Class::Data), key_hash: "ab".into(), have: Some(5), rev: Some(60.0), soft: vec!["_N_T_/a".into()] };
        let frame = Frame::new(kind::GET, 7, &get, b"");
        let wire = frame.encode();
        let mut buf = bytes::BytesMut::new();
        buf.extend_from_slice(&wire[..wire.len() - 3]);
        assert_eq!(take_frame(&mut buf), Ok(None), "half a frame waits");
        buf.extend_from_slice(&wire[wire.len() - 3..]);
        buf.extend_from_slice(&Frame::new(kind::REPLY, 7, &Done { ok: true, t: 9, now: 10, error: None }, b"xyz").encode());
        let first = take_frame(&mut buf).unwrap().unwrap();
        assert_eq!((first.kind, first.id), (kind::GET, 7));
        let (head, body): (Get, _) = first.open().unwrap();
        assert_eq!((head.class, head.key_hash.as_str(), head.have, head.soft.len(), body.len()), (Some(Class::Data), "ab", Some(5), 1, 0));
        let second = take_frame(&mut buf).unwrap().unwrap();
        let (done, body): (Done, _) = second.open().unwrap();
        assert!(done.ok && done.t == 9 && &body[..] == b"xyz" && buf.is_empty());
        // The JSON a worker writes by hand reads the same.
        let by_hand: Get = serde_json::from_str(r#"{"c":"d","k":"ff"}"#).unwrap();
        assert_eq!((by_hand.class, by_hand.have), (Some(Class::Deployment), None));
        // A length no frame has ends the connection.
        let mut bad = bytes::BytesMut::from(&[1u8, 0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff][..]);
        assert!(take_frame(&mut bad).is_err());
    }

    #[test]
    fn an_entrys_body_is_its_meta_and_parts() {
        let meta = Meta { kind: "APP_PAGE".into(), key_hash: "k".into(), key: "/blog/a".into(), parts: vec![("html".into(), 5), ("rsc".into(), 3), ("seg:/_tree".into(), 0), ("postponed".into(), 2)], rev: Some(60.0), ..Meta::default() };
        let body = meta.body(&[b"<p/>\n", b"1:a", b"", b"{}"]);
        let (back, at) = Meta::of(&body).unwrap();
        assert_eq!(back, meta);
        assert_eq!(&body[back.part("html", at).unwrap()], b"<p/>\n");
        assert_eq!(&body[back.part("rsc", at).unwrap()], b"1:a");
        assert_eq!(&body[back.part("postponed", at).unwrap()], b"{}");
        assert!(back.part("seg:/_tree", at).unwrap().is_empty() && back.part("data", at).is_none());
        assert!(Meta::of(&body[..body.len() - 1]).is_none(), "a part cut short isn't an entry");
        assert!(Meta::of(b"<html>").is_none());
        // Lengths that wrap around to the body's are no entry.
        let forged = Meta { kind: "APP_PAGE".into(), key_hash: "k".into(), parts: vec![("html".into(), u64::MAX), ("rsc".into(), 4)], ..Meta::default() };
        let mut body = forged.body(&[]);
        body.extend_from_slice(b"abc");
        assert!(Meta::of(&body).is_none());
        assert!(forged.part("rsc", usize::MAX - 1).is_none());
    }

    #[test]
    fn scopes_tags_and_times() {
        assert_eq!(scope(Class::Deployment, "shop", "preview", "shop-1a2b3c4d"), "shop-1a2b3c4d");
        assert_eq!(scope(Class::Data, "shop", "preview", "shop-1a2b3c4d"), "shop.data.preview");
        assert_eq!(scope(Class::Data, "shop", "", "x"), "shop.data.production");
        assert_ne!(hash_input("a", "k"), hash_input("b", "k"));
        assert_eq!(storable_tag(" posts "), Some("posts"));
        assert_eq!(storable_tag("_N_T_/blog/[slug]/layout"), Some("_N_T_/blog/[slug]/layout"));
        assert!(storable_tag("a,b").is_none() && storable_tag("").is_none() && storable_tag("a\nb").is_none() && storable_tag(&"x".repeat(257)).is_none());
        assert_eq!(fresh_until(1_000, Some(1.5)), Some(2_500));
        assert_eq!(fresh_until(1_000, None), None);
        assert_eq!(fresh_until(1_000, Some(f64::INFINITY)), None);
        assert!(storable_header("content-type", "text/html") && !storable_header("bad name", "x") && !storable_header("x-a", "a\nb"));
    }
}
