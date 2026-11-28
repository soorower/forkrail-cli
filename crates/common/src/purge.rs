//! Cache tags and purging by tag, as Vercel has them (vercel.com/docs/caching/cdn-cache/purge, read
//! 2026-11-16), shared by the CLI, the routing table, the runtime and the router.
//!
//! - A function's answer names its tags in `Vercel-Cache-Tag` (comma-separated), or the app adds
//!   them with `addCacheTag` from `@vercel/functions`. The router keeps them with the answer, and
//!   never passes the header on.
//! - A purge names tags (case-sensitive, never containing a comma: a comma separates two) or `*`
//!   for everything, in one project and environment: production, preview, or (from the CLI, the
//!   dashboard or the API) both. **Invalidate**: what was kept before is served once more
//!   (`STALE`) while it's made again. **Delete**: it's made again while the visitor waits
//!   (`REVALIDATED`); with a revalidation deadline, served stale until then.
//! - Purging by source image is the same on the image cache's system tags (the source, commas
//!   allowed, up to 1013 bytes).
//!
//! Limits, Vercel's: 256 UTF-8 bytes a tag (escaped characters at their escaped length), 128 tags
//! on a kept answer, 16 tags in one call of the API (and so the CLI), a deadline of up to a year.

use serde::{Deserialize, Serialize};

/// The answer header naming its tags (lowercase, as headers are compared).
pub const CACHE_TAG_HEADER: &str = "vercel-cache-tag";
/// The tag that matches everything kept for a project and environment (Vercel's dashboard `*`).
pub const ALL: &str = "*";
pub const MAX_TAG_BYTES: usize = 256;
pub const MAX_SRC_IMAGE_BYTES: usize = 1013;
/// Tags a kept answer keeps; more are dropped.
pub const MAX_TAGS_PER_ANSWER: usize = 128;
/// Tags in one purge from the CLI, the dashboard or the API (Vercel's REST API's bulk limit).
pub const MAX_TAGS_PER_CALL: usize = 16;
/// Tags in one purge from a function (`invalidateByTag([...])`): as many as an answer can have.
pub const MAX_TAGS_PER_RUNTIME_CALL: usize = MAX_TAGS_PER_ANSWER;
/// The longest revalidation deadline, in seconds (Vercel's: a year).
pub const MAX_DEADLINE_S: u64 = 31_536_000;
/// Purges a project's routing-table entry keeps, newest last: the routers apply each once they
/// see it, and remember it themselves.
pub const MAX_TABLE_PURGES: usize = 50;
/// The environments a purge may name.
pub const ENVIRONMENTS: [&str; 2] = ["production", "preview"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PurgeOp {
    /// Served stale once while made again in the background (`STALE`).
    Invalidate,
    /// Made again while the visitor waits (`REVALIDATED`), after the deadline if one is given.
    Delete,
}

impl PurgeOp {
    pub fn as_str(self) -> &'static str {
        match self {
            PurgeOp::Invalidate => "invalidate",
            PurgeOp::Delete => "delete",
        }
    }
}

/// One purge: what, of which environment, and when it was asked for (what was kept before then
/// is purged; what's kept after isn't).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Purge {
    pub op: PurgeOp,
    /// Tags, or source images with `src_images`; `*` is everything.
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub src_images: bool,
    /// `production` or `preview`; none for both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// A delete's revalidation deadline: what was kept is served stale (and made again) for this
    /// many seconds, and only then made while the visitor waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_s: Option<u64>,
    /// When it was asked for, in Unix milliseconds.
    #[serde(default)]
    pub at_ms: u64,
    /// Who asked (a CLI user, `dashboard`), for the table's readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// A tag's length as Vercel counts it: its UTF-8 bytes, with the characters JSON escapes (quotes,
/// backslashes, control characters) at their escaped length.
pub fn tag_bytes(tag: &str) -> usize {
    tag.chars()
        .map(|c| match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        })
        .sum()
}

/// The tags in a `Vercel-Cache-Tag` value (or a CLI's `--tag a,b`): split at commas, each trimmed,
/// empty ones skipped.
pub fn split_tags(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim).filter(|t| !t.is_empty())
}

/// What a tag can't be, or `None` if it's fine.
pub fn tag_problem(tag: &str, src_image: bool) -> Option<String> {
    let max = if src_image { MAX_SRC_IMAGE_BYTES } else { MAX_TAG_BYTES };
    if tag.is_empty() {
        return Some("an empty tag".into());
    }
    if !src_image && tag.contains(',') {
        return Some(format!("{tag:?}: a tag can't contain a comma (a comma separates two tags)"));
    }
    if tag_bytes(tag) > max {
        return Some(format!("{}…: at most {max} bytes", tag.chars().take(40).collect::<String>()));
    }
    None
}

/// `production` for anything but `preview`: deployments from before targets were recorded were
/// production's.
pub fn environment(env: &str) -> &'static str {
    if env == "preview" { "preview" } else { "production" }
}

impl Purge {
    /// Check it, with at most `max_tags` tags; tags given with commas are split first.
    pub fn check(&mut self, max_tags: usize) -> Result<(), String> {
        if !self.src_images {
            self.tags = self.tags.iter().flat_map(|t| split_tags(t)).map(str::to_owned).collect();
        } else {
            self.tags.retain(|t| !t.is_empty());
        }
        // In order, each once; a set to find repeats, as a function can send thousands at once.
        let given = std::mem::take(&mut self.tags);
        let mut seen = std::collections::HashSet::with_capacity(given.len().min(max_tags + 1));
        for tag in given {
            if seen.contains(&tag) {
                continue;
            }
            if self.tags.len() == max_tags {
                return Err(format!("at most {max_tags} tags at once"));
            }
            seen.insert(tag.clone());
            self.tags.push(tag);
        }
        if self.tags.is_empty() {
            return Err(if self.src_images { "name a source image".into() } else { "name a tag (or * for everything)".into() });
        }
        for tag in &self.tags {
            if tag != ALL
                && let Some(problem) = tag_problem(tag, self.src_images)
            {
                return Err(problem);
            }
        }
        if let Some(target) = &self.target
            && !ENVIRONMENTS.contains(&target.as_str())
        {
            return Err(format!("{target:?}: the target is production or preview"));
        }
        match self.deadline_s {
            Some(_) if self.op == PurgeOp::Invalidate => return Err("a revalidation deadline is for a delete".into()),
            Some(d) if d > MAX_DEADLINE_S => return Err(format!("a revalidation deadline of at most {MAX_DEADLINE_S} seconds (a year), not {d}")),
            Some(0) => self.deadline_s = None,
            _ => {}
        }
        Ok(())
    }

    /// The environments it applies to.
    pub fn environments(&self) -> &'static [&'static str] {
        match self.target.as_deref() {
            Some("production") => &ENVIRONMENTS[..1],
            Some("preview") => &ENVIRONMENTS[1..],
            _ => &ENVIRONMENTS,
        }
    }
}

/// A function's purge, as the runtime sends it to its router (after a `PurgeHello`) in one line of
/// JSON; the router answers with one line, `{"ok":true}` or `{"error":"…"}`. The router fills in
/// the project and environment from the worker, and the time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePurge {
    pub op: PurgeOp,
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub src_images: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_s: Option<u64>,
}

/// The router's answer to a `RuntimePurge`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeReply {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The longest line of a purge (128 tags of 1013 bytes, escaped, with room).
pub const MAX_PURGE_LINE: usize = 256 << 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_their_limits() {
        assert_eq!(split_tags(" a, b ,,c").collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(tag_bytes("ab"), 2);
        assert_eq!(tag_bytes("é"), 2);
        assert_eq!(tag_bytes("a\"b\\"), 6);
        assert!(tag_problem(&"x".repeat(256), false).is_none());
        assert!(tag_problem(&"x".repeat(257), false).is_some());
        assert!(tag_problem(&format!("{}\"", "x".repeat(255)), false).is_some(), "a quote counts twice");
        assert!(tag_problem("a,b", false).is_some());
        assert!(tag_problem("/img.png?a=1,2", true).is_none(), "a source image may have commas");
        assert_eq!((environment("preview"), environment(""), environment("production")), ("preview", "production", "production"));
    }

    #[test]
    fn purges_are_checked() {
        let purge = |op, tags: &[&str]| Purge { op, tags: tags.iter().map(|t| (*t).to_owned()).collect(), src_images: false, target: None, deadline_s: None, at_ms: 1, by: None };
        let mut p = purge(PurgeOp::Invalidate, &["a,b", "b", " c "]);
        p.check(16).unwrap();
        assert_eq!(p.tags, ["a", "b", "c"], "split, trimmed, once each");
        assert_eq!(p.environments(), ["production", "preview"]);
        assert!(purge(PurgeOp::Invalidate, &[]).check(16).is_err());
        assert!(purge(PurgeOp::Invalidate, &[" , "]).check(16).is_err());
        let many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
        assert!(purge(PurgeOp::Invalidate, &many.iter().map(String::as_str).collect::<Vec<_>>()).check(16).is_err());
        let mut all = purge(PurgeOp::Delete, &["*"]);
        all.target = Some("preview".into());
        all.deadline_s = Some(60);
        all.check(16).unwrap();
        assert_eq!(all.environments(), ["preview"]);
        let mut bad = purge(PurgeOp::Invalidate, &["a"]);
        bad.deadline_s = Some(5);
        assert!(bad.check(16).is_err(), "a deadline is a delete's");
        let mut bad = purge(PurgeOp::Delete, &["a"]);
        bad.target = Some("development".into());
        assert!(bad.check(16).is_err());
        let mut zero = purge(PurgeOp::Delete, &["a"]);
        zero.deadline_s = Some(0);
        zero.check(16).unwrap();
        assert_eq!(zero.deadline_s, None);
    }
}
