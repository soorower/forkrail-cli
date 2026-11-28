//! `forkrail redirects …` and `forkrail routes …`: a project's own bulk redirects and routing
//! rules (project_routing.rs), as Vercel's `vercel redirects` and `vercel routes` manage them.
//! Signed in, each op goes through the dashboard's API as you; with `--aws`, straight to the
//! install. Changes are staged until `publish`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Map, Value, json};

use crate::api;
use crate::deploy::{Ctx, log};
use crate::project_routing::{self, RedirectsOp, RoutesOp};

/// Who carries an op: the install itself, or its dashboard's API.
pub enum Via<'a> {
    Aws(&'a Ctx),
    Api(&'a api::Client),
}

impl Via<'_> {
    async fn send(&self, project: &str, set: &str, op: Value, changes: bool) -> Result<Value> {
        match self {
            Via::Aws(ctx) => match set {
                "redirects" => {
                    let op: RedirectsOp = serde_json::from_value(op).context("an op the CLI made")?;
                    project_routing::redirects(ctx, project, op, None).await
                }
                _ => {
                    let op: RoutesOp = serde_json::from_value(op).context("an op the CLI made")?;
                    project_routing::routes(ctx, project, op, None).await
                }
            },
            Via::Api(client) => {
                let path = format!("projects/{project}/{set}");
                if changes { client.post(&path, op).await } else { client.ask(&path, op).await }
            }
        }
    }
}

#[derive(Subcommand)]
pub enum RedirectsCommand {
    /// The redirects: the staged ones if there are changes, else the live ones.
    Ls {
        project: String,
        /// `live`, `staging`, or a version's id.
        #[arg(long)]
        version: Option<String>,
        /// Only those whose source or destination has this.
        #[arg(short, long)]
        query: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Every version, newest first, and which is live and which staged.
    Versions { project: String },
    /// Add a redirect (or change the one with the same source). Staged until `publish`.
    Add {
        project: String,
        /// A path (`/old`) or a full URL (`https://old.example.com/old`).
        source: String,
        destination: String,
        /// 301, 302, 303, 307 or 308; 307 if not given (308 with --permanent).
        #[arg(long)]
        status: Option<u16>,
        #[arg(long)]
        permanent: bool,
        #[arg(long)]
        case_sensitive: bool,
        /// Keep the request's query string.
        #[arg(long)]
        preserve_query_params: bool,
    },
    /// Remove redirects by source. Staged until `publish`.
    Rm { project: String, sources: Vec<String> },
    /// Add the redirects in a CSV, JSON or JSONL file (or replace them all with --replace).
    /// Staged until `publish`.
    Import {
        project: String,
        file: PathBuf,
        #[arg(long)]
        replace: bool,
    },
    /// The redirects as CSV, on stdout.
    Export {
        project: String,
        #[arg(long)]
        version: Option<String>,
    },
    /// Make the staged changes (or --version) live.
    Publish {
        project: String,
        #[arg(long)]
        version: Option<String>,
    },
    /// Make an older version live again.
    Restore { project: String, version: String },
    /// Drop the staged changes.
    Discard { project: String },
    /// What a URL gets: from the staged redirects if there are changes, else the live ones.
    Test {
        project: String,
        url: String,
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RoutesCommand {
    /// The rules, in order: the staged ones if there are changes, else the live ones.
    Ls {
        project: String,
        #[arg(long)]
        version: Option<String>,
        /// Each rule in full.
        #[arg(long)]
        expand: bool,
    },
    /// Every version, newest first.
    Versions { project: String },
    /// Add a rule. Staged until `publish`.
    Add {
        project: String,
        name: String,
        #[command(flatten)]
        rule: RuleArgs,
        /// start, end, before:ID, after:ID, or a position from 1.
        #[arg(long)]
        position: Option<String>,
        /// Add it switched off.
        #[arg(long)]
        disabled: bool,
    },
    /// Change a rule (by id or name): what's given replaces what it had; conditions and changes
    /// are added to its own unless cleared. Staged until `publish`.
    Update {
        project: String,
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        rule: RuleArgs,
        #[arg(long)]
        no_dest: bool,
        #[arg(long)]
        no_status: bool,
        #[arg(long)]
        clear_conditions: bool,
        /// Remove its header and query changes.
        #[arg(long)]
        clear_transforms: bool,
        #[arg(long)]
        clear_headers: bool,
    },
    Rm { project: String, ids: Vec<String> },
    Enable { project: String, id: String },
    Disable { project: String, id: String },
    /// Move a rule: --position start, end, before:ID, after:ID, or a position from 1.
    Move {
        project: String,
        id: String,
        #[arg(long)]
        position: String,
    },
    /// Replace every rule with those in a JSON file (a list of rules, as `export` writes them).
    Import { project: String, file: PathBuf },
    /// The rules as JSON, on stdout.
    Export {
        project: String,
        #[arg(long)]
        version: Option<String>,
    },
    Publish {
        project: String,
        #[arg(long)]
        version: Option<String>,
    },
    Restore { project: String, version: String },
    Discard { project: String },
    /// What a request to URL gets from the rules: which rule decides, and what it does.
    Test {
        project: String,
        url: String,
        #[arg(long, default_value = "GET")]
        method: String,
        /// A request header, `name: value` (repeatable).
        #[arg(long = "header", short = 'H')]
        headers: Vec<String>,
        #[arg(long)]
        version: Option<String>,
    },
}

/// What a rule matches and does, as flags (Vercel's `vercel routes add`).
#[derive(clap::Args, Default)]
pub struct RuleArgs {
    #[arg(long)]
    description: Option<String>,
    /// The path to match.
    #[arg(long)]
    src: Option<String>,
    /// equals, path-to-regexp or regex; guessed from --src if not given.
    #[arg(long)]
    src_syntax: Option<String>,
    #[arg(long)]
    case_sensitive: bool,
    /// rewrite, redirect or set-status.
    #[arg(long)]
    action: Option<String>,
    #[arg(long)]
    dest: Option<String>,
    #[arg(long)]
    status: Option<u16>,
    /// `type:key`, `type:key:value` or `type:key:op=value`; types header, cookie, query, host
    /// (`host:value`); ops eq, contains, re, exists. Repeatable.
    #[arg(long)]
    has: Vec<String>,
    #[arg(long)]
    missing: Vec<String>,
    #[arg(long, value_name = "KEY=VALUE")]
    set_response_header: Vec<String>,
    #[arg(long, value_name = "KEY=VALUE")]
    append_response_header: Vec<String>,
    #[arg(long, value_name = "KEY")]
    delete_response_header: Vec<String>,
    #[arg(long, value_name = "KEY=VALUE")]
    set_request_header: Vec<String>,
    #[arg(long, value_name = "KEY=VALUE")]
    append_request_header: Vec<String>,
    #[arg(long, value_name = "KEY")]
    delete_request_header: Vec<String>,
    #[arg(long, value_name = "KEY=VALUE")]
    set_request_query: Vec<String>,
    #[arg(long, value_name = "KEY=VALUE")]
    append_request_query: Vec<String>,
    #[arg(long, value_name = "KEY")]
    delete_request_query: Vec<String>,
}

/// A `--has` or `--missing` condition.
fn condition(text: &str) -> Result<Value> {
    let (kind, rest) = text.split_once(':').with_context(|| format!("condition {text:?}: write type:key, type:key:value or type:key:op=value"))?;
    if !["header", "cookie", "query", "host"].contains(&kind) {
        bail!("condition {text:?}: the type is header, cookie, query or host");
    }
    if kind == "host" {
        return Ok(json!({"type": "host", "value": rest}));
    }
    let (key, value) = match rest.split_once(':') {
        Some((k, v)) => (k, Some(v)),
        None => (rest, None),
    };
    let value = match value {
        None => None,
        Some(v) => match v.split_once('=') {
            Some(("eq", x)) => Some(json!({"eq": x})),
            Some(("contains", x)) => Some(json!({"re": format!(".*{}.*", regex_escape(x))})),
            Some(("re", x)) => Some(json!({"re": x})),
            Some(("exists", _)) => None,
            _ if v == "exists" => None,
            _ => Some(json!({"eq": v})),
        },
    };
    let mut c = json!({"type": kind, "key": key});
    if let Some(value) = value {
        c["value"] = value;
    }
    Ok(c)
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl RuleArgs {
    /// The transforms the header and query flags ask for.
    fn transforms(&self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let pairs = [
            ("response.headers", "set", &self.set_response_header),
            ("response.headers", "append", &self.append_response_header),
            ("request.headers", "set", &self.set_request_header),
            ("request.headers", "append", &self.append_request_header),
            ("request.query", "set", &self.set_request_query),
            ("request.query", "append", &self.append_request_query),
        ];
        for (kind, op, list) in pairs {
            for item in list {
                let (k, v) = item.split_once('=').with_context(|| format!("{item:?}: write KEY=VALUE"))?;
                out.push(json!({"type": kind, "op": op, "target": {"key": k}, "args": v}));
            }
        }
        for (kind, list) in [("response.headers", &self.delete_response_header), ("request.headers", &self.delete_request_header), ("request.query", &self.delete_request_query)] {
            for k in list {
                out.push(json!({"type": kind, "op": "delete", "target": {"key": k}}));
            }
        }
        Ok(out)
    }

    /// Apply the flags to a rule (a new one's is empty).
    fn apply(&self, rule: &mut Map<String, Value>) -> Result<()> {
        if let Some(d) = &self.description {
            rule.insert("description".into(), json!(d));
        }
        if let Some(s) = &self.src_syntax {
            if !["equals", "path-to-regexp", "regex"].contains(&s.as_str()) {
                bail!("--src-syntax: equals, path-to-regexp or regex");
            }
            rule.insert("srcSyntax".into(), json!(s));
        }
        let route = rule.entry("route").or_insert_with(|| json!({})).as_object_mut().context("a rule's route")?;
        if let Some(src) = &self.src {
            route.insert("src".into(), json!(src));
        }
        if self.case_sensitive {
            route.insert("caseSensitive".into(), json!(true));
        }
        if let Some(dest) = &self.dest {
            route.insert("dest".into(), json!(dest));
        }
        if let Some(status) = self.status {
            route.insert("status".into(), json!(status));
        }
        match self.action.as_deref() {
            None => {}
            Some("rewrite") => {
                if !route.contains_key("dest") {
                    bail!("a rewrite needs --dest");
                }
                if route.get("status").and_then(Value::as_u64).is_some_and(|s| (300..400).contains(&s)) {
                    route.remove("status");
                }
            }
            Some("redirect") => {
                if !route.contains_key("dest") {
                    bail!("a redirect needs --dest");
                }
                if !route.get("status").and_then(Value::as_u64).is_some_and(|s| [301, 302, 303, 307, 308].contains(&s)) {
                    route.insert("status".into(), json!(307));
                }
            }
            Some("set-status" | "set_status") => {
                route.remove("dest");
                if !route.contains_key("status") {
                    bail!("set-status needs --status");
                }
            }
            Some(other) => bail!("--action {other}: rewrite, redirect or set-status"),
        }
        for (key, list) in [("has", &self.has), ("missing", &self.missing)] {
            if list.is_empty() {
                continue;
            }
            let entry = route.entry(key).or_insert_with(|| json!([])).as_array_mut().context("conditions")?;
            for c in list {
                entry.push(condition(c)?);
            }
        }
        let transforms = self.transforms()?;
        if !transforms.is_empty() {
            route.entry("transforms").or_insert_with(|| json!([])).as_array_mut().context("transforms")?.extend(transforms);
        }
        Ok(())
    }
}

fn position(text: &str) -> Result<Value> {
    Ok(match text {
        "start" | "first" => json!({"placement": "start"}),
        "end" | "last" => json!({"placement": "end"}),
        _ => {
            if let Some(id) = text.strip_prefix("before:") {
                json!({"placement": "before", "id": id})
            } else if let Some(id) = text.strip_prefix("after:") {
                json!({"placement": "after", "id": id})
            } else if let Ok(index) = text.parse::<usize>() {
                json!({"placement": "at", "index": index})
            } else {
                bail!("--position: start, end, before:ID, after:ID, or a position from 1")
            }
        }
    })
}

fn staged(answer: &Value, what: &str) {
    let staging = answer.get("staging").and_then(Value::as_str).unwrap_or("?");
    log(&format!("{what}: staged as version {staging}; `publish` makes it live"));
}

pub async fn redirects(via: Via<'_>, command: RedirectsCommand) -> Result<()> {
    let send = |project: String, op: Value, changes: bool| {
        let via = &via;
        async move { via.send(&project, "redirects", op, changes).await }
    };
    match command {
        RedirectsCommand::Ls { project, version, query, offset, limit } => {
            let answer = send(project, json!({"op": "list", "version": version, "query": query, "offset": offset, "limit": limit}), false).await?;
            let list = answer.get("redirects").and_then(Value::as_array).cloned().unwrap_or_default();
            let total = answer.get("total").and_then(Value::as_u64).unwrap_or(0);
            if list.is_empty() {
                log(if total == 0 { "no redirects" } else { "no more redirects" });
            }
            for r in &list {
                let flags = [("case-sensitive", "caseSensitive"), ("keeps query", "preserveQueryParams")]
                    .iter()
                    .filter(|(_, k)| r.get(*k).and_then(Value::as_bool) == Some(true))
                    .map(|(n, _)| *n)
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("{}  {}  →  {}{}", r["statusCode"], r["source"].as_str().unwrap_or(""), r["destination"].as_str().unwrap_or(""), if flags.is_empty() { String::new() } else { format!("  ({flags})") });
            }
            let shown = offset + list.len();
            let which = answer.get("version").and_then(Value::as_str).unwrap_or("none");
            let state = if answer.get("staging").and_then(Value::as_str) == Some(which) { "staged" } else { "live" };
            log(&format!("{shown} of {total}, version {which} ({state}){}", if (shown as u64) < total { format!("; next: --offset {shown}") } else { String::new() }));
        }
        RedirectsCommand::Versions { project } => versions(&send(project, json!({"op": "versions"}), false).await?),
        RedirectsCommand::Add { project, source, destination, status, permanent, case_sensitive, preserve_query_params } => {
            let r = forkrail_common::rules::bulk::BulkRedirect::new(&source, &destination, Some(permanent), status, case_sensitive, preserve_query_params).map_err(anyhow::Error::msg)?;
            let answer = send(project, json!({"op": "add", "redirects": [r]}), true).await?;
            staged(&answer, &format!("{source} → {destination}"));
        }
        RedirectsCommand::Rm { project, sources } => {
            if sources.is_empty() {
                bail!("name the sources to remove");
            }
            let answer = send(project, json!({"op": "remove", "sources": sources}), true).await?;
            staged(&answer, "removed");
        }
        RedirectsCommand::Import { project, file, replace } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // Checked here first, so a mistake is found before anything is sent.
            let n = forkrail_common::rules::bulk::parse(&name, &text).map_err(anyhow::Error::msg)?.len();
            if matches!(via, Via::Api(_)) && text.len() > 5 * 1024 * 1024 {
                bail!("{} is larger than the 5 MB the dashboard takes at once: split it, or import it with --aws", file.display());
            }
            let answer = send(project, json!({"op": "import", "name": name, "text": text, "replace": replace}), true).await?;
            let count = answer.get("count").and_then(Value::as_u64).unwrap_or(0);
            staged(&answer, &format!("{n} imported, {count} in all"));
        }
        RedirectsCommand::Export { project, version } => {
            let answer = send(project, json!({"op": "export", "version": version}), false).await?;
            print!("{}", answer.get("csv").and_then(Value::as_str).unwrap_or(""));
        }
        RedirectsCommand::Publish { project, version } => published(&send(project, json!({"op": "publish", "version": version}), true).await?),
        RedirectsCommand::Restore { project, version } => published(&send(project, json!({"op": "restore", "version": version}), true).await?),
        RedirectsCommand::Discard { project } => {
            send(project, json!({"op": "discard"}), true).await?;
            log("staged changes discarded");
        }
        RedirectsCommand::Test { project, url, version } => {
            let answer = send(project, json!({"op": "test", "url": url, "version": version}), false).await?;
            match answer.get("redirect").filter(|r| !r.is_null()) {
                Some(r) => println!("{} → {}", r["status"], r["location"].as_str().unwrap_or("")),
                None => println!("no redirect: the request goes on to the project's rules and deployment"),
            }
        }
    }
    Ok(())
}

fn versions(answer: &Value) {
    let list = answer.get("versions").and_then(Value::as_array).cloned().unwrap_or_default();
    if list.is_empty() {
        log("no versions yet");
    }
    for v in list {
        let mark = if v["live"] == true {
            "live   "
        } else if v["staging"] == true {
            "staged "
        } else {
            "       "
        };
        println!(
            "{mark} {}  {}  {:>7}  {}{}",
            v["id"].as_str().unwrap_or(""),
            v["created"].as_str().unwrap_or(""),
            v["count"],
            v["note"].as_str().unwrap_or(""),
            v["created_by"].as_str().map(|b| format!(" (by {b})")).unwrap_or_default()
        );
    }
}

fn published(answer: &Value) {
    let regions: Vec<&str> = answer.get("regions").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    log(&format!("version {} is live ({} in all), in {}; the routers follow within seconds", answer["live"].as_str().unwrap_or("?"), answer["count"], regions.join(", ")));
}

fn describe_rule(r: &Value, expand: bool) -> String {
    let route = &r["route"];
    let kind = r["type"].as_str().unwrap_or("");
    let what = match kind {
        "redirect" => format!("redirect {} → {}", route["status"], route["dest"].as_str().unwrap_or("")),
        "rewrite" => format!("rewrite → {}", route["dest"].as_str().unwrap_or("")),
        "set_status" => format!("status {}", route["status"]),
        _ => "changes headers or query".to_owned(),
    };
    let mut line = format!(
        "{}{}  {}  [{}] {}  {what}",
        if r["enabled"] == false { "(off) " } else { "" },
        r["id"].as_str().unwrap_or(""),
        r["name"].as_str().unwrap_or(""),
        r["srcSyntax"].as_str().unwrap_or(""),
        route["src"].as_str().unwrap_or("")
    );
    if expand {
        line.push('\n');
        line.push_str(&serde_json::to_string_pretty(r).unwrap_or_default());
    }
    line
}

pub async fn routes(via: Via<'_>, command: RoutesCommand) -> Result<()> {
    let send = |project: String, op: Value, changes: bool| {
        let via = &via;
        async move { via.send(&project, "routes", op, changes).await }
    };
    let show = |answer: &Value| {
        for (i, r) in answer.get("rules").and_then(Value::as_array).into_iter().flatten().enumerate() {
            println!("{:>3}. {}", i + 1, describe_rule(r, false));
        }
    };
    match command {
        RoutesCommand::Ls { project, version, expand } => {
            let answer = send(project, json!({"op": "list", "version": version}), false).await?;
            let rules = answer.get("rules").and_then(Value::as_array).cloned().unwrap_or_default();
            if rules.is_empty() {
                log("no rules");
            }
            for (i, r) in rules.iter().enumerate() {
                println!("{:>3}. {}", i + 1, describe_rule(r, expand));
            }
            if let Some(v) = answer.get("version").and_then(Value::as_str) {
                let state = if answer.get("staging").and_then(Value::as_str) == Some(v) { "staged" } else { "live" };
                log(&format!("version {v} ({state})"));
            }
        }
        RoutesCommand::Versions { project } => versions(&send(project, json!({"op": "versions"}), false).await?),
        RoutesCommand::Add { project, name, rule, position: at, disabled } => {
            if rule.src.is_none() {
                bail!("a rule needs --src");
            }
            let mut r = Map::new();
            r.insert("name".into(), json!(name));
            r.insert("enabled".into(), json!(!disabled));
            rule.apply(&mut r)?;
            let mut op = json!({"op": "add", "rule": r});
            if let Some(at) = at {
                op["position"] = position(&at)?;
            }
            let answer = send(project, op, true).await?;
            show(&answer);
            staged(&answer, &format!("rule {name:?} added"));
        }
        RoutesCommand::Update { project, id, name, rule, no_dest, no_status, clear_conditions, clear_transforms, clear_headers } => {
            let current = send(project.clone(), json!({"op": "list"}), false).await?;
            let rules = current.get("rules").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut r = rules
                .into_iter()
                .find(|r| r["id"] == id.as_str() || r["name"] == id.as_str())
                .and_then(|r| r.as_object().cloned())
                .with_context(|| format!("there's no rule {id}"))?;
            if let Some(name) = name {
                r.insert("name".into(), json!(name));
            }
            if let Some(route) = r.get_mut("route").and_then(Value::as_object_mut) {
                if no_dest {
                    route.remove("dest");
                }
                if no_status {
                    route.remove("status");
                }
                if clear_conditions {
                    route.remove("has");
                    route.remove("missing");
                }
                if clear_transforms {
                    route.remove("transforms");
                }
                if clear_headers {
                    route.remove("headers");
                }
            }
            rule.apply(&mut r)?;
            let rule_id = r.get("id").and_then(Value::as_str).unwrap_or(&id).to_owned();
            let answer = send(project, json!({"op": "update", "id": rule_id, "rule": r}), true).await?;
            show(&answer);
            staged(&answer, &format!("rule {id} changed"));
        }
        RoutesCommand::Rm { project, ids } => {
            let answer = send(project, json!({"op": "remove", "ids": ids}), true).await?;
            staged(&answer, "removed");
        }
        RoutesCommand::Enable { project, id } => staged(&send(project, json!({"op": "enable", "id": id, "enabled": true}), true).await?, "enabled"),
        RoutesCommand::Disable { project, id } => staged(&send(project, json!({"op": "enable", "id": id, "enabled": false}), true).await?, "disabled"),
        RoutesCommand::Move { project, id, position: at } => {
            let answer = send(project, json!({"op": "move", "id": id, "position": position(&at)?}), true).await?;
            show(&answer);
            staged(&answer, "moved");
        }
        RoutesCommand::Import { project, file } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let value: Value = serde_json::from_str(&text).with_context(|| format!("{}: not JSON", file.display()))?;
            let rules = match value {
                Value::Array(_) => value,
                Value::Object(mut o) => o.remove("rules").context("expected a list of rules, or {\"rules\": [...]}")?,
                _ => bail!("expected a list of rules"),
            };
            let answer = send(project, json!({"op": "replace", "rules": rules}), true).await?;
            show(&answer);
            staged(&answer, "rules replaced");
        }
        RoutesCommand::Export { project, version } => {
            let answer = send(project, json!({"op": "list", "version": version}), false).await?;
            let mut rules = answer.get("rules").cloned().unwrap_or(json!([]));
            for r in rules.as_array_mut().into_iter().flatten() {
                if let Some(o) = r.as_object_mut() {
                    o.remove("type");
                }
            }
            println!("{}", serde_json::to_string_pretty(&rules)?);
        }
        RoutesCommand::Publish { project, version } => published(&send(project, json!({"op": "publish", "version": version}), true).await?),
        RoutesCommand::Restore { project, version } => published(&send(project, json!({"op": "restore", "version": version}), true).await?),
        RoutesCommand::Discard { project } => {
            send(project, json!({"op": "discard"}), true).await?;
            log("staged changes discarded");
        }
        RoutesCommand::Test { project, url, method, headers, version } => {
            let mut map = BTreeMap::new();
            for h in &headers {
                let (k, v) = h.split_once(':').with_context(|| format!("--header {h:?}: write `name: value`"))?;
                map.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
            }
            let answer = send(project, json!({"op": "test", "url": url, "method": method, "headers": map, "version": version}), false).await?;
            let rule = answer.get("rule").and_then(Value::as_str);
            match answer.get("action").and_then(Value::as_str).unwrap_or("continue") {
                "redirect" => println!("{}: redirect {} → {}", rule.unwrap_or("?"), answer["status"], answer["to"].as_str().unwrap_or("")),
                "rewrite" => println!("{}: rewrite → {}", rule.unwrap_or("?"), answer["to"].as_str().unwrap_or("")),
                "status" => println!("{}: status {}", rule.unwrap_or("?"), answer["status"]),
                _ => println!("no rule decides: the request goes on to the deployment"),
            }
            for h in answer.get("headers").and_then(Value::as_array).into_iter().flatten() {
                println!("  response header {}: {}", h["key"].as_str().unwrap_or(""), h["value"].as_str().unwrap_or(""));
            }
            for t in answer.get("transforms").and_then(Value::as_array).into_iter().flatten() {
                println!("  {} {} {}{}", t["type"].as_str().unwrap_or(""), t["op"].as_str().unwrap_or(""), t["target"]["key"], t.get("args").map(|a| format!(" {a}")).unwrap_or_default());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_make_a_rule() {
        let args = RuleArgs {
            src: Some("/protected/:path*".into()),
            src_syntax: Some("path-to-regexp".into()),
            action: Some("redirect".into()),
            dest: Some("/login".into()),
            missing: vec!["cookie:session".into()],
            has: vec!["header:x-a:contains=b.c".into(), "host:example.com".into()],
            set_response_header: vec!["cache-control=no-store".into()],
            delete_request_query: vec!["utm".into()],
            ..Default::default()
        };
        let mut r = Map::new();
        r.insert("name".into(), json!("Auth"));
        args.apply(&mut r).unwrap();
        let rule = forkrail_common::rules::project::from_json(&json!([r])).unwrap();
        assert_eq!(rule[0].kind(), forkrail_common::rules::project::RuleType::Redirect);
        assert_eq!(rule[0].route.status, Some(307));
        forkrail_common::rules::project::compile(&rule).unwrap();
        assert!(condition("nope").is_err());
        assert_eq!(position("after:rt_1").unwrap(), json!({"placement": "after", "id": "rt_1"}));
    }
}
