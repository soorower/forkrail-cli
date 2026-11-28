//! Python functions in `api/` (Vercel's zero-config `api/*.py`): every file a function, all served
//! by one Python app in the deployment's one Lambda function. The project's requirements are
//! installed once for all of them, as for any Python app (package.rs), and
//! `.forkrail/functions.py` lists the functions, each with its file and URL paths, for the
//! runtime's shim (crates/runtime/src/shim.py), which imports each on its first request and serves
//! its `handler` class, or its ASGI or WSGI `app`.

use crate::functions;

/// The app's entry, relative to the package's `app/`: the runtime passes the router's function
/// header to it (crates/runtime/src/app.rs, `DISPATCHERS`), and the shim reads its `FUNCTIONS`.
pub const ENTRY: &str = ".forkrail/functions.py";

/// A Python function: its file, and the top-level name it serves (`app`, `application`, or the
/// `handler` class).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyFunction {
    pub file: String,
    pub variable: String,
}

/// The project's Python functions: the `api/*.py` files that serve something, as Vercel's analysis
/// finds it (`entry_variable`). The others are modules the functions import, not functions.
pub fn discover(dir: &std::path::Path) -> Vec<PyFunction> {
    functions::discover(dir, "py")
        .into_iter()
        .filter_map(|file| {
            let text = std::fs::read_to_string(dir.join(&file)).ok()?;
            Some(PyFunction { variable: entry_variable(&text)?.to_owned(), file })
        })
        .collect()
}

/// What a Python file serves, as Vercel's build reads it (`@vercel/python-analysis`): the first
/// top-level statement, in source order, that binds `app` or `application` (assigned, annotated,
/// defined, or imported under that name) or defines a class named `handler` in any case.
pub fn entry_variable(text: &str) -> Option<&str> {
    let mut in_string: Option<&str> = None;
    for line in text.lines() {
        // Skip the insides of triple-quoted strings (docstrings).
        if let Some(quote) = in_string {
            if line.contains(quote) {
                in_string = None;
            }
            continue;
        }
        for quote in ["\"\"\"", "'''"] {
            if line.matches(quote).count() == 1 {
                in_string = Some(quote);
            }
        }
        if line.starts_with([' ', '\t', '#']) || line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("class ") {
            let name: &str = rest.split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or("");
            if name.eq_ignore_ascii_case("handler") {
                return Some(&rest[..name.len()]);
            }
            continue;
        }
        for name in ["app", "application"] {
            if binds(line, name) {
                return Some(name);
            }
        }
    }
    None
}

/// Whether a top-level line binds `name`: `name = ...`, `name: T = ...`, `def name(`,
/// `async def name(`, `from x import name`, `from x import y as name`.
fn binds(line: &str, name: &str) -> bool {
    let word_end = |rest: &str| rest.is_empty() || !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_');
    if let Some(rest) = line.strip_prefix(name)
        && word_end(rest)
    {
        let rest = rest.trim_start();
        if (rest.starts_with('=') && !rest.starts_with("==")) || rest.starts_with(':') {
            return true;
        }
    }
    for keyword in ["def ", "async def "] {
        if let Some(rest) = line.strip_prefix(keyword).and_then(|r| r.strip_prefix(name))
            && word_end(rest)
        {
            return true;
        }
    }
    if line.starts_with("from ")
        && let Some((_, names)) = line.split_once(" import ")
    {
        return names.trim().trim_matches(|c| c == '(' || c == ')').split(',').any(|part| {
            let part = part.trim();
            part == name || part.rsplit_once(" as ").is_some_and(|(_, alias)| alias.trim() == name)
        });
    }
    false
}

/// The entry's source: the functions, as Python literals.
pub fn entry_source(functions: &[PyFunction]) -> String {
    let mut out = String::from("# Written by forkrail: the project's functions in api/, served by forkrail's runtime (shim.py).\nFUNCTIONS = [\n");
    for f in functions {
        let paths = forkrail_common::rules::file_paths(&f.file);
        // JSON strings and lists are Python literals too.
        out.push_str(&format!(
            "    {{\"file\": {}, \"variable\": {}, \"paths\": {}}},\n",
            serde_json::to_string(&f.file).expect("a string"),
            serde_json::to_string(&f.variable).expect("a string"),
            serde_json::to_string(&paths).expect("strings")
        ));
    }
    out.push_str("]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_lists_every_function() {
        let fns = [PyFunction { file: "api/index.py".into(), variable: "handler".into() }, PyFunction { file: "api/users/[id].py".into(), variable: "app".into() }];
        let source = entry_source(&fns);
        assert!(source.contains(r#"{"file": "api/index.py", "variable": "handler", "paths": ["/api/index.py","/api/index","/api","/api/"]},"#), "{source}");
        assert!(source.contains(r#"{"file": "api/users/[id].py", "variable": "app", "paths": ["/api/users/[id].py","/api/users/[id]"]},"#), "{source}");
    }

    #[test]
    fn what_a_file_serves_as_vercel_reads_it() {
        assert_eq!(entry_variable("from http.server import BaseHTTPRequestHandler\n\nclass handler(BaseHTTPRequestHandler):\n    def do_GET(self):\n        pass\n"), Some("handler"));
        assert_eq!(entry_variable("class Handler(BaseHTTPRequestHandler):\n    pass\n"), Some("Handler"));
        assert_eq!(entry_variable("from flask import Flask\napp = Flask(__name__)\n"), Some("app"));
        assert_eq!(entry_variable("app: FastAPI = FastAPI()\n"), Some("app"));
        assert_eq!(entry_variable("async def app(scope, receive, send):\n    pass\n"), Some("app"));
        assert_eq!(entry_variable("from .server import create as application\n"), Some("application"));
        // Source order decides, not the name.
        assert_eq!(entry_variable("application = make()\napp = other()\n"), Some("application"));
        // A function or a value called handler isn't one; nor names inside a block or a docstring.
        assert_eq!(entry_variable("def handler(event, context):\n    pass\nhandler = 1\n"), None);
        assert_eq!(entry_variable("\"\"\"\napp = 1\n\"\"\"\nif True:\n    app = 2\nimport app\n"), None);
        assert_eq!(entry_variable("def helper():\n    return 1\n"), None);
        assert_eq!(entry_variable("apps = []\napplication_name = 'x'\n"), None);
    }
}
