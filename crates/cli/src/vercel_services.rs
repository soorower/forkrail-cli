//! Code that works only on Vercel: npm packages that sign in to Vercel's own services with the OIDC
//! token Vercel gives its functions (forkrail has none to give), and the Workflow SDK, which picks
//! Vercel's backend because forkrail sets `VERCEL_DEPLOYMENT_ID` as Vercel does. They build and
//! deploy fine and fail at their first call, so a build looks for them in the project's dependencies
//! (`package.json`, and its lockfile for the version installed) beside the names of the project's
//! env vars for the target (never their values), and warns with the fix (docs/deploying.md, "Moving
//! from Vercel").
//!
//! What each package does without its token was read from its published code (the versions named
//! below, 2026-11-16). Packages that work off Vercel as they are get nothing: `@vercel/og`,
//! `@vercel/kv`, `@vercel/postgres`, `flags`, `@vercel/firewall`, `@vercel/otel`,
//! `@vercel/edge-config`, `@vercel/functions` (whose `upgradeWebSocket()` alone fails, which no
//! dependency shows), `@vercel/analytics` and `@vercel/speed-insights` (the router answers them).

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// What a deployment's warnings are about: the project's folder, and the names of its env vars for
/// the target being deployed.
pub fn warnings<'a>(dir: &Path, target: &str, env: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let Some(deps) = Deps::read(dir) else { return Vec::new() };
    let env: BTreeSet<&str> = env.into_iter().map(String::as_str).collect();
    check(&deps, target, &env)
}

fn check(deps: &Deps, target: &str, env: &BTreeSet<&str>) -> Vec<String> {
    let mut warnings = Vec::new();
    let none_for = format!("isn't set for {target}");

    // The AI SDK (`ai` 5 and later, `@ai-sdk/gateway` 4.0.146 read): a model named by a string,
    // or `gateway(…)`, goes to Vercel's AI Gateway, which takes AI_GATEWAY_API_KEY or else Vercel's
    // OIDC token. `ai` 4 and before have no gateway. A project with a model provider of its own
    // (`@ai-sdk/openai` …) is taken to call that one.
    let gateway = deps.direct("@ai-sdk/gateway").map(|_| "@ai-sdk/gateway".to_owned()).or_else(|| {
        let ai = deps.direct("ai")?;
        // The provider first: the lockfile is read only when the version decides.
        (!deps.direct_names().any(model_provider) && deps.at_least("ai", 5)).then(|| format!("ai {ai}"))
    });
    if let Some(package) = gateway
        && !env.contains("AI_GATEWAY_API_KEY")
    {
        warnings.push(format!(
            "{package}: a model named by a string (\"openai/gpt-5\") or by `gateway(…)` goes through Vercel's AI Gateway, which signs in with AI_GATEWAY_API_KEY or else Vercel's OIDC token. forkrail has no OIDC token and AI_GATEWAY_API_KEY {none_for}, so every such call fails (GatewayAuthenticationError). Add AI_GATEWAY_API_KEY (an AI Gateway API key from Vercel's dashboard; Vercel bills its use), or call a model provider directly, such as @ai-sdk/amazon-bedrock with the project's AWS role (`forkrail project NAME --role ROLE`)"
        ));
    }

    // Vercel Blob (`@vercel/blob` 2.9.0 read): `token`, else Vercel's OIDC token with a store id,
    // else BLOB_READ_WRITE_TOKEN. A store connected through OIDC gives the project BLOB_STORE_ID and
    // no token. KV2 (`@vercel/kv2` 0.0.19) is Blob underneath and asks for the same token.
    for package in ["@vercel/blob", "@vercel/kv2"] {
        if deps.direct(package).is_none() || env.contains("BLOB_READ_WRITE_TOKEN") {
            continue;
        }
        let why = if env.contains("BLOB_STORE_ID") {
            "BLOB_STORE_ID without BLOB_READ_WRITE_TOKEN is a Blob store connected through Vercel's OIDC token, which forkrail doesn't have: every Blob call fails (\"No blob credentials found\")".to_owned()
        } else {
            format!("BLOB_READ_WRITE_TOKEN {none_for}, so unless the code passes `token` itself every Blob call fails (\"No blob credentials found\")")
        };
        warnings.push(format!(
            "{package}: {why}. In Vercel's dashboard, open the store (Storage), create a read-write token, and add it here as BLOB_READ_WRITE_TOKEN; the files stay in Vercel Blob, billed by Vercel"
        ));
    }

    // The Workflow SDK (`workflow` 4 and later; 4.0.1-beta.0 and 5.5.0 read): its backend (its
    // "world") is WORKFLOW_TARGET_WORLD, else Vercel's whenever VERCEL_DEPLOYMENT_ID is set, which
    // signs in with Vercel's OIDC token. `workflow` 0.x-2.x is another package (a workspace
    // manager).
    let workflow = deps.direct("workflow").filter(|_| deps.at_least("workflow", 4)).map(|_| "workflow").or_else(|| deps.direct_names().find(|n| n.starts_with("@workflow/")));
    if let Some(package) = workflow
        && !env.contains("WORKFLOW_TARGET_WORLD")
    {
        warnings.push(format!(
            "{package}: the Workflow SDK runs on WORKFLOW_TARGET_WORLD's backend, else on Vercel's whenever VERCEL_DEPLOYMENT_ID is set. forkrail sets VERCEL_DEPLOYMENT_ID (as Vercel does) and WORKFLOW_TARGET_WORLD {none_for}, so it picks Vercel's, which needs Vercel's OIDC token: every workflow fails to start. Set WORKFLOW_TARGET_WORLD=@workflow/world-postgres with WORKFLOW_POSTGRES_URL (or DATABASE_URL) and add @workflow/world-postgres to the dependencies; its worker runs inside the app, so on forkrail queued steps and sleeps go on only while the app has a worker open"
        ));
    }

    // Vercel Sandbox (`@vercel/sandbox` 4.0.0): `token`, `teamId` and `projectId` passed in, else
    // Vercel's OIDC token. Vercel's docs pass them from these three variables; forkrail sets
    // VERCEL_PROJECT_ID to its own project's name, which a project's own value overrides.
    if deps.direct("@vercel/sandbox").is_some() {
        let lacking: Vec<&str> = ["VERCEL_TOKEN", "VERCEL_TEAM_ID", "VERCEL_PROJECT_ID"].into_iter().filter(|n| !env.contains(n)).collect();
        if !lacking.is_empty() {
            warnings.push(format!(
                "@vercel/sandbox: without `token`, `teamId` and `projectId` passed to it, it signs in with Vercel's OIDC token, which forkrail doesn't have, and fails; {} {} for {target}. Add VERCEL_TOKEN (a Vercel access token), VERCEL_TEAM_ID and VERCEL_PROJECT_ID (your Vercel ids: forkrail's own VERCEL_PROJECT_ID is its project's name) and pass them to `Sandbox.create`",
                match lacking.split_last() {
                    Some((last, [])) => (*last).to_owned(),
                    Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
                    None => unreachable!("not empty"),
                },
                if lacking.len() == 1 { "isn't set" } else { "aren't set" }
            ));
        }
    }

    // Vercel Flags (`@vercel/flags-core` 1.11.0, `@flags-sdk/vercel` 1.6.0): the FLAGS SDK key, else
    // Vercel's OIDC token. (The Flags SDK itself, `flags`, needs neither.)
    if let Some(package) = ["@vercel/flags-core", "@flags-sdk/vercel"].into_iter().find(|p| deps.direct(p).is_some())
        && !env.contains("FLAGS")
    {
        warnings.push(format!(
            "{package}: Vercel Flags reads flags with the FLAGS SDK key, else Vercel's OIDC token, which forkrail doesn't have, and FLAGS {none_for}: flags fall back to their defaults or fail. Add FLAGS (the project's SDK key, vf_server_…, from Vercel's dashboard: Flags → SDK Keys)"
        ));
    }

    // Services that sign in with Vercel's OIDC token in production (each read, at the version
    // named): nothing on forkrail lets them work as they are.
    for (package, what) in OIDC_ONLY {
        if deps.direct(package).is_some() {
            warnings.push(format!("{package}: {what}"));
        }
    }
    warnings
}

/// Packages that need Vercel's OIDC token (or, for Passport, a header Vercel's proxy adds) in
/// production, with what fails on forkrail and what to use instead.
const OIDC_ONLY: [(&str, &str); 8] = [
    (
        "@vercel/queue",
        "Vercel Queues (0.11.0) signs in with Vercel's OIDC token unless the code passes its own `token` (VERCEL_QUEUE_TOKEN is for `vercel dev` only), and only Vercel delivers messages to the consumers vercel.json's queue triggers name: on forkrail sending fails at its first call and nothing is delivered. Use a queue in your AWS account, such as SQS, with the project's AWS role (`forkrail project NAME --role ROLE`)",
    ),
    (
        "@vercel/schedules",
        "Vercel's Schedules SDK (0.0.3) signs in with Vercel's OIDC token (VERCEL_SCHEDULE_TOKEN is for `vercel dev` only), which forkrail doesn't have: it fails at its first call. Use vercel.json's `crons`, which forkrail runs",
    ),
    (
        "@vercel/connect",
        "Vercel Connect (2.9.3) gets other services' tokens by signing in with Vercel's OIDC token unless the code passes `vercelToken`: on forkrail it fails at its first call. Sign in to those services with their own keys or OAuth apps, kept as env vars",
    ),
    (
        "@vercel/kms",
        "Vercel KMS (0.3.12) signs in with Vercel's OIDC token, which forkrail doesn't have: every signing call fails. Use AWS KMS in your account with the project's AWS role (`forkrail project NAME --role ROLE`)",
    ),
    (
        "@vercel/passport",
        "Vercel Passport (1.2.0) reads who the visitor is from a header Vercel's proxy adds after its own sign-in, checked against Vercel's keys: forkrail has no Passport, so every visitor reads as signed out. Sign visitors in within the app (an auth library or your identity provider), or keep a site to your team with forkrail's deployment protection (`forkrail protection`)",
    ),
    (
        "@vercel/integrations",
        "getIntegrationToken (0.1.5) signs in with Vercel's OIDC token unless the code passes `vercelToken`: on forkrail it fails at its first call. Use the integration's own credentials (its env vars from the Vercel project)",
    ),
    (
        "@vercel/oidc-aws-credentials-provider",
        "it (3.3.21) trades Vercel's OIDC token for AWS credentials, and forkrail has no such token, so every AWS call made with it fails. Drop it and give the project an IAM role (`forkrail project NAME --role ROLE`): the AWS SDK's default credentials pick it up, with no keys",
    ),
    (
        "@vercel/oidc",
        "getVercelOidcToken() (4.0.9) reads Vercel's OIDC token, which Vercel makes for each of its functions and forkrail can't, so what the app signs in to with it fails. For AWS, give the project an IAM role (`forkrail project NAME --role ROLE`); for Vercel's own services, their own keys (AI_GATEWAY_API_KEY, BLOB_READ_WRITE_TOKEN, FLAGS). Never copy VERCEL_OIDC_TOKEN from `vercel env pull`: it is a 12-hour development token",
    ),
];

/// A model provider for the AI SDK, which then calls that provider directly: Vercel's own
/// `@ai-sdk/*` packages that aren't one are named, so a new provider counts as one.
fn model_provider(name: &str) -> bool {
    const NOT_PROVIDERS: [&str; 18] = [
        "gateway", "provider", "provider-utils", "react", "ui-utils", "rsc", "svelte", "vue", "angular", "solid", "valibot", "otel", "devtools", "mcp", "langchain", "llamaindex", "test-server", "codemod",
    ];
    match name.strip_prefix("@ai-sdk/") {
        Some(rest) => !NOT_PROVIDERS.contains(&rest),
        None => name.contains("ai-sdk-provider") || name.ends_with("-ai-provider") || name.contains("-ai-provider-"),
    }
}

/// The project's npm dependencies: what `package.json` asks for, and (read when a version matters)
/// what its lockfile installs.
struct Deps {
    dir: PathBuf,
    /// `dependencies`, `devDependencies` and `optionalDependencies` by name, with the range asked for.
    direct: BTreeMap<String, String>,
    locked: OnceCell<Option<Locked>>,
}

/// A lockfile's packages: the versions installed under each name.
type Locked = BTreeMap<String, BTreeSet<String>>;

impl Deps {
    fn read(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
        let package: Value = serde_json::from_str(&text).ok()?;
        let mut direct = BTreeMap::new();
        for key in ["dependencies", "devDependencies", "optionalDependencies"] {
            for (name, range) in package.get(key).and_then(Value::as_object).into_iter().flatten() {
                direct.entry(name.clone()).or_insert_with(|| range.as_str().unwrap_or_default().to_owned());
            }
        }
        Some(Self { dir: dir.to_path_buf(), direct, locked: OnceCell::new() })
    }

    /// The range `package.json` asks for, if it names the package.
    fn direct(&self, name: &str) -> Option<&str> {
        self.direct.get(name).map(String::as_str)
    }

    fn direct_names(&self) -> impl Iterator<Item = &str> {
        self.direct.keys().map(String::as_str)
    }

    /// Whether the package as installed is version `major` or later: the newest the lockfile holds,
    /// else what `package.json`'s range takes (one it can't read, such as `latest`, is the newest).
    fn at_least(&self, name: &str, major: u64) -> bool {
        let locked = self.locked.get_or_init(|| locked(&self.dir));
        let installed = locked.as_ref().and_then(|l| l.get(name)).and_then(|versions| versions.iter().filter_map(|v| leading_number(v)).max());
        match installed.or_else(|| self.direct(name).and_then(range_major)) {
            Some(found) => found >= major,
            None => true,
        }
    }
}

/// The highest major version a `package.json` range takes, where it says (`^5.1.0`, `~5`, `5.x`,
/// `<6`, `^4 || ^5`); None for one with no ceiling or no version (`>=4`, `*`, `latest`, `workspace:*`).
fn range_major(range: &str) -> Option<u64> {
    let range = range.trim();
    // An alias (`npm:ai@^5`) asks for what follows its last `@`.
    let range = match range.strip_prefix("npm:") {
        Some(alias) => alias.rsplit_once('@').map_or(alias, |(_, r)| r),
        None => range,
    };
    let mut highest = None;
    for part in range.split("||") {
        let part = part.trim();
        if part.starts_with('>') {
            return None;
        }
        // A hyphen range's ceiling is its second version.
        let part = part.split(" - ").last().unwrap_or(part);
        let n = leading_number(part.trim_start_matches(|c: char| !c.is_ascii_digit()))?;
        let n = if part.starts_with('<') && !part.starts_with("<=") { n.checked_sub(1)? } else { n };
        highest = highest.max(Some(n));
    }
    highest
}

fn leading_number(text: &str) -> Option<u64> {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The packages the project's lockfile installs: npm's, pnpm's, Yarn's (1 and Berry) or Bun's text
/// one. None without one (or with Bun's binary `bun.lockb`).
fn locked(dir: &Path) -> Option<Locked> {
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    if let Some(text) = read("package-lock.json").or_else(|| read("npm-shrinkwrap.json")) {
        return npm_lock(&text);
    }
    if let Some(text) = read("pnpm-lock.yaml") {
        return Some(pnpm_lock(&text));
    }
    if let Some(text) = read("yarn.lock") {
        return Some(yarn_lock(&text));
    }
    read("bun.lock").map(|text| bun_lock(&text))
}

fn add(locked: &mut Locked, name: &str, version: &str) {
    if !name.is_empty() && !version.is_empty() {
        locked.entry(name.to_owned()).or_default().insert(version.to_owned());
    }
}

/// `package-lock.json`: v2/v3's `packages` by path (`node_modules/a/node_modules/b`), else v1's
/// nested `dependencies`.
fn npm_lock(text: &str) -> Option<Locked> {
    let lock: Value = serde_json::from_str(text).ok()?;
    let mut locked = Locked::new();
    if let Some(packages) = lock.get("packages").and_then(Value::as_object) {
        for (path, package) in packages {
            if let (Some((_, name)), Some(version)) = (path.rsplit_once("node_modules/"), package.get("version").and_then(Value::as_str)) {
                add(&mut locked, name, version);
            }
        }
    } else {
        fn walk(deps: &Value, locked: &mut Locked) {
            for (name, package) in deps.as_object().into_iter().flatten() {
                add(locked, name, package.get("version").and_then(Value::as_str).unwrap_or_default());
                if let Some(nested) = package.get("dependencies") {
                    walk(nested, locked);
                }
            }
        }
        walk(lock.get("dependencies").unwrap_or(&Value::Null), &mut locked);
    }
    Some(locked)
}

/// `pnpm-lock.yaml`: the keys of `packages:` (and v9's `snapshots:`), `/name/1.2.3_peer` before
/// lockfile v6, `/name@1.2.3(peer)` in v6 and `name@1.2.3(peer)` in v9, scoped names quoted.
fn pnpm_lock(text: &str) -> Locked {
    let v5 = text.lines().find_map(|l| l.strip_prefix("lockfileVersion:")).is_some_and(|v| v.trim().trim_matches(['\'', '"']).starts_with('5'));
    let mut locked = Locked::new();
    let mut in_packages = false;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if !line.starts_with(' ') {
            in_packages = matches!(line.trim_end(), "packages:" | "snapshots:");
            continue;
        }
        let Some(key) = line.strip_prefix("  ").filter(|k| in_packages && !k.starts_with(' ')).and_then(|k| k.trim_end().strip_suffix(':')) else {
            continue;
        };
        let key = key.trim_matches(['\'', '"']);
        let key = key.strip_prefix('/').unwrap_or(key);
        let key = key.split('(').next().unwrap_or(key);
        let split = if v5 {
            key.rsplit_once('/').map(|(name, version)| (name, version.split('_').next().unwrap_or(version)))
        } else {
            key.get(1..).and_then(|rest| rest.find('@')).map(|at| (&key[..=at], &key[at + 2..]))
        };
        if let Some((name, version)) = split {
            add(&mut locked, name, version);
        }
    }
    locked
}

/// `yarn.lock`: an entry's first descriptor (`"ai@^5.0.0", ai@^5.0.1:` in Yarn 1,
/// `"ai@npm:^5.0.0":` in Berry), then its `version` line.
fn yarn_lock(text: &str) -> Locked {
    let mut locked = Locked::new();
    let mut entry: Option<String> = None;
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            let first = line.trim_end().trim_end_matches(':').split(", ").next().unwrap_or_default().trim_matches('"');
            entry = first.get(1..).and_then(|rest| rest.find('@')).map(|at| first[..=at].to_owned());
            continue;
        }
        if let (Some(name), Some(rest)) = (&entry, line.strip_prefix("  ").filter(|l| !l.starts_with(' ')).and_then(|l| l.strip_prefix("version"))) {
            add(&mut locked, name, rest.trim_start_matches(':').trim().trim_matches('"'));
            entry = None;
        }
    }
    locked
}

/// `bun.lock`: each line of `"packages"` is `"key": ["name@version", …]`.
fn bun_lock(text: &str) -> Locked {
    let mut locked = Locked::new();
    let mut in_packages = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("\"packages\"") {
            in_packages = true;
            continue;
        }
        if !in_packages {
            continue;
        }
        let Some((_, value)) = line.split_once("\": [\"") else { continue };
        let Some(spec) = value.split('"').next() else { continue };
        if let Some(at) = spec.get(1..).and_then(|rest| rest.find('@')) {
            add(&mut locked, &spec[..=at], &spec[at + 2..]);
        }
    }
    locked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::TempDir;

    fn project(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new("vercel-services").unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    fn warned(dir: &Path, env: &[&str]) -> Vec<String> {
        let env: Vec<String> = env.iter().map(|s| (*s).to_owned()).collect();
        warnings(dir, "production", &env)
    }

    #[test]
    fn the_ai_gateway_without_its_key() {
        // `ai` 5 and later route string model ids through the gateway; 4 has none.
        let ai5 = project(&[("package.json", r#"{"dependencies": {"ai": "^5.0.0", "@ai-sdk/react": "2"}}"#)]);
        let w = warned(ai5.path(), &["OPENAI_API_KEY"]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].starts_with("ai ^5.0.0: a model named by a string") && w[0].contains("AI_GATEWAY_API_KEY isn't set for production") && w[0].contains("Add AI_GATEWAY_API_KEY"), "{}", w[0]);
        assert!(warned(ai5.path(), &["AI_GATEWAY_API_KEY"]).is_empty());
        let ai4 = project(&[("package.json", r#"{"dependencies": {"ai": "^4.3.0"}}"#)]);
        assert!(warned(ai4.path(), &[]).is_empty());
        // `latest` is the newest; the lockfile's version wins over the range.
        let latest = project(&[("package.json", r#"{"dependencies": {"ai": "latest"}}"#)]);
        assert_eq!(warned(latest.path(), &[]).len(), 1);
        let pinned = project(&[
            ("package.json", r#"{"dependencies": {"ai": "*"}}"#),
            ("package-lock.json", r#"{"lockfileVersion": 3, "packages": {"": {}, "node_modules/ai": {"version": "4.3.19"}}}"#),
        ]);
        assert!(warned(pinned.path(), &[]).is_empty());
        // Its own provider: the app calls that one. The gateway named directly is always the gateway.
        let openai = project(&[("package.json", r#"{"dependencies": {"ai": "^5", "@ai-sdk/openai": "^2"}}"#)]);
        assert!(warned(openai.path(), &[]).is_empty());
        let router = project(&[("package.json", r#"{"dependencies": {"ai": "^5", "@openrouter/ai-sdk-provider": "^1"}}"#)]);
        assert!(warned(router.path(), &[]).is_empty());
        let both = project(&[("package.json", r#"{"dependencies": {"ai": "^5", "@ai-sdk/openai": "^2", "@ai-sdk/gateway": "^1"}}"#)]);
        assert!(warned(both.path(), &[])[0].starts_with("@ai-sdk/gateway: "));
    }

    #[test]
    fn blob_connected_through_oidc() {
        let blob = project(&[("package.json", r#"{"dependencies": {"@vercel/blob": "^2.0.0"}}"#)]);
        let w = warned(blob.path(), &["BLOB_STORE_ID"]);
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("@vercel/blob: BLOB_STORE_ID without BLOB_READ_WRITE_TOKEN is a Blob store connected through Vercel's OIDC token") && w[0].contains("add it here as BLOB_READ_WRITE_TOKEN"), "{}", w[0]);
        assert!(warned(blob.path(), &["BLOB_STORE_ID", "BLOB_READ_WRITE_TOKEN"]).is_empty());
        assert!(warned(blob.path(), &[])[0].contains("BLOB_READ_WRITE_TOKEN isn't set for production, so unless the code passes `token` itself"));
        let kv2 = project(&[("package.json", r#"{"dependencies": {"@vercel/kv2": "0.0.19"}}"#)]);
        assert!(warned(kv2.path(), &[])[0].starts_with("@vercel/kv2: "));
        assert!(warned(kv2.path(), &["BLOB_READ_WRITE_TOKEN"]).is_empty());
    }

    #[test]
    fn the_workflow_sdk_picks_vercels_backend() {
        let wf = project(&[("package.json", r#"{"dependencies": {"workflow": "^5.5.0"}}"#)]);
        let w = warned(wf.path(), &["DATABASE_URL"]);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].starts_with("workflow: ") && w[0].contains("forkrail sets VERCEL_DEPLOYMENT_ID") && w[0].contains("Set WORKFLOW_TARGET_WORLD=@workflow/world-postgres"),
            "{}",
            w[0]
        );
        assert!(warned(wf.path(), &["WORKFLOW_TARGET_WORLD"]).is_empty());
        // `workflow` before 4 is another package; Vercel's scope is Vercel's at any version.
        let other = project(&[("package.json", r#"{"devDependencies": {"workflow": "^2.0.6"}}"#)]);
        assert!(warned(other.path(), &[]).is_empty());
        let scoped = project(&[("package.json", r#"{"dependencies": {"@workflow/next": "4.0.1-beta.3"}}"#)]);
        assert!(warned(scoped.path(), &[])[0].starts_with("@workflow/next: "));
    }

    #[test]
    fn oidc_only_packages() {
        let all = project(&[(
            "package.json",
            r#"{"dependencies": {"@vercel/queue": "0.11", "@vercel/schedules": "0", "@vercel/connect": "2", "@vercel/kms": "0.3", "@vercel/passport": "1", "@vercel/integrations": "0.1", "@vercel/oidc-aws-credentials-provider": "3", "@vercel/oidc": "4"}}"#,
        )]);
        let w = warned(all.path(), &[]);
        assert_eq!(w.len(), 8, "{w:?}");
        assert!(w.iter().all(|w| w.contains("Vercel's OIDC token") || w.contains("Vercel's proxy")), "{w:?}");
        assert!(w.iter().any(|w| w.starts_with("@vercel/oidc-aws-credentials-provider: ") && w.contains("give the project an IAM role")), "{w:?}");
        assert!(w.iter().any(|w| w.starts_with("@vercel/queue: ") && w.contains("nothing is delivered")), "{w:?}");
        assert!(w.iter().any(|w| w.starts_with("@vercel/passport: ") && w.contains("every visitor reads as signed out")), "{w:?}");

        // Sandbox and Flags work with their own keys.
        let sandbox = project(&[("package.json", r#"{"dependencies": {"@vercel/sandbox": "^4"}}"#)]);
        let w = warned(sandbox.path(), &["VERCEL_TOKEN"]);
        assert!(w[0].contains("VERCEL_TEAM_ID and VERCEL_PROJECT_ID aren't set for production"), "{}", w[0]);
        assert!(warned(sandbox.path(), &["VERCEL_TEAM_ID", "VERCEL_PROJECT_ID"])[0].contains("VERCEL_TOKEN isn't set for production"));
        assert!(warned(sandbox.path(), &["VERCEL_TOKEN", "VERCEL_TEAM_ID", "VERCEL_PROJECT_ID"]).is_empty());
        let flags = project(&[("package.json", r#"{"dependencies": {"@flags-sdk/vercel": "^1", "flags": "^4"}}"#)]);
        assert!(warned(flags.path(), &[])[0].starts_with("@flags-sdk/vercel: Vercel Flags reads flags with the FLAGS SDK key"));
        assert!(warned(flags.path(), &["FLAGS"]).is_empty());
    }

    #[test]
    fn packages_that_work_off_vercel_get_nothing() {
        let fine = project(&[(
            "package.json",
            r#"{"dependencies": {"@vercel/og": "1", "@vercel/kv": "3", "@vercel/postgres": "0.10", "flags": "4", "@vercel/firewall": "1", "@vercel/otel": "2", "@vercel/edge-config": "1", "@vercel/functions": "3", "@vercel/analytics": "2", "@vercel/speed-insights": "2"}}"#,
        )]);
        assert!(warned(fine.path(), &[]).is_empty());
        // Brought in by another package isn't used by the app: `ai` 5 pulls in @vercel/oidc,
        // `workflow` @vercel/queue.
        let transitive = project(&[
            ("package.json", r#"{"dependencies": {"next": "16"}}"#),
            ("package-lock.json", r#"{"lockfileVersion": 3, "packages": {"node_modules/@vercel/queue": {"version": "0.11.0"}, "node_modules/@vercel/blob": {"version": "2.9.0"}}}"#),
        ]);
        assert!(warned(transitive.path(), &[]).is_empty());
        // No package.json: nothing to say.
        assert!(warned(project(&[]).path(), &[]).is_empty());
    }

    #[test]
    fn ranges() {
        for (range, major) in [("^5.1.0", Some(5)), ("~5", Some(5)), ("5.x", Some(5)), ("5", Some(5)), ("=4.3.19", Some(4)), ("v4.0.0", Some(4)), ("<6", Some(5)), ("<=6.1", Some(6)), ("^4 || ^5.0.0", Some(5)), ("4.0.0 - 6.2.0", Some(6)), ("npm:ai@^7", Some(7))] {
            assert_eq!(range_major(range), major, "{range}");
        }
        for range in [">=4", "*", "latest", "", "workspace:*", "x"] {
            assert_eq!(range_major(range), None, "{range}");
        }
    }

    #[test]
    fn lockfiles() {
        let installed = |l: &Locked, name: &str| l.get(name).map(|v| v.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
        let npm = npm_lock(r#"{"lockfileVersion": 3, "packages": {"": {"name": "x"}, "node_modules/ai": {"version": "5.0.1"}, "node_modules/x/node_modules/ai": {"version": "4.3.0"}, "node_modules/@ai-sdk/gateway": {"version": "1.0.0"}, "node_modules/lib": {"resolved": "packages/lib", "link": true}}}"#).unwrap();
        assert_eq!(installed(&npm, "ai"), ["4.3.0", "5.0.1"]);
        assert_eq!(installed(&npm, "@ai-sdk/gateway"), ["1.0.0"]);
        assert!(!npm.contains_key("lib"));
        let v1 = npm_lock(r#"{"lockfileVersion": 1, "dependencies": {"ai": {"version": "4.3.19", "dependencies": {"zod": {"version": "3.25.0"}}}}}"#).unwrap();
        assert_eq!((installed(&v1, "ai"), installed(&v1, "zod")), (vec!["4.3.19".to_owned()], vec!["3.25.0".to_owned()]));

        let pnpm9 = pnpm_lock("lockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      ai:\n        specifier: ^5.0.0\n        version: 5.0.1(zod@3.25.0)\n\npackages:\n\n  '@ai-sdk/gateway@1.0.0':\n    resolution: {integrity: sha512-x}\n\n  ai@5.0.1:\n    resolution: {integrity: sha512-y}\n\nsnapshots:\n\n  ai@5.0.1(zod@3.25.0):\n    dependencies:\n      '@ai-sdk/gateway': 1.0.0\n");
        assert_eq!((installed(&pnpm9, "ai"), installed(&pnpm9, "@ai-sdk/gateway")), (vec!["5.0.1".to_owned()], vec!["1.0.0".to_owned()]));
        assert!(!pnpm9.contains_key("."), "importers aren't packages");
        let pnpm6 = pnpm_lock("lockfileVersion: '6.0'\n\npackages:\n\n  /@ai-sdk/gateway@1.0.0:\n    resolution: {integrity: x}\n\n  /ai@5.0.1(react@19.0.0):\n    dev: false\n");
        assert_eq!((installed(&pnpm6, "ai"), installed(&pnpm6, "@ai-sdk/gateway")), (vec!["5.0.1".to_owned()], vec!["1.0.0".to_owned()]));
        let pnpm5 = pnpm_lock("lockfileVersion: 5.4\n\npackages:\n\n  /@ai-sdk/gateway/1.0.0:\n    resolution: {integrity: x}\n\n  /ai/4.3.19_react@18.2.0+zod@3.25.0:\n    dev: false\n");
        assert_eq!((installed(&pnpm5, "ai"), installed(&pnpm5, "@ai-sdk/gateway")), (vec!["4.3.19".to_owned()], vec!["1.0.0".to_owned()]));

        let yarn1 = yarn_lock("# yarn lockfile v1\n\n\n\"@ai-sdk/gateway@1.0.0\":\n  version \"1.0.0\"\n  resolved \"https://x\"\n\nai@^5.0.0, ai@^5.0.1:\n  version \"5.0.1\"\n  dependencies:\n    version \"^1.0.0\"\n");
        assert_eq!((installed(&yarn1, "ai"), installed(&yarn1, "@ai-sdk/gateway")), (vec!["5.0.1".to_owned()], vec!["1.0.0".to_owned()]));
        let berry = yarn_lock("__metadata:\n  version: 8\n\n\"@ai-sdk/gateway@npm:1.0.0\":\n  version: 1.0.0\n\n\"ai@npm:^5.0.0, ai@npm:^5.0.1\":\n  version: 5.0.1\n  resolution: \"ai@npm:5.0.1\"\n");
        assert_eq!((installed(&berry, "ai"), installed(&berry, "@ai-sdk/gateway")), (vec!["5.0.1".to_owned()], vec!["1.0.0".to_owned()]));
        assert!(!berry.contains_key("__metadata"));

        let bun = bun_lock("{\n  \"lockfileVersion\": 1,\n  \"workspaces\": {\n    \"\": {\n      \"dependencies\": {\n        \"ai\": \"^5.0.0\",\n      },\n    },\n  },\n  \"packages\": {\n    \"@ai-sdk/gateway\": [\"@ai-sdk/gateway@1.0.0\", \"\", {}, \"sha512-x\"],\n\n    \"ai\": [\"ai@5.0.1\", \"\", { \"dependencies\": { \"@ai-sdk/gateway\": \"1.0.0\" } }, \"sha512-y\"],\n  }\n}\n");
        assert_eq!((installed(&bun, "ai"), installed(&bun, "@ai-sdk/gateway")), (vec!["5.0.1".to_owned()], vec!["1.0.0".to_owned()]));
    }
}
