//! `forkrail firewall …`: a project's firewall, or the install's with `--install` (firewall.rs).
//! Signed in, each op goes through the dashboard's API as you; with `--aws`, straight to the
//! install. Every change is a draft until `publish`; `diff` shows what publishing would change.
//!
//! Conditions on the command line are `[not] TYPE[:KEY] OP [VALUE]`, Vercel's names with a few
//! short ones: `path pre /admin`, `not geo_country inc US,CA`, `header:x-api-key ex`,
//! `ip eq 192.0.2.0/24`, `ua sub curl`. `--if` starts a group, `--and` adds to it, `--or` starts
//! another; a rule matches when any group does.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::firewall::{self as fw, FirewallOp};
use crate::routing_cli::Via;

/// Whose firewall: a project's, or the install's.
#[derive(Args, Clone)]
pub struct Scope {
    /// The project (or `--install`).
    #[arg(required_unless_present = "install")]
    pub project: Option<String>,
    /// The install's own firewall, in front of every project (an owner's to change).
    #[arg(long, conflicts_with = "project")]
    pub install: bool,
}

impl Scope {
    fn name(&self) -> Option<&str> {
        if self.install { None } else { self.project.as_deref() }
    }

    fn label(&self) -> String {
        match self.name() {
            Some(p) => p.to_owned(),
            None => "install".to_owned(),
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Then {
    Deny,
    /// The challenge page: browsers pass after a moment's work, scripts don't.
    Challenge,
    Log,
    Bypass,
    Redirect,
    #[value(name = "rate_limit", alias = "rate-limit")]
    RateLimit,
}

/// What a request over a rate limit gets.
#[derive(Clone, Copy, ValueEnum)]
pub enum Over {
    /// 429 Too Many Requests.
    #[value(name = "rate_limit", alias = "429")]
    RateLimit,
    /// 403.
    Deny,
    /// The challenge page.
    Challenge,
    /// Nothing: recorded only.
    Log,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Algo {
    #[value(name = "fixed_window")]
    FixedWindow,
    #[value(name = "token_bucket")]
    TokenBucket,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum IpThen {
    Deny,
    Challenge,
    Log,
    Bypass,
}

#[derive(Subcommand)]
pub enum FirewallCommand {
    /// The rules and IP blocks: the draft if there is one, else what's live.
    Show {
        #[command(flatten)]
        scope: Scope,
        /// `live`, `staging`, or a version's id.
        #[arg(long)]
        version: Option<String>,
        /// The configuration as JSON (Vercel's shape).
        #[arg(long)]
        json: bool,
    },
    /// Custom rules.
    Rule {
        #[command(subcommand)]
        command: RuleCommand,
    },
    /// IP blocks: an address or range, on one host, `*.domain`, or every host (`*`).
    Ip {
        #[command(subcommand)]
        command: IpCommand,
    },
    /// Turn the custom rules on (IP blocks apply either way). A draft until `publish`.
    Enable {
        #[command(flatten)]
        scope: Scope,
    },
    /// Turn the custom rules off. A draft until `publish`.
    Disable {
        #[command(flatten)]
        scope: Scope,
    },
    /// What publishing the draft would change.
    Diff {
        #[command(flatten)]
        scope: Scope,
    },
    /// Make the draft (or --version) live: every router has it within about a second.
    Publish {
        #[command(flatten)]
        scope: Scope,
        #[arg(long)]
        version: Option<String>,
    },
    /// Drop the draft.
    Discard {
        #[command(flatten)]
        scope: Scope,
    },
    /// Make an older version live again.
    Rollback {
        #[command(flatten)]
        scope: Scope,
        version: String,
    },
    /// Every version, newest first.
    Versions {
        #[command(flatten)]
        scope: Scope,
    },
    /// What a request would get from the draft (else what's live).
    Test {
        #[command(flatten)]
        scope: Scope,
        url: String,
        #[arg(short = 'X', long, default_value = "GET")]
        method: String,
        /// `Name: value`, more than once.
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// The client's address.
        #[arg(long)]
        ip: Option<String>,
        /// The client's country, `US`.
        #[arg(long)]
        country: Option<String>,
        #[arg(long)]
        asn: Option<u32>,
        /// `production` or `preview`.
        #[arg(long)]
        environment: Option<String>,
    },
    /// Attack mode: every visitor to the project must pass a challenge (search engines' crawlers
    /// and crons pass). Applies at once. API clients can't run a challenge: use rate limits there.
    AttackMode {
        project: String,
        /// How long: 1h, 6h or 24h.
        #[arg(long = "for", default_value = "1h")]
        duration: String,
        /// Turn it off.
        #[arg(long, conflicts_with = "duration")]
        off: bool,
    },
    /// Addresses the system mitigations (per-address connection and request limits, automatic
    /// blocks) leave alone. Applies at once; the custom rules still apply.
    SystemBypass {
        #[command(subcommand)]
        command: BypassCommand,
    },
    /// What applies at once: attack mode, system bypasses, the pause.
    Mitigations {
        #[command(flatten)]
        scope: Scope,
    },
    /// Turn the install's system mitigations off for 24 hours (owners). For a false alarm only:
    /// nothing then stops one address from taking every connection.
    Pause,
    /// Turn the system mitigations back on.
    Resume,
    /// The system mitigations' limits on each address, for the whole install. Without options,
    /// shows them; changing them is an owner's, and applies at once on every router.
    Limits {
        /// Connections open at once from one address (16 to 65,536; default 256).
        #[arg(long)]
        connections: Option<u32>,
        /// Requests from one address in 10 seconds, over every project, before it's blocked (100
        /// to 1,000,000; default 5,000).
        #[arg(long)]
        requests: Option<u32>,
        /// How long a block lasts: 90s, 10m, 1h (a minute to a day; default 10m).
        #[arg(long)]
        block: Option<String>,
        /// Every limit back to the routers' own first.
        #[arg(long)]
        reset: bool,
    },
    /// Replace the whole configuration with a JSON file (Vercel's export reads as it is). A draft
    /// until `publish`.
    Import {
        #[command(flatten)]
        scope: Scope,
        file: PathBuf,
    },
}

// Parsed once per run: the size of `Add` doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum RuleCommand {
    /// Add a rule at the end. A draft until `publish`.
    Add {
        #[command(flatten)]
        scope: Scope,
        #[arg(long)]
        name: String,
        #[arg(long)]
        description: Option<String>,
        /// The first condition of the first group.
        #[arg(long = "if", value_name = "CONDITION", required_unless_present = "json")]
        first: Option<String>,
        /// Another condition in the same group (all must hold). Comes after the condition it joins.
        #[arg(long = "and", value_name = "CONDITION")]
        and: Vec<String>,
        /// The first condition of another group (any group may hold).
        #[arg(long = "or", value_name = "CONDITION")]
        or: Vec<String>,
        #[arg(long, value_enum, required_unless_present = "json")]
        then: Option<Then>,
        /// With deny, challenge, or a rate limit: once a client is refused, hold its address to that
        /// answer this long (1m-1h).
        #[arg(long = "for", value_name = "DURATION")]
        hold: Option<String>,
        /// With rate_limit: requests allowed per window.
        #[arg(long, alias = "limit", value_name = "N")]
        requests: Option<u64>,
        /// With rate_limit: the window, in seconds (10-3600).
        #[arg(long, value_name = "SECONDS", default_value_t = 60)]
        window: u64,
        /// With rate_limit: what makes requests one client's (ip, header:<name>, user_agent, asn,
        /// cookie:<name>, query:<name>, path, host, country); repeat for one bucket per combination.
        #[arg(long = "key", value_name = "KEY")]
        keys: Vec<String>,
        /// With rate_limit: how requests are counted.
        #[arg(long, value_enum, default_value = "fixed_window")]
        algo: Algo,
        /// With rate_limit: what a request over the limit gets.
        #[arg(long, value_enum, default_value = "rate_limit")]
        over: Over,
        /// With redirect: where to (a path, or a URL).
        #[arg(long)]
        location: Option<String>,
        /// With redirect: 301 instead of 307.
        #[arg(long)]
        permanent: bool,
        /// Add it turned off.
        #[arg(long)]
        off: bool,
        /// The rule as JSON instead (Vercel's shape); the other options are ignored.
        #[arg(long, conflicts_with_all = ["first", "then"])]
        json: Option<String>,
    },
    /// Remove a rule (by id or name).
    Rm {
        #[command(flatten)]
        scope: Scope,
        rule: String,
    },
    /// Turn a rule on.
    On {
        #[command(flatten)]
        scope: Scope,
        rule: String,
    },
    /// Turn a rule off.
    Off {
        #[command(flatten)]
        scope: Scope,
        rule: String,
    },
    /// Move a rule to POSITION (1 is first).
    Move {
        #[command(flatten)]
        scope: Scope,
        rule: String,
        position: usize,
    },
}

#[derive(Subcommand)]
pub enum IpCommand {
    /// Block (or log, or let past the custom rules) an address or range. A draft until `publish`.
    Add {
        #[command(flatten)]
        scope: Scope,
        ip: String,
        /// One host, `*.example.com`, or `*` for every host.
        #[arg(long, default_value = "*")]
        host: String,
        #[arg(long, value_enum, default_value = "deny")]
        action: IpThen,
        #[arg(long)]
        note: Option<String>,
    },
    /// Remove an IP block (by id or address).
    Rm {
        #[command(flatten)]
        scope: Scope,
        ip: String,
    },
}

#[derive(Subcommand)]
pub enum BypassCommand {
    /// Leave an address or range alone, on one host, `*.example.com`, or every host (`*`).
    Add {
        #[command(flatten)]
        scope: Scope,
        ip: String,
        #[arg(long, default_value = "*")]
        domain: String,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Remove one, by id or address.
    Rm {
        #[command(flatten)]
        scope: Scope,
        id: String,
    },
}

/// `[not] TYPE[:KEY] OP [VALUE]` as a condition.
pub fn parse_condition(text: &str) -> Result<Value> {
    let mut words = text.split_whitespace().peekable();
    let neg = words.peek() == Some(&"not");
    if neg {
        words.next();
    }
    let field = words.next().with_context(|| format!("{text:?}: give TYPE OP VALUE, like `path pre /admin`"))?;
    let op = words.next().with_context(|| format!("{text:?}: no operator (eq, neq, pre, suf, sub, re, inc, ninc, ex, nex, gt, gte, lt, lte, list)"))?;
    let value: Vec<&str> = words.collect();
    let value = value.join(" ");
    let (kind, key) = match field.split_once(':') {
        Some((k, key)) => (k, Some(key)),
        None => (field, None),
    };
    let kind = match kind {
        "ip" => "ip_address",
        "ua" | "agent" => "user_agent",
        "country" => "geo_country",
        "continent" => "geo_continent",
        "region_name" | "state" => "geo_country_region",
        "city" => "geo_city",
        "asn" => "geo_as_number",
        "env" => "environment",
        "ja4" => "ja4_digest",
        other => other,
    };
    let mut c = json!({"type": kind, "op": op});
    if neg {
        c["neg"] = json!(true);
    }
    if let Some(key) = key {
        c["key"] = json!(key);
    }
    match op {
        "ex" | "nex" => {}
        _ if value.is_empty() => bail!("{text:?}: {op} needs a value"),
        "inc" | "ninc" => c["value"] = json!(value.split(',').map(str::trim).filter(|v| !v.is_empty()).collect::<Vec<_>>()),
        _ => c["value"] = json!(value),
    }
    Ok(c)
}

/// The groups `--if`, `--and` and `--or` make, in the order they were given on the command line.
fn groups(first: &str, and: &[String], or: &[String]) -> Result<Vec<Value>> {
    // clap gives each option's values apart; the order between them comes from the arguments.
    let args: Vec<String> = std::env::args().collect();
    let mut order: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (a, None),
        };
        if matches!(flag, "--if" | "--and" | "--or") {
            let value = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    args.get(i).cloned().unwrap_or_default()
                }
            };
            order.push((flag.trim_start_matches("--").to_owned(), value));
        }
        i += 1;
    }
    // Run from a test, or a caller that didn't come through the command line: --and joins the
    // first group, each --or starts one.
    if order.is_empty() {
        order.push(("if".into(), first.to_owned()));
        order.extend(and.iter().map(|a| ("and".into(), a.clone())));
        order.extend(or.iter().map(|o| ("or".into(), o.clone())));
    }
    let mut out: Vec<Vec<Value>> = Vec::new();
    for (flag, text) in order {
        let c = parse_condition(&text)?;
        match flag.as_str() {
            "and" if !out.is_empty() => out.last_mut().unwrap().push(c),
            "and" => bail!("--and comes after the --if or --or it joins"),
            _ => out.push(vec![c]),
        }
    }
    Ok(out.into_iter().map(|cs| json!({"conditions": cs})).collect())
}

async fn send(via: &Via<'_>, scope: &Scope, op: Value) -> Result<Value> {
    let parsed: FirewallOp = serde_json::from_value(op.clone()).context("an op the CLI made")?;
    let changes = parsed.changes();
    match via {
        Via::Aws(ctx) => fw::run(ctx, scope.name(), parsed, None).await,
        Via::Api(client) => {
            let mut body = op;
            if let Some(p) = scope.name() {
                body["project"] = json!(p);
            }
            if changes { client.post("firewall", body).await } else { client.ask("firewall", body).await }
        }
    }
}

async fn change(via: &Via<'_>, scope: &Scope, changes: Value) -> Result<()> {
    let answer = send(via, scope, json!({"op": "change", "changes": changes})).await?;
    eprintln!("==> {}: draft {} (publish with `forkrail firewall publish {}`)", scope.label(), answer["staging"].as_str().unwrap_or("?"), scope_args(scope));
    Ok(())
}

fn print_mitigations(v: &Value) {
    let when = |key: &str| v[key].as_u64().map(|t| chrono::DateTime::from_timestamp(t as i64, 0).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default());
    match v["project"].as_str() {
        Some(p) => match when("attack_mode_until") {
            Some(until) => println!("{p}: attack mode on until {until}"),
            None => println!("{p}: attack mode off"),
        },
        None => match when("paused_until") {
            Some(until) => println!("system mitigations PAUSED until {until}"),
            None => println!("system mitigations on"),
        },
    }
    if let Some(by) = v["changed_by"].as_str() {
        println!("  last changed by {by} at {}", v["changed_at"].as_str().unwrap_or("?"));
    }
    let bypass = v["system_bypass"].as_array().cloned().unwrap_or_default();
    if bypass.is_empty() {
        println!("  no system bypasses");
    }
    for b in bypass {
        let note = b["note"].as_str().filter(|n| !n.is_empty()).map(|n| format!("  ({n})")).unwrap_or_default();
        println!("  bypass {} on {} [{}]{note}", b["source_ip"].as_str().unwrap_or(""), b["domain"].as_str().unwrap_or("*"), b["id"].as_str().unwrap_or(""));
    }
}

fn print_limits(l: &Value) {
    let n = |k: &str| l[k].as_u64().unwrap_or(0);
    let default = |k: &str| if l[k] == l["defaults"][k] { " (default)" } else { "" };
    println!("per address, over every project{}:", if l["custom"].as_bool() == Some(true) { "" } else { ", the routers' own" });
    println!("  {} open connections{}", n("connections"), default("connections"));
    println!("  {} requests in {} s{}", n("requests"), n("window_s"), default("requests"));
    let block = n("block_s");
    let block = if block % 3600 == 0 { format!("{} h", block / 3600) } else if block % 60 == 0 { format!("{} min", block / 60) } else { format!("{block} s") };
    println!("  then blocked for {block}{}", default("block_s"));
}

/// `90s`, `10m`, `1h`, `1d` (or plain seconds) in seconds.
fn seconds(text: &str) -> Result<u32> {
    let t = text.trim();
    let (digits, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len()));
    let n: u32 = digits.parse().with_context(|| format!("{text:?}: a number and s, m, h or d"))?;
    let times = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("{text:?}: a number and s, m, h or d"),
    };
    n.checked_mul(times).with_context(|| format!("{text:?} is too long"))
}

fn scope_args(scope: &Scope) -> String {
    match scope.name() {
        Some(p) => p.to_owned(),
        None => "--install".to_owned(),
    }
}

fn condition_text(c: &Value) -> String {
    let mut s = String::new();
    if c["neg"].as_bool() == Some(true) {
        s.push_str("not ");
    }
    s.push_str(c["type"].as_str().unwrap_or("?"));
    if let Some(k) = c["key"].as_str() {
        s.push(':');
        s.push_str(k);
    }
    s.push(' ');
    s.push_str(c["op"].as_str().unwrap_or("?"));
    match &c["value"] {
        Value::Null => {}
        Value::Array(list) => s.push_str(&format!(" {}", list.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(","))),
        Value::String(v) => s.push_str(&format!(" {v}")),
        other => s.push_str(&format!(" {other}")),
    }
    s
}

fn print_config(config: &Value) {
    if config["firewallEnabled"].as_bool() == Some(false) {
        println!("custom rules: off");
    }
    let rules = config["rules"].as_array().cloned().unwrap_or_default();
    if rules.is_empty() {
        println!("no custom rules");
    }
    for (i, r) in rules.iter().enumerate() {
        let m = &r["action"]["mitigate"];
        let mut then = m["action"].as_str().unwrap_or("?").to_owned();
        if let Some(r) = m["rateLimit"].as_object().filter(|_| then == "rate_limit") {
            let keys: Vec<&str> = r.get("keys").and_then(Value::as_array).map(|k| k.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let over = r.get("action").and_then(Value::as_str).unwrap_or("rate_limit");
            then = format!(
                "rate limit {} per {} s by {} ({}), then {}",
                r.get("limit").and_then(Value::as_u64).unwrap_or(0),
                r.get("window").and_then(Value::as_u64).unwrap_or(0),
                keys.join(" + "),
                r.get("algo").and_then(Value::as_str).unwrap_or("?").replace('_', " "),
                if over == "rate_limit" { "429" } else { over }
            );
        }
        if let Some(d) = m["actionDuration"].as_str() {
            then.push_str(&format!(" for {d}"));
        }
        if let Some(l) = m["redirect"]["location"].as_str() {
            then.push_str(&format!(" to {l}{}", if m["redirect"]["permanent"].as_bool() == Some(true) { " (301)" } else { "" }));
        }
        let off = if r["active"].as_bool() == Some(false) { " (off)" } else { "" };
        println!("{:>3}. {} [{}]{off}: {then}", i + 1, r["name"].as_str().unwrap_or(""), r["id"].as_str().unwrap_or(""));
        for (g, group) in r["conditionGroup"].as_array().cloned().unwrap_or_default().iter().enumerate() {
            let conds: Vec<String> = group["conditions"].as_array().cloned().unwrap_or_default().iter().map(condition_text).collect();
            println!("       {} {}", if g == 0 { "if" } else { "or" }, conds.join(" and "));
        }
    }
    let ips = config["ips"].as_array().cloned().unwrap_or_default();
    if !ips.is_empty() {
        println!("IP blocks:");
        for ip in ips {
            let note = ip["notes"].as_str().filter(|n| !n.is_empty()).map(|n| format!("  ({n})")).unwrap_or_default();
            println!("  {} {} on {} [{}]{note}", ip["action"].as_str().unwrap_or("?"), ip["ip"].as_str().unwrap_or("?"), ip["hostname"].as_str().unwrap_or("?"), ip["id"].as_str().unwrap_or(""));
        }
    }
    if let Some(lists) = config["lists"].as_object() {
        for (name, entries) in lists {
            println!("list {name}: {} addresses", entries.as_array().map_or(0, |a| a.len()));
        }
    }
}

fn print_diff(changes: &Value) {
    let list = changes.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("no changes");
    }
    for c in list {
        let what = match c["kind"].as_str() {
            Some("rule") => "rule",
            Some("ip") => "IP block",
            Some("system_bypass") => "system bypass",
            Some("list") => "list",
            _ => "custom rules",
        };
        let label = c["label"].as_str().unwrap_or("");
        match c["change"].as_str() {
            Some("moved") => println!("~ {what} {label}: from {} to {}", c["from"], c["to"]),
            Some("added") => println!("+ {what} {label}"),
            Some("removed") => println!("- {what} {label}"),
            Some("changed") => println!("~ {what} {label}"),
            Some(on) => println!("~ {what} {on}"),
            None => {}
        }
    }
}

pub async fn run(via: Via<'_>, command: FirewallCommand) -> Result<()> {
    match command {
        FirewallCommand::Show { scope, version, json } => {
            let answer = send(&via, &scope, json!({"op": "get", "version": version})).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&answer["config"])?);
                return Ok(());
            }
            let live = answer["live"].as_str().unwrap_or("none");
            match answer["staging"].as_str() {
                Some(draft) => eprintln!("==> {}: showing {}; live {live}, draft {draft} ({} changes)", scope.label(), answer["version"].as_str().unwrap_or("-"), answer["pending"].as_array().map_or(0, |a| a.len())),
                None => eprintln!("==> {}: live {live}", scope.label()),
            }
            print_config(&answer["config"]);
        }
        FirewallCommand::Rule { command } => match command {
            RuleCommand::Add { scope, name, description, first, and, or, then, hold, requests, window, keys, algo, over, location, permanent, off, json: raw } => {
                let rule = match raw {
                    Some(text) => {
                        let mut v: Value = serde_json::from_str(&text).context("--json isn't JSON")?;
                        if v.get("name").is_none() {
                            v["name"] = json!(name);
                        }
                        v
                    }
                    None => {
                        let (Some(first), Some(then)) = (first, then) else { bail!("give --if and --then, or --json") };
                        let mut mitigate = json!({"action": match then {
                            Then::Deny => "deny",
                            Then::Challenge => "challenge",
                            Then::Log => "log",
                            Then::Bypass => "bypass",
                            Then::Redirect => "redirect",
                            Then::RateLimit => "rate_limit",
                        }});
                        if let Some(d) = hold {
                            mitigate["actionDuration"] = json!(d);
                        }
                        if let Then::RateLimit = then {
                            let limit = requests.context("a rate limit needs --requests (per --window seconds)")?;
                            let keys = if keys.is_empty() { vec!["ip".to_owned()] } else { keys };
                            mitigate["rateLimit"] = json!({
                                "algo": match algo { Algo::FixedWindow => "fixed_window", Algo::TokenBucket => "token_bucket" },
                                "window": window,
                                "limit": limit,
                                "keys": keys,
                                "action": match over { Over::RateLimit => "rate_limit", Over::Deny => "deny", Over::Challenge => "challenge", Over::Log => "log" },
                            });
                        }
                        if let Then::Redirect = then {
                            let location = location.context("a redirect needs --location")?;
                            mitigate["redirect"] = json!({"location": location, "permanent": permanent});
                        }
                        json!({
                            "name": name,
                            "description": description.unwrap_or_default(),
                            "active": !off,
                            "conditionGroup": groups(&first, &and, &or)?,
                            "action": {"mitigate": mitigate},
                        })
                    }
                };
                change(&via, &scope, json!([{"action": "rules.insert", "value": rule}])).await?;
            }
            RuleCommand::Rm { scope, rule } => change(&via, &scope, json!([{"action": "rules.remove", "id": rule}])).await?,
            RuleCommand::On { scope, rule } => change(&via, &scope, json!([{"action": "rules.active", "id": rule, "value": true}])).await?,
            RuleCommand::Off { scope, rule } => change(&via, &scope, json!([{"action": "rules.active", "id": rule, "value": false}])).await?,
            RuleCommand::Move { scope, rule, position } => {
                if position == 0 {
                    bail!("positions start at 1");
                }
                change(&via, &scope, json!([{"action": "rules.priority", "id": rule, "value": position - 1}])).await?
            }
        },
        FirewallCommand::Ip { command } => match command {
            IpCommand::Add { scope, ip, host, action, note } => {
                let action = match action {
                    IpThen::Deny => "deny",
                    IpThen::Challenge => "challenge",
                    IpThen::Log => "log",
                    IpThen::Bypass => "bypass",
                };
                change(&via, &scope, json!([{"action": "ip.insert", "value": {"ip": ip, "hostname": host, "action": action, "notes": note.unwrap_or_default()}}])).await?
            }
            IpCommand::Rm { scope, ip } => change(&via, &scope, json!([{"action": "ip.remove", "id": ip}])).await?,
        },
        FirewallCommand::Enable { scope } => change(&via, &scope, json!([{"action": "firewallEnabled", "value": true}])).await?,
        FirewallCommand::Disable { scope } => change(&via, &scope, json!([{"action": "firewallEnabled", "value": false}])).await?,
        FirewallCommand::Diff { scope } => {
            let answer = send(&via, &scope, json!({"op": "diff"})).await?;
            eprintln!("==> {}: {} → {}", scope.label(), answer["from"].as_str().unwrap_or("nothing"), answer["to"].as_str().unwrap_or("nothing"));
            print_diff(&answer["changes"]);
        }
        FirewallCommand::Publish { scope, version } => {
            let answer = send(&via, &scope, json!({"op": "publish", "version": version})).await?;
            eprintln!("==> {}: {} is live ({} rules, {} IP blocks)", scope.label(), answer["live"].as_str().unwrap_or("?"), answer["rules"], answer["ips"]);
        }
        FirewallCommand::Discard { scope } => {
            send(&via, &scope, json!({"op": "discard"})).await?;
            eprintln!("==> {}: draft dropped", scope.label());
        }
        FirewallCommand::Rollback { scope, version } => {
            let answer = send(&via, &scope, json!({"op": "restore", "version": version})).await?;
            eprintln!("==> {}: {} is live again", scope.label(), answer["live"].as_str().unwrap_or("?"));
        }
        FirewallCommand::Versions { scope } => {
            let answer = send(&via, &scope, json!({"op": "versions"})).await?;
            for v in answer["versions"].as_array().cloned().unwrap_or_default() {
                let tag = if v["live"].as_bool() == Some(true) {
                    " (live)"
                } else if v["staging"].as_bool() == Some(true) {
                    " (draft)"
                } else {
                    ""
                };
                println!(
                    "{}{tag}  {}  {}  {}",
                    v["id"].as_str().unwrap_or(""),
                    v["created"].as_str().unwrap_or(""),
                    v["created_by"].as_str().unwrap_or("-"),
                    v["note"].as_str().unwrap_or("")
                );
            }
        }
        FirewallCommand::Test { scope, url, method, headers, ip, country, asn, environment } => {
            let mut map = BTreeMap::new();
            for h in headers {
                let (k, v) = h.split_once(':').with_context(|| format!("header {h:?}: give `Name: value`"))?;
                map.insert(k.trim().to_owned(), v.trim().to_owned());
            }
            let answer = send(&via, &scope, json!({"op": "test", "url": url, "method": method, "headers": map, "ip": ip, "country": country, "asn": asn, "environment": environment})).await?;
            let by = answer["name"].as_str().map(|n| format!(" by {n:?}")).unwrap_or_default();
            let status = answer["answer"]["status"].as_u64().map(|s| format!(": {s}")).unwrap_or_default();
            let to = answer["answer"]["location"].as_str().map(|l| format!(" to {l}")).unwrap_or_default();
            let hold = answer["answer"]["hold_s"].as_u64().map(|s| format!(", address held off {s} s")).unwrap_or_default();
            println!("{}{by}{status}{to}{hold}", answer["action"].as_str().unwrap_or("?"));
            for r in answer["counted"].as_array().cloned().unwrap_or_default() {
                println!("counted by rate limit {:?} ({} per {} s)", r["name"].as_str().unwrap_or(""), r["limit"], r["window_s"]);
            }
        }
        FirewallCommand::AttackMode { project, duration, off } => {
            let scope = Scope { project: Some(project), install: false };
            let v = send(&via, &scope, json!({"op": "attack_mode", "duration": duration, "off": off})).await?;
            print_mitigations(&v);
        }
        FirewallCommand::SystemBypass { command } => {
            let (scope, op) = match command {
                BypassCommand::Add { scope, ip, domain, note } => (scope, json!({"op": "system_bypass_add", "source_ip": ip, "domain": domain, "note": note})),
                BypassCommand::Rm { scope, id } => (scope, json!({"op": "system_bypass_remove", "id": id})),
            };
            print_mitigations(&send(&via, &scope, op).await?);
        }
        FirewallCommand::Mitigations { scope } => print_mitigations(&send(&via, &scope, json!({"op": "mitigations"})).await?),
        FirewallCommand::Pause => print_mitigations(&send(&via, &Scope { project: None, install: true }, json!({"op": "pause"})).await?),
        FirewallCommand::Resume => print_mitigations(&send(&via, &Scope { project: None, install: true }, json!({"op": "resume"})).await?),
        FirewallCommand::Limits { connections, requests, block, reset } => {
            let install = Scope { project: None, install: true };
            let block_s = block.as_deref().map(seconds).transpose()?;
            let change = connections.is_some() || requests.is_some() || block_s.is_some() || reset;
            let request = if change { json!({"op": "limits", "connections": connections, "requests": requests, "block_s": block_s, "reset": reset}) } else { json!({"op": "mitigations"}) };
            print_limits(&send(&via, &install, request).await?["limits"]);
        }
        FirewallCommand::Import { scope, file } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let mut config: Value = serde_json::from_str(&text).with_context(|| format!("{} isn't JSON", file.display()))?;
            // Vercel's GET answers wrap the configuration in `active`.
            if let Some(inner) = config.get("active").cloned().filter(Value::is_object) {
                config = inner;
            }
            let answer = send(&via, &scope, json!({"op": "replace", "config": config})).await?;
            eprintln!("==> {}: draft {} (see it with `forkrail firewall diff {}`)", scope.label(), answer["staging"].as_str().unwrap_or("?"), scope_args(&scope));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_from_the_command_line() {
        assert_eq!(parse_condition("path pre /admin").unwrap(), json!({"type": "path", "op": "pre", "value": "/admin"}));
        assert_eq!(parse_condition("not country inc US, CA").unwrap(), json!({"type": "geo_country", "op": "inc", "neg": true, "value": ["US", "CA"]}));
        assert_eq!(parse_condition("header:X-Api-Key ex").unwrap(), json!({"type": "header", "op": "ex", "key": "X-Api-Key"}));
        assert_eq!(parse_condition("ua sub Go-http-client").unwrap(), json!({"type": "user_agent", "op": "sub", "value": "Go-http-client"}));
        assert_eq!(parse_condition("ip eq 192.0.2.0/24").unwrap()["type"], "ip_address");
        assert_eq!(parse_condition("ua re ^python requests").unwrap()["value"], "^python requests");
        assert!(parse_condition("path").is_err());
        assert!(parse_condition("path eq").is_err());
    }

    #[test]
    fn groups_follow_and_and_or() {
        let g = groups("path pre /admin", &["not country eq US".into()], &["method eq TRACE".into()]).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0]["conditions"].as_array().unwrap().len(), 2);
        assert_eq!(g[1]["conditions"][0]["value"], "TRACE");
    }
}
