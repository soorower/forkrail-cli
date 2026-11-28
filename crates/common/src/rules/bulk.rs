//! Bulk redirects, as Vercel has them: up to a million plain path-to-URL redirects a project, read
//! from CSV, JSON or JSONL (`bulkRedirectsPath` at deploy, or a project's own set changed from the
//! dashboard or CLI), and looked up before any other rule.
//!
//! A set is stored as one binary table (`static/redirects/<sha256>`): a sorted index of the
//! sources' hashes, then the records. The router reads it into memory, or maps a large one from
//! disk, and answers a request with a binary search and one comparison: no parsing.
//!
//! Matching: a `source` is a path (`/old`) or a full URL (`https://old.example.com/old`), without
//! its query. Both it and the request's path are compared percent-decoded, and without regard to
//! case unless the redirect is `caseSensitive`. A source with a host matches only that host, and
//! is tried before one without.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::convert::percent_decode;
use super::engine::merge_query;

/// Vercel's limits: redirects a project, characters in a source or destination, and files in a
/// `bulkRedirectsPath` folder.
pub const MAX_REDIRECTS: usize = 1_000_000;
pub const MAX_URL: usize = 2048;
pub const MAX_FILES: usize = 100;

const MAGIC: &[u8; 8] = b"FRBULK01";
const HEADER: usize = 16;
const INDEX_ENTRY: usize = 16;
const CASE_SENSITIVE: u8 = 1;
const PRESERVE_QUERY: u8 = 2;
/// The most records with one hash a lookup reads.
const MAX_SAME_HASH: usize = 8;

/// One redirect, as written in a file or sent by the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkRedirect {
    pub source: String,
    pub destination: String,
    /// 301, 302, 303, 307 or 308.
    pub status_code: u16,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub preserve_query_params: bool,
}

impl BulkRedirect {
    /// A redirect from the fields Vercel accepts: `permanent` (308, else 307) unless `statusCode`
    /// says otherwise.
    pub fn new(source: &str, destination: &str, permanent: Option<bool>, status: Option<u16>, case_sensitive: bool, preserve_query: bool) -> Result<BulkRedirect, String> {
        let status_code = status.unwrap_or(if permanent == Some(true) { 308 } else { 307 });
        let r = BulkRedirect { source: source.to_owned(), destination: destination.to_owned(), status_code, case_sensitive, preserve_query_params: preserve_query };
        r.check()?;
        Ok(r)
    }

    pub fn check(&self) -> Result<(), String> {
        if ![301, 302, 303, 307, 308].contains(&self.status_code) {
            return Err(format!("status {} must be 301, 302, 303, 307 or 308", self.status_code));
        }
        if self.source.chars().count() > MAX_URL || self.destination.chars().count() > MAX_URL {
            return Err(format!("a source or destination is longer than {MAX_URL} characters"));
        }
        key(&self.source, self.case_sensitive)?;
        let d = &self.destination;
        let url = d.get(..8).map(str::to_ascii_lowercase).is_some_and(|p| p.starts_with("http://") || p.starts_with("https://"));
        if !(d.starts_with('/') || url) || d.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!("destination {d:?} must be a path starting with `/` or an http(s) URL, with no spaces"));
        }
        Ok(())
    }

    /// What a request matching it is told: where to go, with the client's query if it keeps it.
    pub fn location(&self, query: Option<&str>) -> String {
        if self.preserve_query_params { merge_query(&self.destination, query) } else { self.destination.clone() }
    }
}

/// The key a source is found by: `host/path` or `/path`, decoded, lowercase unless case-sensitive.
/// Two redirects with the same key and case sensitivity are the same redirect.
pub fn source_key(source: &str, case_sensitive: bool) -> Result<String, String> {
    key(source, case_sensitive)
}

fn key(source: &str, case_sensitive: bool) -> Result<String, String> {
    let lower = source.get(..8).map(str::to_ascii_lowercase).unwrap_or_default();
    let (host, path) = if let Some(rest) = ["https://", "http://"].iter().find_map(|p| lower.starts_with(p).then(|| &source[p.len()..])) {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let host = rest[..end].to_ascii_lowercase();
        let host = host.strip_suffix(":443").or_else(|| host.strip_suffix(":80")).unwrap_or(&host).to_owned();
        if host.is_empty() || host.contains('@') {
            return Err(format!("source {source:?} has no host"));
        }
        let path = &rest[end..];
        (Some(host), if path.starts_with('/') { path } else { "/" })
    } else if source.starts_with('/') {
        (None, source)
    } else {
        return Err(format!("source {source:?} must be a path starting with `/` or a full URL"));
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let path = if path.is_empty() { "/".to_owned() } else { percent_decode(path) };
    if path.chars().any(|c| c.is_control()) {
        return Err(format!("source {source:?} has control characters"));
    }
    let key = match host {
        Some(h) => format!("{h}{path}"),
        None => path,
    };
    Ok(if case_sensitive { key } else { key.to_lowercase() })
}

fn hash(bytes: &[u8]) -> u64 {
    // FNV-1a: fast for short keys, and the same on every machine.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Read redirects from a file's text, by its name's extension (`.csv`, `.json`, `.jsonl`).
pub fn parse(name: &str, text: &str) -> Result<Vec<BulkRedirect>, String> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".csv") {
        parse_csv(text).map_err(|e| format!("{name}: {e}"))
    } else if lower.ends_with(".jsonl") {
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line).map_err(|e| format!("{name}:{}: {e}", i + 1))?;
            out.push(from_json(&v).map_err(|e| format!("{name}:{}: {e}", i + 1))?);
        }
        Ok(out)
    } else if lower.ends_with(".json") {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("{name}: {e}"))?;
        let Some(list) = v.as_array() else { return Err(format!("{name}: must be an array of redirects")) };
        list.iter().enumerate().map(|(i, v)| from_json(v).map_err(|e| format!("{name}: item {i}: {e}"))).collect()
    } else {
        Err(format!("{name}: bulk redirect files are .csv, .json or .jsonl"))
    }
}

const FIELDS: [&str; 6] = ["source", "destination", "permanent", "statusCode", "caseSensitive", "preserveQueryParams"];

fn from_json(v: &Value) -> Result<BulkRedirect, String> {
    let Some(obj) = v.as_object() else { return Err("must be an object".into()) };
    if let Some(k) = obj.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(format!("unknown field `{k}`"));
    }
    let text = |k: &str| match obj.get(k) {
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        None | Some(Value::Null) => Ok(None),
        Some(_) => Err(format!("`{k}` must be text")),
    };
    let flag = |k: &str| match obj.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::String(s)) => boolean(s).map(Some).ok_or_else(|| format!("`{k}` must be true or false")),
        Some(_) => Err(format!("`{k}` must be true or false")),
    };
    let status = match obj.get("statusCode") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).and_then(|n| u16::try_from(n).ok()).ok_or("`statusCode` must be a number")?),
    };
    let source = text("source")?.ok_or("needs `source`")?;
    let destination = text("destination")?.ok_or("needs `destination`")?;
    BulkRedirect::new(source, destination, flag("permanent")?, status, flag("caseSensitive")?.unwrap_or(false), flag("preserveQueryParams")?.unwrap_or(false))
}

fn boolean(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "t" | "true" => Some(true),
        "f" | "false" | "" => Some(false),
        _ => None,
    }
}

/// CSV with a header naming the fields, in any order (RFC 4180 quoting).
fn parse_csv(text: &str) -> Result<Vec<BulkRedirect>, String> {
    let rows = csv_rows(text)?;
    let mut rows = rows.into_iter().filter(|(_, r)| !(r.len() == 1 && r[0].trim().is_empty()));
    let Some((_, header)) = rows.next() else { return Ok(Vec::new()) };
    let header: Vec<String> = header.iter().map(|h| h.trim().trim_start_matches('\u{feff}').to_owned()).collect();
    if let Some(h) = header.iter().find(|h| !FIELDS.contains(&h.as_str())) {
        return Err(format!("line 1: unknown column `{h}` (columns are {})", FIELDS.join(", ")));
    }
    for need in ["source", "destination"] {
        if !header.iter().any(|h| h == need) {
            return Err(format!("line 1: needs a `{need}` column"));
        }
    }
    let col = |name: &str| header.iter().position(|h| h == name);
    let mut out = Vec::new();
    for (line, row) in rows {
        let at = |name: &str| col(name).and_then(|i| row.get(i)).map(|s| s.trim()).filter(|s| !s.is_empty());
        let flag = |name: &str| at(name).map(|v| boolean(v).ok_or_else(|| format!("line {line}: `{name}` must be true or false"))).transpose();
        let status = at("statusCode").map(|v| v.parse::<u16>().map_err(|_| format!("line {line}: `statusCode` must be a number"))).transpose()?;
        let source = at("source").ok_or_else(|| format!("line {line}: needs a source"))?;
        let destination = at("destination").ok_or_else(|| format!("line {line}: needs a destination"))?;
        let r = BulkRedirect::new(source, destination, flag("permanent")?, status, flag("caseSensitive")?.unwrap_or(false), flag("preserveQueryParams")?.unwrap_or(false))
            .map_err(|e| format!("line {line}: {e}"))?;
        out.push(r);
    }
    Ok(out)
}

/// CSV records with the line each starts on.
fn csv_rows(text: &str) -> Result<Vec<(usize, Vec<String>)>, String> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut line = 1;
    let mut start = 1;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                '\n' => {
                    line += 1;
                    field.push(c);
                }
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.trim().is_empty() => {
                field.clear();
                quoted = true;
            }
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push((start, std::mem::take(&mut row)));
                line += 1;
                start = line;
            }
            _ => field.push(c),
        }
    }
    if quoted {
        return Err(format!("line {start}: a quoted field never ends"));
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push((start, row));
    }
    Ok(rows)
}

/// The table for a set of redirects. Two with the same source (as matched) are refused.
pub fn build(redirects: &[BulkRedirect]) -> Result<Vec<u8>, String> {
    if redirects.len() > MAX_REDIRECTS {
        return Err(format!("{} redirects; the limit is {MAX_REDIRECTS} a project", redirects.len()));
    }
    let mut keyed: Vec<(u64, String, &BulkRedirect)> = Vec::with_capacity(redirects.len());
    for r in redirects {
        r.check().map_err(|e| format!("{}: {e}", r.source))?;
        let k = key(&r.source, r.case_sensitive)?;
        keyed.push((hash(k.as_bytes()), k, r));
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.case_sensitive.cmp(&b.2.case_sensitive)));
    for pair in keyed.windows(2) {
        if pair[0].1 == pair[1].1 && pair[0].2.case_sensitive == pair[1].2.case_sensitive {
            return Err(format!("source {:?} is there twice (also as {:?})", pair[1].2.source, pair[0].2.source));
        }
    }
    let mut records = Vec::new();
    let mut index = Vec::with_capacity(keyed.len() * INDEX_ENTRY);
    for (h, k, r) in &keyed {
        let offset = u32::try_from(records.len()).map_err(|_| "the table is larger than 4 GB")?;
        let source = if *k == r.source { "" } else { r.source.as_str() };
        let flags = if r.case_sensitive { CASE_SENSITIVE } else { 0 } | if r.preserve_query_params { PRESERVE_QUERY } else { 0 };
        records.push(flags);
        records.push(0);
        records.extend_from_slice(&r.status_code.to_le_bytes());
        for part in [k.as_str(), source, r.destination.as_str()] {
            let len = u16::try_from(part.len()).map_err(|_| "a source or destination is too long")?;
            records.extend_from_slice(&len.to_le_bytes());
        }
        records.extend_from_slice(k.as_bytes());
        records.extend_from_slice(source.as_bytes());
        records.extend_from_slice(r.destination.as_bytes());
        index.extend_from_slice(&h.to_le_bytes());
        index.extend_from_slice(&offset.to_le_bytes());
        index.extend_from_slice(&0u32.to_le_bytes());
    }
    let mut out = Vec::with_capacity(HEADER + index.len() + records.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(keyed.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&index);
    out.extend_from_slice(&records);
    Ok(out)
}

/// A table, over its bytes in memory or mapped from a file.
pub struct Table<B> {
    bytes: B,
    count: usize,
}

/// One record, borrowed from the table.
pub struct Found<'a> {
    pub destination: &'a str,
    pub status: u16,
    pub preserve_query: bool,
}

impl Found<'_> {
    pub fn location(&self, query: Option<&str>) -> String {
        if self.preserve_query { merge_query(self.destination, query) } else { self.destination.to_owned() }
    }
}

impl<B: AsRef<[u8]>> Table<B> {
    /// Check the table's shape, so lookups needn't.
    pub fn new(bytes: B) -> Result<Table<B>, String> {
        let b = bytes.as_ref();
        if b.len() < HEADER || &b[..8] != MAGIC {
            return Err("not a bulk redirect table".into());
        }
        let count = u32::from_le_bytes(b[8..12].try_into().unwrap()) as usize;
        let records = HEADER + count * INDEX_ENTRY;
        if b.len() < records {
            return Err("the table is cut short".into());
        }
        let table = Table { bytes, count };
        for i in 0..count {
            if table.record(i).is_none() {
                return Err(format!("record {i} is damaged"));
            }
        }
        Ok(table)
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn hash_at(&self, i: usize) -> u64 {
        let at = HEADER + i * INDEX_ENTRY;
        u64::from_le_bytes(self.bytes.as_ref()[at..at + 8].try_into().unwrap())
    }

    /// Record `i`: (flags, status, key, source, destination).
    fn record(&self, i: usize) -> Option<(u8, u16, &str, &str, &str)> {
        let b = self.bytes.as_ref();
        let at = HEADER + i * INDEX_ENTRY + 8;
        let offset = u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize;
        let r = b.get(HEADER + self.count * INDEX_ENTRY + offset..)?;
        let flags = *r.first()?;
        let status = u16::from_le_bytes(r.get(2..4)?.try_into().ok()?);
        let len = |n: usize| r.get(4 + n * 2..6 + n * 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as usize);
        let (k, s, d) = (len(0)?, len(1)?, len(2)?);
        let text = |from: usize, n: usize| r.get(10 + from..10 + from + n).and_then(|t| std::str::from_utf8(t).ok());
        Some((flags, status, text(0, k)?, text(k, s)?, text(k + s, d)?))
    }

    /// The record with `key` and the given case sensitivity.
    fn find(&self, key: &str, case_sensitive: bool) -> Option<Found<'_>> {
        let h = hash(key.as_bytes());
        let mut lo = 0;
        let mut hi = self.count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.hash_at(mid) < h { lo = mid + 1 } else { hi = mid }
        }
        // Records with one key's hash: its case-sensitive and -insensitive ones, and a 64-bit
        // collision now and then. A table a build wrote could hold millions; a few are looked at.
        let mut i = lo;
        while i < self.count.min(lo + MAX_SAME_HASH) && self.hash_at(i) == h {
            if let Some((flags, status, k, _, dest)) = self.record(i)
                && k == key
                && (flags & CASE_SENSITIVE != 0) == case_sensitive
            {
                return Some(Found { destination: dest, status, preserve_query: flags & PRESERVE_QUERY != 0 });
            }
            i += 1;
        }
        None
    }

    /// The redirect for a request to `host` (lowercase, no port) and `path` (as sent, encoded),
    /// if there is one.
    pub fn lookup(&self, host: &str, path: &str) -> Option<Found<'_>> {
        if self.count == 0 {
            return None;
        }
        let decoded = percent_decode(path);
        let with_host = format!("{host}{decoded}");
        for k in [with_host.as_str(), decoded.as_str()] {
            if let Some(found) = self.find(k, true) {
                return Some(found);
            }
            let lower = k.to_lowercase();
            if let Some(found) = self.find(&lower, false) {
                return Some(found);
            }
        }
        None
    }

    /// Every redirect, in the table's order.
    pub fn entries(&self) -> impl Iterator<Item = BulkRedirect> + '_ {
        (0..self.count).filter_map(|i| {
            let (flags, status, key, source, dest) = self.record(i)?;
            Some(BulkRedirect {
                source: if source.is_empty() { key.to_owned() } else { source.to_owned() },
                destination: dest.to_owned(),
                status_code: status,
                case_sensitive: flags & CASE_SENSITIVE != 0,
                preserve_query_params: flags & PRESERVE_QUERY != 0,
            })
        })
    }
}

/// Redirects as CSV, with every column, for export.
pub fn to_csv(redirects: impl Iterator<Item = BulkRedirect>) -> String {
    let quote = |s: &str| if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_owned() };
    let mut out = String::from("source,destination,statusCode,caseSensitive,preserveQueryParams\n");
    for r in redirects {
        out.push_str(&format!("{},{},{},{},{}\n", quote(&r.source), quote(&r.destination), r.status_code, r.case_sensitive, r.preserve_query_params));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_in_every_format() {
        let csv = "source,destination,permanent\n/source/path,/destination/path,true\n\"/a,b\",https://x.com/y,f\nhttps://Old-Domain.com/page,/new-page,t\n";
        let got = parse("r.csv", csv).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].status_code, 308);
        assert_eq!(got[1].source, "/a,b");
        assert_eq!(got[1].status_code, 307);
        let json = r#"[{"source": "/a", "destination": "/b", "statusCode": 301, "caseSensitive": true, "preserveQueryParams": true}]"#;
        assert_eq!(parse("r.json", json).unwrap()[0], BulkRedirect::new("/a", "/b", None, Some(301), true, true).unwrap());
        let jsonl = "{\"source\": \"/a\", \"destination\": \"/b\", \"permanent\": true}\n\n{\"source\": \"/c\", \"destination\": \"/d\"}\n";
        assert_eq!(parse("r.jsonl", jsonl).unwrap().len(), 2);
        assert!(parse("r.csv", "source,dest\n/a,/b").unwrap_err().contains("unknown column"));
        assert!(parse("r.csv", "source,destination,statusCode\n/a,/b,200").unwrap_err().contains("line 2"));
        assert!(parse("r.json", r#"[{"source": "a", "destination": "/b"}]"#).unwrap_err().contains("item 0"));
        assert!(parse("r.txt", "").is_err());
    }

    #[test]
    fn lookups() {
        let list = vec![
            BulkRedirect::new("/Old", "/new", None, None, false, false).unwrap(),
            BulkRedirect::new("/Exact", "/e", Some(true), None, true, false).unwrap(),
            BulkRedirect::new("/keep", "/kept?b=2", None, Some(301), false, true).unwrap(),
            BulkRedirect::new("https://old.example.com/page", "https://new.example.com/", None, None, false, false).unwrap(),
            BulkRedirect::new("/page", "/elsewhere", None, None, false, false).unwrap(),
            BulkRedirect::new("/caf%C3%A9", "/coffee", None, None, false, false).unwrap(),
        ];
        let table = Table::new(build(&list).unwrap()).unwrap();
        assert_eq!(table.len(), 6);
        let go = |host: &str, path: &str, q: Option<&str>| table.lookup(host, path).map(|f| (f.location(q), f.status));
        assert_eq!(go("a.com", "/old", Some("x=1")), Some(("/new".into(), 307)));
        assert_eq!(go("a.com", "/OLD", None), Some(("/new".into(), 307)));
        assert_eq!(go("a.com", "/Exact", None), Some(("/e".into(), 308)));
        assert_eq!(go("a.com", "/exact", None), None);
        assert_eq!(go("a.com", "/keep", Some("a=1&b=1")), Some(("/kept?a=1&b=2".into(), 301)));
        assert_eq!(go("old.example.com", "/page", None), Some(("https://new.example.com/".into(), 307)));
        assert_eq!(go("other.com", "/page", None), Some(("/elsewhere".into(), 307)));
        assert_eq!(go("a.com", "/café", None), Some(("/coffee".into(), 307)));
        assert_eq!(go("a.com", "/CAF%C3%A9", None), Some(("/coffee".into(), 307)));
        assert_eq!(go("a.com", "/old/", None), None);
        let back: Vec<BulkRedirect> = table.entries().collect();
        assert_eq!(back.len(), 6);
        assert!(back.contains(&list[3]));
        assert!(to_csv(back.into_iter()).starts_with("source,destination,statusCode"));
    }

    #[test]
    fn mistakes_are_refused() {
        let twice = vec![BulkRedirect::new("/a", "/b", None, None, false, false).unwrap(), BulkRedirect::new("/A", "/c", None, None, false, false).unwrap()];
        assert!(build(&twice).unwrap_err().contains("twice"));
        // The same path once case-sensitive and once not is two redirects.
        let both = vec![BulkRedirect::new("/a", "/b", None, None, true, false).unwrap(), BulkRedirect::new("/a", "/c", None, None, false, false).unwrap()];
        assert!(build(&both).is_ok());
        assert!(BulkRedirect::new("/a", "b", None, None, false, false).is_err());
        assert!(BulkRedirect::new("/a", "/b", None, Some(200), false, false).is_err());
        assert!(Table::new(b"nonsense".to_vec()).is_err());
    }

    /// `cargo test --release -p forkrail-common --features rules -- --ignored million`
    #[test]
    #[ignore]
    fn a_million_redirects() {
        let list: Vec<BulkRedirect> = (0..MAX_REDIRECTS)
            .map(|i| BulkRedirect::new(&format!("/products/item-{i}/details"), &format!("https://shop.example.com/p/{i}"), Some(true), None, false, false).unwrap())
            .collect();
        let started = std::time::Instant::now();
        let bytes = build(&list).unwrap();
        println!("built {} MB in {:?}", bytes.len() >> 20, started.elapsed());
        let table = Table::new(bytes).unwrap();
        let started = std::time::Instant::now();
        let n = 200_000;
        let mut hits = 0;
        for i in 0..n {
            let path = if i % 2 == 0 { format!("/products/item-{}/details", i * 7 % MAX_REDIRECTS) } else { format!("/missing/{i}") };
            hits += usize::from(table.lookup("shop.example.com", &path).is_some());
        }
        let each = started.elapsed() / n as u32;
        println!("{n} lookups, {hits} hits: {each:?} each");
        assert_eq!(hits, n / 2);
        assert!(each < std::time::Duration::from_micros(100));
    }
}
