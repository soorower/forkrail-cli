//! Next's client-reference manifests, written once instead of thousands of times.
//!
//! Next writes a `page_client-reference-manifest.js` beside every App Router page's server code:
//! `globalThis.__RSC_MANIFEST["/page"] = {…}`, which its server evaluates before rendering the
//! page. Under Turbopack every client module's entry carries the whole `chunks` list of its chunk
//! group, so a big app writes the same few lists thousands of times: in shadcn/ui's docs site
//! (Next 16.4.5) six of these files are 318 MB each, each the same 5 lists of 1,581 paths written
//! 2,410 times; the 32 files are 2.0 GB of a 3.2 GB server, and loading one costs Next 0.9 s and
//! 1.3 GB of memory (docs/data/n0/large-apps.md).
//!
//! `compact_into` writes each such file again with every distinct `chunks` list once, in an array
//! the entries point into: `(function(){var c=[[…],[…]];…"chunks":c[0]…})();`. Evaluated, it
//! assigns the same values (equal lists are the same array instead of equal copies; React's
//! Flight client only reads them). The 32 files above become 5.6 MB, the server 220 MB, and a big
//! page's first render needs a quarter of the memory.
//!
//! It is careful by construction: a file is rewritten only when it has exactly the shape Next
//! writes (assignments to `globalThis.__RSC_MANIFEST`), only `"chunks"` keys whose value is an
//! array of string literals are touched, and the rewrite is expanded back and must give the
//! original text byte for byte. Anything else, and a file the rewrite wouldn't make smaller, is
//! left as it is. The build's own files are never changed: the rewrites go to a folder of the
//! caller's, and the caller packages them in place of the originals.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The file name Next gives these manifests.
const SUFFIX: &str = "client-reference-manifest.js";
/// Smaller files aren't worth a look.
const MIN_BYTES: u64 = 64 * 1024;
/// What every one of them starts with (Next 13.4 to 16, Turbopack and webpack alike).
const GLOBAL: &str = "globalThis.__RSC_MANIFEST";
const OPEN: &str = "(function(){var c=[";
const CLOSE: &str = "\n})();\n";

/// What `compact_into` did.
#[derive(Debug, Default)]
pub struct Compacted {
    /// Manifests looked at, and their bytes before and after (unchanged ones count as they are).
    pub files: usize,
    pub before: u64,
    pub after: u64,
    /// Each rewritten file, by its canonical path, to its rewrite.
    pub replaced: BTreeMap<PathBuf, PathBuf>,
}

impl Compacted {
    /// A line for the build log, or None when there was nothing to rewrite.
    pub fn describe(&self) -> Option<String> {
        if self.replaced.is_empty() {
            return None;
        }
        let mb = |b: u64| b as f64 / 1e6;
        Some(format!(
            "client-reference manifests: {} files, {:.1} MB written as {:.1} MB ({} rewritten with their repeated chunk lists shared)",
            self.files,
            mb(self.before),
            mb(self.after),
            self.replaced.len()
        ))
    }
}

/// Find Next's client-reference manifests under `root` (not in `node_modules`) and write the ones
/// that shrink to `into`, at their paths relative to `root`. Nothing under `root` changes.
pub fn compact_into(root: &Path, into: &Path) -> Result<Compacted> {
    let root = root.canonicalize().with_context(|| format!("reading {}", root.display()))?;
    let mut found = Vec::new();
    find(&root, &mut found)?;
    found.sort();
    let mut done = Compacted::default();
    for path in found {
        let size = std::fs::metadata(&path)?.len();
        done.files += 1;
        done.before += size;
        let rewritten = if size >= MIN_BYTES {
            let text = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            std::str::from_utf8(&text).ok().and_then(compact_text)
        } else {
            None
        };
        let Some(rewritten) = rewritten else {
            done.after += size;
            continue;
        };
        let out = into.join(path.strip_prefix(&root).unwrap_or(&path));
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&out, &rewritten).with_context(|| format!("writing {}", out.display()))?;
        done.after += rewritten.len() as u64;
        done.replaced.insert(path, out);
    }
    Ok(done)
}

/// Every manifest file under `dir`, links not followed, `node_modules` left out.
fn find(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        if kind.is_dir() && name != "node_modules" {
            find(&entry.path(), out)?;
        } else if kind.is_file() && name.to_str().is_some_and(|n| n.ends_with(SUFFIX)) {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// The manifest `text` with its repeated `chunks` lists shared, if it has Next's shape, the
/// rewrite expands back to it exactly, and it is smaller.
pub fn compact_text(text: &str) -> Option<String> {
    if !text.trim_start().starts_with(GLOBAL) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut pool: Vec<&str> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut body = String::with_capacity(text.len() / 8);
    let mut copied = 0; // `text[..copied]` is in `body` already
    let mut i = 0;
    let mut last_code = 0u8; // the last byte outside a string that isn't whitespace
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' | b'`' => {
                let end = string_end(bytes, i)?;
                let is_key = bytes[i] == b'"' && &text[i..end] == "\"chunks\"" && matches!(last_code, b'{' | b',');
                if is_key && let Some((colon_end, array_end)) = chunks_value(bytes, end) {
                    let array = &text[colon_end..array_end];
                    let n = *index.entry(array).or_insert_with(|| {
                        pool.push(array);
                        pool.len() - 1
                    });
                    body.push_str(&text[copied..colon_end]);
                    body.push_str(&format!("c[{n}]"));
                    copied = array_end;
                    i = array_end;
                    last_code = b']';
                    continue;
                }
                last_code = b'"';
                i = end;
            }
            b'/' if matches!(bytes.get(i + 1), Some(b'/' | b'*')) => return None, // comments: not Next's shape
            b if b.is_ascii_whitespace() => i += 1,
            b => {
                last_code = b;
                i += 1;
            }
        }
    }
    if pool.is_empty() {
        return None;
    }
    body.push_str(&text[copied..]);
    let mut out = String::with_capacity(body.len() + pool.iter().map(|p| p.len() + 1).sum::<usize>() + 32);
    out.push_str(OPEN);
    out.push_str(&pool.join(","));
    out.push_str("];\n");
    out.push_str(&body);
    out.push_str(CLOSE);
    (out.len() < text.len() && expand(&out).as_deref() == Some(text)).then_some(out)
}

/// The end (one past the closing quote) of the string literal starting at `start`.
fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return Some(i + 1),
            b'$' if quote == b'`' && bytes.get(i + 1) == Some(&b'{') => return None, // template code: not Next's shape
            _ => i += 1,
        }
    }
    None
}

/// After a `"chunks"` key ending at `key_end`: where its value starts (after the colon and any
/// whitespace) and ends, if the value is an array of string literals.
fn chunks_value(bytes: &[u8], key_end: usize) -> Option<(usize, usize)> {
    let mut i = skip_space(bytes, key_end);
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i = skip_space(bytes, i + 1);
    let start = i;
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let mut want_item = true;
    loop {
        i = skip_space(bytes, i);
        match bytes.get(i)? {
            b']' => return Some((start, i + 1)),
            b'"' if want_item => {
                i = string_end(bytes, i)?;
                want_item = false;
            }
            b',' if !want_item => {
                i += 1;
                want_item = true;
            }
            _ => return None,
        }
    }
}

fn skip_space(bytes: &[u8], mut i: usize) -> usize {
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// `compact_text`'s output back to the text it was made from, read independently of how it was
/// made: the pool's arrays, then every `c[N]` outside a string replaced by array N.
fn expand(out: &str) -> Option<String> {
    let body_with_close = out.strip_prefix(OPEN)?;
    let bytes = body_with_close.as_bytes();
    let mut pool = Vec::new();
    let mut i = 0;
    loop {
        let (start, end) = array_at(bytes, i)?;
        pool.push(&body_with_close[start..end]);
        i = end;
        match bytes.get(i)? {
            b',' => i += 1,
            b']' if bytes.get(i + 1) == Some(&b';') && bytes.get(i + 2) == Some(&b'\n') => {
                i += 3;
                break;
            }
            _ => return None,
        }
    }
    let body = body_with_close[i..].strip_suffix(CLOSE)?;
    let b = body.as_bytes();
    let mut text = String::with_capacity(body.len() * 8);
    let (mut copied, mut j) = (0, 0);
    while j < b.len() {
        match b[j] {
            b'"' | b'\'' | b'`' => j = string_end(b, j)?,
            b'c' if b.get(j + 1) == Some(&b'[') && (j == 0 || !is_ident(b[j - 1])) => {
                let digits = b[j + 2..].iter().take_while(|d| d.is_ascii_digit()).count();
                if digits == 0 || b.get(j + 2 + digits) != Some(&b']') {
                    return None;
                }
                let n: usize = body[j + 2..j + 2 + digits].parse().ok()?;
                text.push_str(&body[copied..j]);
                text.push_str(pool.get(n)?);
                j += 3 + digits;
                copied = j;
            }
            _ => j += 1,
        }
    }
    text.push_str(&body[copied..]);
    Some(text)
}

/// The array of string literals starting at `i`: its start and end.
fn array_at(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    if bytes.get(i) != Some(&b'[') {
        return None;
    }
    let mut j = i + 1;
    let mut want_item = true;
    loop {
        j = skip_space(bytes, j);
        match bytes.get(j)? {
            b']' => return Some((i, j + 1)),
            b'"' if want_item => {
                j = string_end(bytes, j)?;
                want_item = false;
            }
            b',' if !want_item => {
                j += 1;
                want_item = true;
            }
            _ => return None,
        }
    }
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(lists: &[&str], times: usize) -> String {
        let mut entries = Vec::new();
        for n in 0..times {
            let list = lists[n % lists.len()];
            entries.push(format!(r#""[project]/m{n}.js":{{"id":{n},"name":"*","chunks":{list},"async":false}}"#));
        }
        format!(
            "globalThis.__RSC_MANIFEST = globalThis.__RSC_MANIFEST || {{}};\nglobalThis.__RSC_MANIFEST[\"/page\"] = {{\"moduleLoading\":{{\"prefix\":\"\"}},\"clientModules\":{{{}}},\"ssrModuleMapping\":{{}},\"entryCSSFiles\":{{\"x\":[{{\"path\":\"a.css\",\"inlined\":false}}]}}}};\n",
            entries.join(",")
        )
    }

    /// A list of `n` chunk paths, as Turbopack writes them.
    fn list(n: usize, tag: &str) -> String {
        format!("[{}]", (0..n).map(|i| format!("\"server/chunks/ssr/{tag}_{i:04}._.js\"")).collect::<Vec<_>>().join(","))
    }

    #[test]
    fn repeated_lists_are_written_once_and_expand_back() {
        let long = list(60, "a");
        let a = format!(r#"["server/chunks/c\"quoted\".js",{}"#, &long[1..]);
        let b = r#"[ "x.js" , "y.js" ]"#;
        let text = manifest(&[&a, b], 200);
        let out = compact_text(&text).expect("it shrinks");
        assert!(out.len() < text.len() / 3, "{} -> {}", text.len(), out.len());
        assert_eq!(out.matches("server/chunks/ssr/a_0007._.js").count(), 1);
        assert!(out.starts_with(OPEN) && out.ends_with(CLOSE));
        assert_eq!(expand(&out).as_deref(), Some(text.as_str()));
        // Empty lists beside long ones are shared too; a file of empty lists alone wouldn't shrink.
        let mixed = manifest(&[&a, "[]"], 50);
        assert_eq!(expand(&compact_text(&mixed).unwrap()).as_deref(), Some(mixed.as_str()));
        assert!(compact_text(&manifest(&["[]"], 50)).is_none());
    }

    #[test]
    fn anything_but_next_s_shape_is_left_alone() {
        let a = r#"["a.js","b.js","c.js","d.js"]"#;
        let good = manifest(&[a], 100);
        // Not starting with the global.
        assert!(compact_text(&format!("var x = 1;\n{good}")).is_none());
        // A comment, a template literal with code.
        assert!(compact_text(&format!("{good}// done\n")).is_none());
        assert!(compact_text(&good.replace("\"moduleLoading\"", "`${x}`:1,\"moduleLoading\"")).is_none());
        // A list holding something other than string literals stays as it is.
        let odd = manifest(&[r#"["a.js",x,"b.js"]"#], 100);
        assert!(compact_text(&odd).is_none());
        // `"chunks"` that is a value, not a key, isn't touched.
        let value = good.replacen("\"name\":\"*\"", "\"name\":\"chunks\"", 1);
        let out = compact_text(&value).unwrap();
        assert_eq!(expand(&out).as_deref(), Some(value.as_str()));
        // Nothing to share: nothing written.
        assert!(compact_text("globalThis.__RSC_MANIFEST = globalThis.__RSC_MANIFEST || {};\n").is_none());
    }

    #[test]
    fn rewrites_go_beside_the_build_never_into_it() {
        let dir = crate::package::TempDir::new("rsc-test").unwrap();
        let root = dir.path().join("standalone");
        let page = root.join("apps/web/.next/server/app/docs");
        std::fs::create_dir_all(&page).unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        let big = manifest(&[&list(100, "p")], 2000);
        std::fs::write(page.join("page_client-reference-manifest.js"), &big).unwrap();
        std::fs::write(page.join("small_client-reference-manifest.js"), "globalThis.__RSC_MANIFEST = {};").unwrap();
        std::fs::write(root.join("node_modules/x/page_client-reference-manifest.js"), &big).unwrap();
        let into = dir.path().join("rewrites");
        let done = compact_into(&root, &into).unwrap();
        assert_eq!((done.files, done.replaced.len()), (2, 1));
        let (original, rewrite) = done.replaced.iter().next().unwrap();
        assert!(original.ends_with("apps/web/.next/server/app/docs/page_client-reference-manifest.js"));
        assert_eq!(rewrite, &into.join("apps/web/.next/server/app/docs/page_client-reference-manifest.js"));
        assert_eq!(std::fs::read_to_string(page.join("page_client-reference-manifest.js")).unwrap(), big, "the build's file is untouched");
        assert!(done.after < done.before / 10);
        assert!(done.describe().unwrap().contains("2 files"));
    }
}

/// By hand, on a real build: `FORKRAIL_RSC_DIR=<standalone> FORKRAIL_RSC_OUT=<folder> cargo test -p
/// forkrail-cli --lib rsc_manifests::real -- --ignored --nocapture`
#[cfg(test)]
mod real {
    #[test]
    #[ignore]
    fn real() {
        let (Some(dir), Some(out)) = (std::env::var_os("FORKRAIL_RSC_DIR"), std::env::var_os("FORKRAIL_RSC_OUT")) else { return };
        let started = std::time::Instant::now();
        let done = super::compact_into(std::path::Path::new(&dir), std::path::Path::new(&out)).unwrap();
        println!("{} in {:.1} s", done.describe().unwrap_or_default(), started.elapsed().as_secs_f64());
        for (from, to) in &done.replaced {
            println!("{}\t{}", from.display(), to.display());
        }
    }
}
