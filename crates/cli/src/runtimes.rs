//! Which version of its language a project runs on, read from its files the way Vercel reads them,
//! and the managed Lambda runtime that runs it.
//!
//! - **Python** (Vercel's `packages/python`, `python-analysis`): `.python-version` (the nearest,
//!   from the entry's folder up to the project's top) wins over `pyproject.toml`'s
//!   `requires-python`, then `Pipfile.lock`'s `python_version`. Vercel offers 3.12, 3.13 and 3.14,
//!   and tries its default first, then the newest. forkrail's default is 3.13, what every project
//!   ran before versions were read, so a project that names none never moves: `>=3.12` is 3.13,
//!   `<3.13` is 3.12. A version that isn't offered, or one that can't be read, gets the default and
//!   a warning. Lambda runs all three (`python3.12`, `python3.13`, `python3.14`, on Amazon Linux
//!   2023).
//! - **Node** (Vercel's `build-utils` `node-version.ts`): `engines.node` in `package.json`, a semver
//!   range; the newest major the range takes wins. Lambda runs `nodejs24.x` and `nodejs22.x`
//!   (`nodejs26.x`, Vercel's default since October 2026, is still a preview on Lambda, so 24 is
//!   the default here). A range that takes none of them fails the build, as on Vercel.
//! - **Go**: `go.mod`'s `toolchain`, else its `go` line, which Go's own toolchain switching fetches
//!   (`GOTOOLCHAIN=auto`); the program then runs on `provided.al2023` whatever its version.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::Value;

/// The Python versions a project may ask for: Vercel's, each a Lambda runtime.
pub const PYTHON_VERSIONS: [&str; 3] = ["3.12", "3.13", "3.14"];
/// The default, for a project that says nothing (or asks for one it can't have): 3.13, what every
/// project ran before versions were read (Vercel's is 3.12).
pub const PYTHON_DEFAULT: &str = "3.13";
/// The Node majors Lambda runs (GA), newest first.
pub const NODE_MAJORS: [u32; 2] = [24, 22];
pub const NODE_DEFAULT: u32 = 24;

/// The version a project runs, its Lambda runtime and where it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// `3.12`, `24`.
    pub version: String,
    /// `python3.12`, `nodejs24.x`.
    pub runtime: String,
    /// What said so (`.python-version`, `pyproject.toml requires-python ">=3.12"`), or None for the
    /// default.
    pub from: Option<String>,
    /// Said to the user: a version that can't be had.
    pub warning: Option<String>,
}

impl Version {
    /// For the build log: `Python 3.12 (from .python-version)`.
    pub fn describe(&self, language: &str) -> String {
        match &self.from {
            Some(from) => format!("{language} {} (from {from})", self.version),
            None => format!("{language} {} (the default)", self.version),
        }
    }
}

fn python(version: &str, from: Option<String>, warning: Option<String>) -> Version {
    Version { version: version.to_owned(), runtime: format!("python{version}"), from, warning }
}

/// The project's Python version. `entry` is relative to `dir`, whose `.python-version` files are
/// read from the entry's folder up to `dir`.
pub fn python_version(dir: &Path, entry: &str) -> Version {
    let mut folder = Path::new(entry).parent().map(Path::to_path_buf).unwrap_or_default();
    loop {
        let file = dir.join(&folder).join(".python-version");
        if let Ok(text) = std::fs::read_to_string(&file) {
            let name = Path::new(&folder).join(".python-version").display().to_string();
            return match text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).and_then(major_minor) {
                Some(asked) if PYTHON_VERSIONS.contains(&asked.as_str()) => python(&asked, Some(name), None),
                Some(asked) => python(PYTHON_DEFAULT, None, Some(format!("{name} asks for Python {asked}, which isn't offered ({}): using {PYTHON_DEFAULT}", PYTHON_VERSIONS.join(", ")))),
                None => python(PYTHON_DEFAULT, None, Some(format!("{name} can't be read as a Python version: using {PYTHON_DEFAULT}"))),
            };
        }
        if !folder.pop() {
            break;
        }
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("pyproject.toml"))
        && let Ok(table) = toml::from_str::<toml::Table>(&text)
        && let Some(spec) = table.get("project").and_then(|p| p.get("requires-python")).and_then(|r| r.as_str())
    {
        return from_specifier(spec, &format!("pyproject.toml requires-python {spec:?}"));
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("Pipfile.lock"))
        && let Ok(lock) = serde_json::from_str::<Value>(&text)
        && let Some(asked) = lock.pointer("/_meta/requires/python_version").and_then(Value::as_str)
    {
        // Vercel raises an older one to its default rather than failing.
        let from = format!("Pipfile.lock python_version {asked:?}");
        return match major_minor(asked) {
            Some(v) if PYTHON_VERSIONS.contains(&v.as_str()) => python(&v, Some(from), None),
            Some(v) if version_tuple(&v) < version_tuple(PYTHON_DEFAULT) => python(PYTHON_DEFAULT, None, Some(format!("{from} is older than Python {PYTHON_DEFAULT}: using {PYTHON_DEFAULT}"))),
            _ => from_specifier(&format!("=={asked}.*"), &from),
        };
    }
    python(PYTHON_DEFAULT, None, None)
}

/// The first offered version a PEP 440 specifier takes, tried as Vercel tries them: the default
/// first, then the newest.
fn from_specifier(spec: &str, from: &str) -> Version {
    let Some(clauses) = parse_specifier(spec) else {
        return python(PYTHON_DEFAULT, None, Some(format!("{from} can't be read: using Python {PYTHON_DEFAULT}")));
    };
    let mut order: Vec<&str> = vec![PYTHON_DEFAULT];
    let mut rest: Vec<&str> = PYTHON_VERSIONS.iter().copied().filter(|v| *v != PYTHON_DEFAULT).collect();
    rest.sort_by_key(|v| std::cmp::Reverse(version_tuple(v)));
    order.extend(rest);
    match order.into_iter().find(|v| clauses.iter().all(|c| c.takes(version_tuple(v)))) {
        Some(v) => python(v, Some(from.to_owned()), None),
        None => python(PYTHON_DEFAULT, None, Some(format!("{from} takes none of the Python versions offered ({}): using {PYTHON_DEFAULT}", PYTHON_VERSIONS.join(", ")))),
    }
}

/// `3.12` from `3.12`, `3.12.4`, `cpython-3.12.4`, `python3.12`, `3.12.*`.
fn major_minor(text: &str) -> Option<String> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let mut parts = text[start..].split(|c: char| !c.is_ascii_digit());
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some(format!("{major}.{minor}"))
}

fn version_tuple(v: &str) -> (u32, u32) {
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// One clause of a PEP 440 specifier, on major.minor only (patch numbers are dropped, as Vercel
/// drops them).
#[derive(Debug, Clone, Copy)]
struct Clause {
    op: Op,
    version: (u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
    Ne,
    Compatible,
}

impl Clause {
    fn takes(&self, v: (u32, u32)) -> bool {
        match self.op {
            Op::Ge => v >= self.version,
            Op::Gt => v > self.version,
            Op::Le => v <= self.version,
            Op::Lt => v < self.version,
            // `==3.12`, `==3.12.4` and `==3.12.*` all mean 3.12 here.
            Op::Eq => v == self.version,
            Op::Ne => v != self.version,
            // `~=3.12` is `==3.*` from 3.12 on in PEP 440; Vercel reads it as `==3.12.*`.
            Op::Compatible => v == self.version,
        }
    }
}

fn parse_specifier(spec: &str) -> Option<Vec<Clause>> {
    let mut clauses = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (op, rest) = [("~=", Op::Compatible), (">=", Op::Ge), ("<=", Op::Le), ("==", Op::Eq), ("!=", Op::Ne), (">", Op::Gt), ("<", Op::Lt)]
            .iter()
            .find_map(|(text, op)| part.strip_prefix(text).map(|rest| (*op, rest.trim())))?;
        let numbers: Vec<u32> = rest.trim_end_matches(".*").split('.').map(|n| n.parse().ok()).collect::<Option<Vec<u32>>>()?;
        let version = match numbers.as_slice() {
            [major] => (*major, 0),
            [major, minor, ..] => (*major, *minor),
            [] => return None,
        };
        // `<3.13.2` still takes 3.13; `!=3.12.4` doesn't rule out 3.12.
        let op = match (op, numbers.len() > 2) {
            (Op::Lt, true) => Op::Le,
            (Op::Ne, true) => continue,
            (op, _) => op,
        };
        clauses.push(Clause { op, version });
    }
    Some(clauses)
}

/// The project's Node major from `package.json`'s `engines.node`, and its Lambda runtime.
pub fn node_version(dir: &Path) -> Result<Version> {
    let node = |major: u32, from: Option<String>, warning: Option<String>| Version { version: major.to_string(), runtime: format!("nodejs{major}.x"), from, warning };
    let range = std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|p| p.pointer("/engines/node").and_then(Value::as_str).map(str::to_owned));
    let Some(range) = range.filter(|r| !r.trim().is_empty()) else { return Ok(node(NODE_DEFAULT, None, None)) };
    let from = format!("package.json engines.node {range:?}");
    let Some(sets) = parse_range(&range) else { bail!("{from} isn't a version range Node's semver reads") };
    match NODE_MAJORS.iter().find(|major| sets.iter().any(|set| set.overlaps(**major))) {
        Some(major) => {
            let warning = (range.trim().split('.').count() == 3 && range.trim().chars().all(|c| c.is_ascii_digit() || c == '.'))
                .then(|| format!("{from} names a whole version: Lambda runs the newest {major}.x"));
            Ok(node(*major, Some(from), warning))
        }
        None if sets.iter().any(|set| set.overlaps(26)) => bail!("{from}: Node 26 is still a preview on Lambda; use 24.x or 22.x"),
        None => bail!("{from} takes none of the Node versions offered ({}): Node 20 and older are past their end of life", NODE_MAJORS.map(|m| format!("{m}.x")).join(", ")),
    }
}

/// A semver comparator set (one side of `||`) as the versions it takes: `[lo, hi)`, each a version
/// as a number (major * 1e6 + minor * 1e3 + patch).
#[derive(Debug, Clone, Copy)]
struct Interval {
    lo: u64,
    hi: u64,
}

impl Interval {
    fn overlaps(&self, major: u32) -> bool {
        let (lo, hi) = (u64::from(major) * 1_000_000, u64::from(major + 1) * 1_000_000);
        self.lo < hi && lo < self.hi && self.lo < self.hi
    }
}

/// Node's semver ranges, as far as `engines.node` uses them: `||`, hyphen ranges, `^`, `~`, `x`
/// and `*` wildcards, comparators and partial versions.
fn parse_range(range: &str) -> Option<Vec<Interval>> {
    let mut sets = Vec::new();
    for alternative in range.split("||") {
        let alternative = alternative.trim();
        let mut interval = Interval { lo: 0, hi: u64::MAX };
        let tokens: Vec<&str> = alternative.split_whitespace().collect();
        let mut i = 0;
        // `>= 22` is written with a space at times.
        let mut joined: Vec<String> = Vec::new();
        while i < tokens.len() {
            let token = tokens[i];
            if matches!(token, ">=" | "<=" | ">" | "<" | "=" | "^" | "~") && i + 1 < tokens.len() {
                joined.push(format!("{token}{}", tokens[i + 1]));
                i += 2;
            } else {
                joined.push(token.to_owned());
                i += 1;
            }
        }
        let mut k = 0;
        while k < joined.len() {
            let (lo, hi) = if k + 2 < joined.len() && joined[k + 1] == "-" {
                // A hyphen range: `22 - 24` takes 22.0.0 up to anything 24.x.
                let (lo, _) = partial(&joined[k])?;
                let (to, to_hi) = partial(&joined[k + 2])?;
                k += 3;
                (lo.0, if to.1 { to_hi } else { to.0 + 1 })
            } else {
                let r = comparator(&joined[k])?;
                k += 1;
                r
            };
            interval.lo = interval.lo.max(lo);
            interval.hi = interval.hi.min(hi);
        }
        sets.push(interval);
    }
    Some(sets)
}

/// A partial version (`22`, `22.1`, `22.x`, `*`) as its lowest version, whether it was partial,
/// and the first version past what it covers.
fn partial(text: &str) -> Option<((u64, bool), u64)> {
    let text = text.trim().trim_start_matches(['v', 'V', '=']);
    if text.is_empty() || text == "*" || text.eq_ignore_ascii_case("x") {
        return Some(((0, true), u64::MAX));
    }
    let core = text.split(['-', '+']).next()?;
    let parts: Vec<&str> = core.split('.').collect();
    let number = |i: usize| -> Option<Option<u64>> {
        match parts.get(i) {
            None => Some(None),
            Some(p) if *p == "x" || *p == "X" || *p == "*" => Some(None),
            Some(p) => p.parse::<u64>().ok().map(Some),
        }
    };
    let (major, minor, patch) = (number(0)?, number(1)?, number(2)?);
    let major = major?;
    let lo = major * 1_000_000 + minor.unwrap_or(0) * 1_000 + patch.unwrap_or(0);
    let hi = match (minor, patch) {
        (None, _) => (major + 1) * 1_000_000,
        (Some(minor), None) => major * 1_000_000 + (minor + 1) * 1_000,
        (Some(_), Some(_)) => lo + 1,
    };
    Some(((lo, minor.is_none() || patch.is_none()), hi))
}

fn comparator(text: &str) -> Option<(u64, u64)> {
    let ops = [">=", "<=", ">", "<", "^", "~", "="];
    let (op, rest) = ops.iter().find_map(|op| text.strip_prefix(op).map(|r| (*op, r))).unwrap_or(("", text));
    let ((lo, _), hi) = partial(rest)?;
    Some(match op {
        ">=" => (lo, u64::MAX),
        ">" => (hi, u64::MAX),
        "<=" => (0, hi),
        "<" => (0, lo),
        "^" => {
            let major = lo / 1_000_000;
            if major > 0 { (lo, (major + 1) * 1_000_000) } else { (lo, hi.max(lo + 1)) }
        }
        "~" => {
            let (major, minor) = (lo / 1_000_000, lo / 1_000 % 1_000);
            let partial_major = hi == (major + 1) * 1_000_000;
            (lo, if partial_major { hi } else { major * 1_000_000 + (minor + 1) * 1_000 })
        }
        _ => (lo, hi),
    })
}

/// The Go version `go.mod` asks for: its `toolchain` (`go1.26.2`), else its `go` line.
pub fn go_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("go.mod")).ok()?;
    let line = |key: &str| text.lines().find_map(|l| l.trim().strip_prefix(key).map(|v| v.trim().to_owned()));
    line("toolchain ").map(|t| t.trim_start_matches("go").to_owned()).or_else(|| line("go "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("forkrail-rt-{}", forkrail_common::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn py(files: &[(&str, &str)]) -> (String, Option<String>) {
        let v = python_version(&project(files), "api/main.py");
        (v.runtime, v.warning)
    }

    #[test]
    fn python_versions_as_vercel_reads_them() {
        // Nothing named: 3.13, what every project ran before versions were read.
        assert_eq!(py(&[]), ("python3.13".into(), None));
        assert_eq!(python_version(&project(&[]), "api/main.py").describe("Python"), "Python 3.13 (the default)");
        assert_eq!(py(&[(".python-version", "3.12\n")]).0, "python3.12");
        assert_eq!(py(&[(".python-version", "3.13\n")]).0, "python3.13");
        assert_eq!(py(&[(".python-version", "# pinned\ncpython-3.14.1\n")]).0, "python3.14");
        // The nearest wins: beside the entry, over the top's.
        assert_eq!(py(&[(".python-version", "3.13"), ("api/.python-version", "3.14")]).0, "python3.14");
        assert_eq!(py(&[(".python-version", "3.13"), ("api/.python-version", "3.12")]).0, "python3.12");
        // .python-version over requires-python.
        assert_eq!(py(&[(".python-version", "3.13"), ("pyproject.toml", "[project]\nrequires-python = \">=3.14\"\n")]).0, "python3.13");
        // The default first, then the newest.
        assert_eq!(py(&[("pyproject.toml", "[project]\nrequires-python = \">=3.10\"\n")]).0, "python3.13");
        assert_eq!(py(&[("pyproject.toml", "[project]\nrequires-python = \">=3.12\"\n")]).0, "python3.13");
        assert_eq!(py(&[("pyproject.toml", "[project]\nrequires-python = \">=3.14\"\n")]).0, "python3.14");
        assert_eq!(py(&[("pyproject.toml", "[project]\nrequires-python = \"<3.13\"\n")]).0, "python3.12");
        assert_eq!(py(&[("pyproject.toml", "[project]\nrequires-python = \">=3.11,<3.13\"\n")]).0, "python3.12");
        assert_eq!(py(&[("pyproject.toml", "[project]\nrequires-python = \"==3.12.*\"\n")]).0, "python3.12");
        assert_eq!(py(&[("pyproject.toml", "[project]\nrequires-python = \"~=3.13\"\n")]).0, "python3.13");
        assert_eq!(py(&[("pyproject.toml", "[project]\nrequires-python = \">3.13,<3.14.5\"\n")]).0, "python3.14");
        // What can't be had gets the default, and says so.
        let (runtime, warning) = py(&[(".python-version", "3.11")]);
        assert_eq!(runtime, "python3.13");
        assert!(warning.unwrap().contains("3.11"));
        assert!(py(&[("pyproject.toml", "[project]\nrequires-python = \"<3.10\"\n")]).1.is_some());
        // Pipfile.lock, and an older one raised to the default.
        assert_eq!(py(&[("Pipfile.lock", r#"{"_meta": {"requires": {"python_version": "3.12"}}}"#)]).0, "python3.12");
        assert_eq!(py(&[("Pipfile.lock", r#"{"_meta": {"requires": {"python_version": "3.14"}}}"#)]).0, "python3.14");
        assert_eq!(py(&[("Pipfile.lock", r#"{"_meta": {"requires": {"python_version": "3.9"}}}"#)]).0, "python3.13");
        // A pyproject.toml without requires-python says nothing.
        assert_eq!(py(&[("pyproject.toml", "[tool.ruff]\nline-length = 100\n")]), ("python3.13".into(), None));
    }

    fn node(range: &str) -> Result<String> {
        node_version(&project(&[("package.json", &format!(r#"{{"engines": {{"node": {range:?}}}}}"#))])).map(|v| v.runtime)
    }

    #[test]
    fn node_versions_as_vercel_reads_them() {
        assert_eq!(node_version(&project(&[("package.json", "{}")])).unwrap().runtime, "nodejs24.x");
        assert_eq!(node_version(&project(&[])).unwrap().runtime, "nodejs24.x");
        for (range, runtime) in [
            ("24.x", "nodejs24.x"),
            ("22.x", "nodejs22.x"),
            ("^22.0.0", "nodejs22.x"),
            ("^24", "nodejs24.x"),
            (">=22.0.0", "nodejs24.x"),
            (">=18", "nodejs24.x"),
            (">= 20 < 23", "nodejs22.x"),
            ("~22.11", "nodejs22.x"),
            ("22", "nodejs22.x"),
            ("20 || 22", "nodejs22.x"),
            ("18.x || 20.x || 22.x || 24.x", "nodejs24.x"),
            ("22.12.0", "nodejs22.x"),
            ("v22", "nodejs22.x"),
            ("*", "nodejs24.x"),
            ("22 - 24", "nodejs24.x"),
            ("<23", "nodejs22.x"),
        ] {
            assert_eq!(node(range).unwrap_or_else(|e| panic!("{range}: {e}")), runtime, "{range}");
        }
        assert!(node("20.x").unwrap_err().to_string().contains("end of life"));
        assert!(node("18").is_err());
        assert!(node("26.x").unwrap_err().to_string().contains("preview"));
        assert!(node("not a range ^^").is_err());
    }

    #[test]
    fn go_versions() {
        assert_eq!(go_version(&project(&[("go.mod", "module x\n\ngo 1.25\n")])).as_deref(), Some("1.25"));
        assert_eq!(go_version(&project(&[("go.mod", "module x\n\ngo 1.24.2\ntoolchain go1.26.1\n")])).as_deref(), Some("1.26.1"));
        assert_eq!(go_version(&project(&[])), None);
    }
}
