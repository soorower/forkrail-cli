//! `.env` files for `forkrail env pull`, and the environment `forkrail env run` gives a command.
//!
//! A pull writes each readable variable as `KEY="value"`, with newlines as `\n`, the way the
//! files frameworks read (`next dev`, dotenv) are usually written. When the file is already there
//! it's merged, not replaced: entries only in the file are kept as they are written, and a variable
//! whose value can't be read back (a production or preview one) keeps the file's own value, or gets
//! an empty placeholder to fill in.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::deploy::log;
use crate::env::Pulled;

/// One assignment in a `.env` file, as written there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    /// Its whole text: more than one line when a quoted value spans lines.
    pub raw: String,
    /// Whether its value is empty (`KEY=`, `KEY=""`).
    pub empty: bool,
}

/// A `.env` file's assignments, in order (`export` allowed, quoted values may span lines);
/// comments and lines that aren't assignments are left out.
pub fn entries(text: &str) -> Vec<Entry> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let rest = line.trim_start();
        let rest = rest.strip_prefix("export ").map(str::trim_start).unwrap_or(rest);
        let key_len = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')).unwrap_or(rest.len());
        let key = &rest[..key_len];
        let Some(value) = rest[key_len..].trim_start().strip_prefix('=') else { continue };
        if key.is_empty() || key.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let value = value.trim_start();
        let (raw, empty) = match value.chars().next() {
            Some(quote @ ('"' | '\'' | '`')) => {
                // The closing quote, maybe on a later line; a backslash escapes it except in '…'.
                let mut text = value[1..].to_owned();
                let mut taken = 0;
                let close = loop {
                    if let Some(at) = closing(&text, quote) {
                        break Some(at);
                    }
                    match lines.get(i + taken) {
                        Some(next) => {
                            text.push('\n');
                            text.push_str(next);
                            taken += 1;
                        }
                        None => break None,
                    }
                };
                match close {
                    Some(at) => {
                        let raw = std::iter::once(line).chain(lines[i..i + taken].iter().copied()).collect::<Vec<_>>().join("\n");
                        i += taken;
                        (raw, at == 0)
                    }
                    // Never closed: the line alone, as written.
                    None => (line.to_owned(), value.len() == 1),
                }
            }
            _ => (line.to_owned(), value.split('#').next().unwrap_or("").trim().is_empty()),
        };
        out.push(Entry { key: key.to_owned(), raw, empty });
    }
    out
}

/// Where `quote` closes `text` (which follows the opening one), if it does.
fn closing(text: &str, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        if c == quote && !escaped {
            return Some(at);
        }
        escaped = quote != '\'' && c == '\\' && !escaped;
    }
    None
}

/// `KEY="value"`, with newlines and carriage returns as `\n` and `\r`.
fn line(key: &str, value: &str) -> String {
    format!("{key}=\"{}\"", value.replace('\n', "\\n").replace('\r', "\\r"))
}

/// What a pull writes into a file holding `existing` (none if there's no file yet).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Merged {
    pub text: String,
    /// Entries only in the file, kept.
    pub kept: Vec<String>,
    /// Secrets the file gave a value, kept.
    pub kept_secrets: Vec<String>,
    /// Secrets written empty, to fill in.
    pub placeholders: Vec<String>,
}

pub fn merge(existing: Option<&str>, pulled: &Pulled) -> Merged {
    let target = pulled.label();
    let local = existing.map(entries).unwrap_or_default();
    // The last assignment of a name is the one that counts, as in every reader of these files.
    let last: BTreeMap<&str, &Entry> = local.iter().map(|e| (e.key.as_str(), e)).collect();
    let secret: BTreeSet<&str> = pulled.secret.iter().map(String::as_str).filter(|k| !pulled.values.contains_key(*k)).collect();
    let mut merged = Merged::default();
    let mut text = format!("# Created by forkrail env pull: {}'s {target} variables. Pull again after a change.\n", pulled.project);
    let names: BTreeSet<&str> = pulled.values.keys().map(String::as_str).chain(secret.iter().copied()).collect();
    for name in &names {
        if let Some(value) = pulled.values.get(*name) {
            text.push_str(&line(name, value));
        } else if let Some(entry) = last.get(name).filter(|e| !e.empty) {
            text.push_str(&entry.raw);
            merged.kept_secrets.push(name.to_string());
        } else {
            text.push_str(&format!("# {name} is a secret in {target}: its value can't be read back, so give it one here\n{name}=\"\""));
            merged.placeholders.push(name.to_string());
        }
        text.push('\n');
    }
    let mut seen = BTreeSet::new();
    let others: Vec<&Entry> = local.iter().rev().filter(|e| !names.contains(e.key.as_str()) && seen.insert(e.key.as_str())).collect();
    if !others.is_empty() {
        text.push_str(&format!("\n# Kept from this file: not set in {target}\n"));
        for entry in others.into_iter().rev() {
            text.push_str(&entry.raw);
            text.push('\n');
            merged.kept.push(entry.key.clone());
        }
    }
    merged.text = text;
    merged
}

/// Write what was pulled into `path`, merged with what's there; a file that's there is changed
/// only after a yes (`yes`, or one typed on a terminal). A new file is readable by its owner only.
pub fn write_pulled(pulled: &Pulled, path: &Path, yes: bool) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if existing.is_some() && !yes {
        if !std::io::stdin().is_terminal() {
            bail!("{} is there already: add --yes to update it (variables only in it are kept)", path.display());
        }
        eprint!("{} is there already: update it? Variables only in it are kept. [y/N] ", path.display());
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            bail!("left {} as it was", path.display());
        }
    }
    let merged = merge(existing.as_deref(), pulled);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("writing {}", path.display()))?;
    file.write_all(merged.text.as_bytes()).with_context(|| format!("writing {}", path.display()))?;
    let count = pulled.values.len();
    log(&format!(
        "{} {}: {count} {} variable{} from {}",
        if existing.is_some() { "updated" } else { "created" },
        path.display(),
        pulled.label(),
        if count == 1 { "" } else { "s" },
        pulled.project
    ));
    if !merged.kept.is_empty() {
        log(&format!("kept {} (in the file, not set in {})", merged.kept.join(", "), pulled.label()));
    }
    if !merged.kept_secrets.is_empty() {
        log(&format!("kept the file's own values for {}: secrets, which can't be read back", merged.kept_secrets.join(", ")));
    }
    if !merged.placeholders.is_empty() {
        log(&format!("{} {} secret{}: fill {} in", merged.placeholders.join(", "), if merged.placeholders.len() == 1 { "is a" } else { "are" }, if merged.placeholders.len() == 1 { "" } else { "s" }, if merged.placeholders.len() == 1 { "it" } else { "them" }));
    }
    if git_ignores(path) == Some(false) {
        log(&format!("note: git doesn't ignore {}: keep it out of the repository (add it to .gitignore)", path.display()));
    }
    Ok(())
}

/// Whether git ignores `path`; `None` outside a repository or without git.
fn git_ignores(path: &Path) -> Option<bool> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name()?;
    let status = std::process::Command::new("git").arg("-C").arg(dir).args(["check-ignore", "-q", "--"]).arg(name).stderr(std::process::Stdio::null()).status().ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// Run `command` with what was pulled added to this process's environment; a variable already set
/// here keeps its value, as with a `.env` file. Answers the command's exit code.
pub fn run(pulled: &Pulled, command: &[String]) -> Result<i32> {
    let Some((program, args)) = command.split_first() else { bail!("no command: `forkrail env run -- next dev`") };
    let unset: Vec<&str> = pulled.secret.iter().map(String::as_str).filter(|k| !pulled.values.contains_key(*k) && std::env::var_os(k).is_none()).collect();
    if !unset.is_empty() {
        log(&format!("{} {} in {}: secret, so not passed on", unset.join(", "), if unset.len() == 1 { "is set" } else { "are set" }, pulled.label()));
    }
    let mut child = std::process::Command::new(program);
    child.args(args);
    for (key, value) in &pulled.values {
        if std::env::var_os(key).is_none() {
            child.env(key, value);
        }
    }
    let status = child.status().with_context(|| format!("running {program}"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::EnvTarget;

    fn pulled(target: EnvTarget, values: &[(&str, &str)], secret: &[&str]) -> Pulled {
        Pulled {
            project: "shop".into(),
            target,
            branch: None,
            values: values.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            secret: secret.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_files_assignments_are_read_as_written() {
        let text = "# a comment\nexport A=1\nB = \"two words\" # why\nC='it''s'\nD=\"line one\nline two\"\nE=\nF=\"\"\nnot an assignment\n9X=1\nG.H-I=x\nA=again\n";
        let found = entries(text);
        let keys: Vec<&str> = found.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["A", "B", "C", "D", "E", "F", "G.H-I", "A"]);
        assert_eq!(found[3].raw, "D=\"line one\nline two\"");
        assert!(found[4].empty && found[5].empty && !found[0].empty && !found[3].empty);
        // An escaped quote doesn't close a value; a value never closed is its line alone.
        let found = entries("A=\"say \\\"hi\\\"\"\nB=\"open\nC=3\n");
        assert_eq!(found[0].raw, "A=\"say \\\"hi\\\"\"");
        assert_eq!(found[1].raw, "B=\"open");
        assert_eq!(found[2].key, "C");
    }

    #[test]
    fn a_pull_writes_values_and_placeholders() {
        let p = pulled(EnvTarget::Development, &[("API_URL", "https://x.test"), ("PEM", "a\nb")], &[]);
        let m = merge(None, &p);
        assert!(m.text.starts_with("# Created by forkrail env pull: shop's development variables"));
        assert!(m.text.contains("API_URL=\"https://x.test\"\nPEM=\"a\\nb\"\n"), "{}", m.text);
        assert!(m.kept.is_empty() && m.placeholders.is_empty());

        let p = pulled(EnvTarget::Production, &[], &["STRIPE_KEY"]);
        let m = merge(None, &p);
        assert!(m.text.contains("# STRIPE_KEY is a secret in production"), "{}", m.text);
        assert!(m.text.contains("\nSTRIPE_KEY=\"\"\n"));
        assert_eq!(m.placeholders, ["STRIPE_KEY"]);
    }

    #[test]
    fn a_pull_merges_into_the_file_there() {
        let existing = "LOCAL_ONLY=keep me\nAPI_URL=old\nSTRIPE_KEY=sk_test_local\nEMPTY_SECRET=\nNOTES=\"two\nlines\"\nLOCAL_ONLY=keep me too\n";
        let p = pulled(EnvTarget::Preview, &[("API_URL", "new")], &["STRIPE_KEY", "EMPTY_SECRET"]);
        let m = merge(Some(existing), &p);
        assert!(m.text.contains("API_URL=\"new\"\n") && !m.text.contains("API_URL=old"), "{}", m.text);
        assert!(m.text.contains("\nSTRIPE_KEY=sk_test_local\n"));
        assert_eq!(m.kept_secrets, ["STRIPE_KEY"]);
        assert_eq!(m.placeholders, ["EMPTY_SECRET"]);
        // Kept once each, the last assignment, in the file's order.
        assert_eq!(m.kept, ["NOTES", "LOCAL_ONLY"]);
        assert!(m.text.ends_with("# Kept from this file: not set in preview\nNOTES=\"two\nlines\"\nLOCAL_ONLY=keep me too\n"), "{}", m.text);
        // Pulling again into what was written changes nothing.
        let again = merge(Some(&m.text), &p);
        assert_eq!(again.text, m.text);
        assert_eq!(again.kept, m.kept);
    }
}
