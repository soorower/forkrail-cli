//! One forkrail install: the names everything in it is found by. An install lives in one AWS
//! account, with its state in a bucket in its home region; every other name follows from those, so
//! the CLI, the router and the setup scripts agree by using these instead of their own literals.

/// Every resource forkrail creates carries this tag, and nothing without it is ever changed: the
/// account may run other workloads.
pub const TAG_KEY: &str = "project";
pub const TAG_VALUE: &str = "forkrail";
/// A project's function also carries its project's name.
pub const PROJECT_TAG: &str = "forkrail-project";
/// The role a project's functions run as unless it names its own (`forkrail project --role`).
pub const WORKER_ROLE: &str = "forkrail-worker";
pub const DEFAULT_HOME_REGION: &str = "us-east-1";
/// Every function's environment names the install's home region, where its state is.
pub const HOME_REGION_ENV: &str = "FORKRAIL_HOME_REGION";

/// State, in the install's bucket.
pub const ROUTES_KEY: &str = "state/routes.json";
pub const SETTINGS_KEY: &str = "state/settings.json";
pub const PROJECTS_PREFIX: &str = "state/projects/";
pub const CERTS_PREFIX: &str = "state/certs/";
pub const ANALYTICS_ENDPOINT_KEY: &str = "state/analytics/endpoint.json";
pub const ANALYTICS_CERT_KEY: &str = "state/analytics/ca.pem";

/// Secrets, as SSM SecureStrings in the home region.
pub const CLICKHOUSE_READER_PARAM: &str = "/forkrail/clickhouse/reader";
pub const CLOUDFLARE_TOKEN_PARAM: &str = "/forkrail/cloudflare-api-token";
/// The routers' shared key for the firewall's challenge passes: 32 random bytes as hex
/// (SecureString), made by `test-env.sh push` if missing.
pub const CHALLENGE_KEY_PARAM: &str = "/forkrail/router/challenge-key";
/// The routers' shared key for their link, over which pages go between them (the router's
/// `cache/peers.rs`): 32 random bytes as hex (SecureString), made by `test-env.sh push` if missing.
pub const PEER_KEY_PARAM: &str = "/forkrail/router/peer-key";
/// Projects' env var values, one SecureString each under `<prefix><project>/<target>/<KEY>`, and
/// previews' values for one Git branch under `<prefix><project>/branches/<branch>/<KEY>`
/// ([`env_branch_dir`]).
pub const ENV_PARAM_PREFIX: &str = "/forkrail/env/";
/// The KMS key that encrypts env var values, in every region with functions
/// (deploy/install/secrets.yaml): the parameters, and the functions' environments.
pub const ENV_KEY_ALIAS: &str = "alias/forkrail-env";

/// The dashboard's own parameters. Its secrets among them are SecureStrings under
/// [`ENV_KEY_ALIAS`], and the dashboard's role may decrypt what's under this prefix and nothing
/// else (deploy/aws/dashboard.sh), so a project's env var values stay out of its reach.
pub const WEB_PARAM_PREFIX: &str = "/forkrail/web/";
/// The password of the dashboard's Postgres user (deploy/aws/analytics.sh postgres makes it).
pub const POSTGRES_PASSWORD_PARAM: &str = "/forkrail/web/postgres";
/// The install's GitHub App, for signing in: its OAuth client secret, and the secret GitHub signs
/// webhooks with.
pub const GITHUB_CLIENT_SECRET_PARAM: &str = "/forkrail/web/github-client-secret";
pub const GITHUB_WEBHOOK_SECRET_PARAM: &str = "/forkrail/web/github-webhook-secret";
/// Who owns the install, as GitHub accounts (plain JSON, not a secret): they can always sign in,
/// and only someone with the AWS account can change the list (`forkrail owners`).
pub const OWNERS_PARAM: &str = "/forkrail/web/owners";
/// The GitHub App itself (plain JSON: id, slug, client id, addresses), which the dashboard reads.
pub const GITHUB_APP_PARAM: &str = "/forkrail/github/app";
/// The app's private key, which signs requests for repository tokens. Outside the dashboard's
/// prefix on purpose: the dashboard can't read it.
pub const GITHUB_PRIVATE_KEY_PARAM: &str = "/forkrail/github/private-key";

/// The SSM parameter holding one env var's value. `target` is a target's name, or a branch's
/// [`env_branch_dir`].
pub fn env_param(project: &str, target: &str, key: &str) -> String {
    format!("{ENV_PARAM_PREFIX}{project}/{target}/{key}")
}

/// The longest a branch's part of a parameter name may be, so that the whole name, with a long
/// project and variable name, stays within SSM's 1011 characters (counted with the ARN).
pub const MAX_BRANCH_DIR: usize = 400;

/// Where one Git branch's preview values are kept, under a project's: `branches/<branch>`, the
/// branch written in the characters SSM allows in a name. Letters, digits, `.` and `-` stay; every
/// other byte (`/` and `_` among them) becomes `_` and two hex digits, so no two branches share a
/// place: `feature/logs` is `branches/feature_2Flogs`.
pub fn env_branch_dir(branch: &str) -> String {
    let mut dir = String::from("branches/");
    for b in branch.bytes() {
        if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' {
            dir.push(b as char);
        } else {
            dir.push_str(&format!("_{b:02X}"));
        }
    }
    dir
}

pub fn project_key(name: &str) -> String {
    format!("{PROJECTS_PREFIX}{name}.json")
}

pub fn deployment_key(id: &str) -> String {
    format!("{DEPLOYMENTS_PREFIX}{id}.json")
}

pub const DEPLOYMENTS_PREFIX: &str = "state/deployments/";

/// A canceled remote build's record, as it was canceled: the builder looks for it, through a URL
/// it's given, to know it should stop (it has no rights of its own to read the record). Missing
/// while the build goes on.
pub fn cancel_key(id: &str) -> String {
    format!("{DEPLOYMENTS_PREFIX}{id}.cancel")
}

/// Where the install says where its project files and deployment records are kept
/// (`forkrail state mode`): in the bucket, or in its table (deploy/install/state.yaml). Missing,
/// the bucket.
pub const STORE_KEY: &str = "state/store.json";
/// The table that holds project files and deployment records, one item each, and the KMS key it's
/// encrypted with: only the control function, the dashboard and the install's administrators may
/// read or write it (its resource policy).
pub const STATE_TABLE: &str = "forkrail-state";
pub const STATE_KEY_ALIAS: &str = "alias/forkrail-state";
/// The table's indexes: every item of a kind (`project`, `deployment`) by key, carrying only its
/// version; and a project's deployments by when they were made, in full.
pub const BY_KIND_INDEX: &str = "by-kind";
pub const BY_PROJECT_INDEX: &str = "by-project";

/// The kinds of state the table holds, with the bucket prefix each one's files are under.
pub const TABLE_KINDS: [(&str, &str); 2] = [("project", PROJECTS_PREFIX), ("deployment", DEPLOYMENTS_PREFIX)];

/// A state file's item in the table, `(kind, key)`, for the files the table holds: the bucket's
/// key without `state/` and `.json` (`projects/pyapp`, `deployments/a1b2c3d4`). None for every
/// other file, which stays in the bucket.
pub fn table_item(key: &str) -> Option<(&'static str, String)> {
    TABLE_KINDS.iter().find_map(|&(kind, prefix)| {
        let name = key.strip_prefix(prefix)?.strip_suffix(".json")?;
        (!name.is_empty() && !name.contains('/')).then(|| (kind, key["state/".len()..key.len() - ".json".len()].to_owned()))
    })
}

/// The bucket key of a table item's key: the inverse of [`table_item`].
pub fn state_key(item: &str) -> String {
    format!("state/{item}.json")
}

/// The kind of item a bucket prefix lists, when it's one the table holds whole.
pub fn table_kind(prefix: &str) -> Option<&'static str> {
    TABLE_KINDS.iter().find(|(_, p)| *p == prefix).map(|(kind, _)| *kind)
}

/// The lock one publish of a project holds, so two don't publish from the function at once.
pub fn lock_key(project: &str) -> String {
    format!("state/locks/{project}.json")
}

/// Where a custom domain's certificate is kept once issued.
pub fn cert_key(domain: &str) -> String {
    format!("{CERTS_PREFIX}{domain}/cert.pem")
}

/// Where the issuing router says how a custom domain's certificate stands (`domains::CertStatus`).
pub fn cert_status_key(domain: &str) -> String {
    format!("{CERTS_PREFIX}{domain}/{}", crate::domains::STATUS_FILE)
}

/// What every project function's name starts with.
pub const FUNCTION_PREFIX: &str = "forkrail-app-";

/// Written while `forkrail sweep --yes` deletes static files, `{"until": <unix seconds>}`: a deploy
/// that sees it uploads every static file of its build instead of skipping those already there.
pub const SWEEP_KEY: &str = "state/sweep.json";

/// The dashboard's own table (deploy/install/web.yaml): its sign-ins, and what it keeps per project
/// (a protection password, bypass names, share links under `protection#<project>`).
pub const WEB_TABLE: &str = "forkrail-web";

/// The dashboard's own project (deploy/aws/dashboard.sh): never deleted from the dashboard or the CLI.
pub const DASHBOARD_PROJECT: &str = "forkrail-web";

/// Where the dashboard keeps a deployment's page preview: `previews/<project>/<id>.json` and `.jpg`.
pub fn previews_prefix(project: &str) -> String {
    format!("previews/{project}/")
}

/// A project's Lambda function. Each deployment is one published version of it.
pub fn function_name(project: &str) -> String {
    format!("{FUNCTION_PREFIX}{project}")
}

/// The builder (deploy/install/builder.yaml): the Lambda function remote builds run in, and the
/// SNS topic Lambda reports each build's end to, which the control function listens on.
pub const BUILDER_FUNCTION: &str = "forkrail-builder";
/// The builder with more memory (builder.yaml `LargeMemorySize`): a build that ran out of the
/// builder's memory is sent to it once, and its project's builds start there from then on.
pub const BUILDER_LARGE_FUNCTION: &str = "forkrail-builder-large";
/// The queue events go to (what happens to deployments, attacks, certificates), and the function
/// that takes them from it: to the team's webhooks, its members' notifications and their email
/// (deploy/install/notify.yaml, deploy/notify).
pub const EVENTS_QUEUE: &str = "forkrail-events";
pub const NOTIFY_FUNCTION: &str = "forkrail-notify";
pub const BUILDS_TOPIC: &str = "forkrail-builds";

/// forkrail's own functions besides the builders, which run for the team but outside any router:
/// control (deploys, promotes, the dashboard's actions), the notifier, the spend guard, and the
/// one that activates the cost allocation tag. The guard counts their time toward the team budget.
pub const CONTROL_FUNCTION: &str = "forkrail-control";
pub const GUARD_FUNCTION: &str = "forkrail-guard";
pub const COST_TAGS_FUNCTION: &str = "forkrail-cost-tags";
pub const OWN_FUNCTIONS: [&str; 4] = [CONTROL_FUNCTION, NOTIFY_FUNCTION, GUARD_FUNCTION, COST_TAGS_FUNCTION];
/// The registry the builder's images are kept in (deploy/aws/builder.sh).
pub const BUILDER_REPOSITORY: &str = "forkrail-builder";
/// What the guard counted of forkrail's own costs this month, from AWS's numbers (storage, the
/// tables, its own functions): the routers and the dashboard add it to the team's spend.
pub const OWN_SPEND_KEY: &str = "state/spend/own.json";

/// A remote build's source as the CLI uploaded it, by content: what the project's ignore files
/// leave, encrypted under the env key (it may hold a committed `.env`). Kept for rebuilds.
pub fn upload_key(project: &str, sha256: &str) -> String {
    format!("uploads/{project}/{sha256}.zip")
}

/// What one remote build reads and writes, gone once it has ended: its order (with env values,
/// encrypted under the env key) and its output.
pub fn build_key(project: &str, id: &str, name: &str) -> String {
    format!("builds/{project}/{id}/{name}")
}

/// A project's build cache for a target (`production` or `preview`): what its builds in the builder
/// keep for the next one, its `node_modules` or installed Python requirements (the CLI's cache.rs).
pub fn cache_key(project: &str, target: &str) -> String {
    format!("cache/{project}/{target}.tar.zst")
}

/// A Next.js project's `.next/cache` for a target, kept apart from `cache_key`'s archive because it
/// is written after every build, not only when the dependencies change (the CLI's cache.rs).
pub fn next_cache_key(project: &str, target: &str) -> String {
    format!("cache/{project}/{target}.next.tar.zst")
}

/// The state bucket, unless the install names its own.
pub fn default_bucket(account: &str) -> String {
    format!("forkrail-artifacts-{account}")
}

/// The bucket Lambda in `region` reads packages from, and the router there reads static files from:
/// Lambda takes code only from a bucket in its own region. The state bucket itself in the home
/// region, `<bucket>-<region>` in the others.
pub fn code_bucket(bucket: &str, home_region: &str, region: &str) -> String {
    if region == home_region { bucket.to_owned() } else { format!("{bucket}-{region}") }
}

/// The partition an ARN is in: `aws`, or `aws-cn` and `aws-us-gov`, whose ARNs differ.
pub fn partition_of(arn: &str) -> Option<&str> {
    arn.strip_prefix("arn:")?.split(':').next().filter(|p| !p.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub account: String,
    pub partition: String,
    /// Where the state bucket is, and where deployments are first published.
    pub home_region: String,
    pub bucket: String,
}

impl Install {
    /// The install in `account`, with the default bucket unless `bucket` names another.
    pub fn new(account: &str, partition: &str, home_region: &str, bucket: Option<&str>) -> Self {
        Self {
            account: account.to_owned(),
            partition: partition.to_owned(),
            home_region: home_region.to_owned(),
            bucket: bucket.map_or_else(|| default_bucket(account), str::to_owned),
        }
    }

    pub fn code_bucket(&self, region: &str) -> String {
        code_bucket(&self.bucket, &self.home_region, region)
    }

    /// The SQS queue schedules' runs go through (`crate::schedules::QUEUE`), in the home region.
    pub fn schedule_queue_arn(&self) -> String {
        format!("arn:{}:sqs:{}:{}:{}", self.partition, self.home_region, self.account, crate::schedules::QUEUE)
    }

    pub fn schedule_queue_url(&self) -> String {
        let domain = if self.partition == "aws-cn" { "amazonaws.com.cn" } else { "amazonaws.com" };
        format!("https://sqs.{}.{domain}/{}/{}", self.home_region, self.account, crate::schedules::QUEUE)
    }

    /// The queue events go to (`EVENTS_QUEUE`), in the home region.
    pub fn events_queue_url(&self) -> String {
        let domain = if self.partition == "aws-cn" { "amazonaws.com.cn" } else { "amazonaws.com" };
        format!("https://sqs.{}.{domain}/{}/{EVENTS_QUEUE}", self.home_region, self.account)
    }

    /// The role EventBridge Scheduler sends runs to the queue as.
    pub fn scheduler_role_arn(&self) -> String {
        format!("arn:{}:iam::{}:role/{}", self.partition, self.account, crate::schedules::SCHEDULER_ROLE)
    }

    /// The role a project's functions run as: its own, by name or ARN, or the shared worker role.
    pub fn role_arn(&self, role: Option<&str>) -> String {
        match role {
            Some(arn) if arn.starts_with("arn:") => arn.to_owned(),
            Some(name) => format!("arn:{}:iam::{}:role/{name}", self.partition, self.account),
            None => format!("arn:{}:iam::{}:role/{WORKER_ROLE}", self.partition, self.account),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names an install had before they were gathered here: changing any of them would orphan
    /// an existing install's resources.
    #[test]
    fn names_are_unchanged() {
        let install = Install::new("123456789012", "aws", "us-east-1", None);
        assert_eq!(install.bucket, "forkrail-artifacts-123456789012");
        assert_eq!(install.code_bucket("us-east-1"), "forkrail-artifacts-123456789012");
        assert_eq!(install.code_bucket("us-west-1"), "forkrail-artifacts-123456789012-us-west-1");
        assert_eq!(install.role_arn(None), "arn:aws:iam::123456789012:role/forkrail-worker");
        assert_eq!(install.role_arn(Some("forkrail-dashboard")), "arn:aws:iam::123456789012:role/forkrail-dashboard");
        assert_eq!(install.role_arn(Some("arn:aws:iam::2:role/x")), "arn:aws:iam::2:role/x");
        assert_eq!(function_name("pyapp"), "forkrail-app-pyapp");
        assert_eq!(project_key("pyapp"), "state/projects/pyapp.json");
        assert_eq!(deployment_key("a1b2c3d4"), "state/deployments/a1b2c3d4.json");
        assert_eq!(cert_key("api.example.com"), "state/certs/api.example.com/cert.pem");
        assert_eq!(lock_key("pyapp"), "state/locks/pyapp.json");
        assert_eq!(env_param("pyapp", "preview", "GREETING"), "/forkrail/env/pyapp/preview/GREETING");
        assert_eq!(env_param("pyapp", &env_branch_dir("feature/logs"), "GREETING"), "/forkrail/env/pyapp/branches/feature_2Flogs/GREETING");
        assert_eq!(env_branch_dir("Fix-1.2"), "branches/Fix-1.2");
        // `_` is escaped too, so `a_2Fb` and `a/b` stay apart; bytes beyond ASCII one by one.
        assert_eq!(env_branch_dir("a_2Fb"), "branches/a_5F2Fb");
        assert_eq!(env_branch_dir("dependabot/npm_and_yarn/é"), "branches/dependabot_2Fnpm_5Fand_5Fyarn_2F_C3_A9");
        assert_eq!((TAG_KEY, TAG_VALUE, PROJECT_TAG), ("project", "forkrail", "forkrail-project"));
        assert_eq!((BUILDER_FUNCTION, BUILDS_TOPIC), ("forkrail-builder", "forkrail-builds"));
        assert_eq!(upload_key("pyapp", "ab"), "uploads/pyapp/ab.zip");
        assert_eq!(build_key("pyapp", "a1b2c3d4", "order.json"), "builds/pyapp/a1b2c3d4/order.json");
        assert_eq!(cache_key("pyapp", "preview"), "cache/pyapp/preview.tar.zst");
        assert_eq!(next_cache_key("shop", "production"), "cache/shop/production.next.tar.zst");
        assert_eq!(
            (POSTGRES_PASSWORD_PARAM, GITHUB_CLIENT_SECRET_PARAM, GITHUB_WEBHOOK_SECRET_PARAM, OWNERS_PARAM),
            ("/forkrail/web/postgres", "/forkrail/web/github-client-secret", "/forkrail/web/github-webhook-secret", "/forkrail/web/owners")
        );
        for name in [POSTGRES_PASSWORD_PARAM, GITHUB_CLIENT_SECRET_PARAM, GITHUB_WEBHOOK_SECRET_PARAM, OWNERS_PARAM] {
            assert!(name.starts_with(WEB_PARAM_PREFIX), "{name} must be under the dashboard's prefix");
        }
        assert_eq!((GITHUB_APP_PARAM, GITHUB_PRIVATE_KEY_PARAM), ("/forkrail/github/app", "/forkrail/github/private-key"));
        assert!(!GITHUB_PRIVATE_KEY_PARAM.starts_with(WEB_PARAM_PREFIX), "the dashboard must not read the app's private key");
        assert_eq!((STORE_KEY, STATE_TABLE, STATE_KEY_ALIAS), ("state/store.json", "forkrail-state", "alias/forkrail-state"));
        assert_eq!((BY_KIND_INDEX, BY_PROJECT_INDEX), ("by-kind", "by-project"));
        assert_eq!(cancel_key("a1b2c3d4"), "state/deployments/a1b2c3d4.cancel");
    }

    /// Project files and deployment records are the table's; everything else under `state/` stays
    /// in the bucket (the routers read it), and a key maps to its item and back.
    #[test]
    fn which_files_the_table_holds() {
        assert_eq!(table_item("state/projects/pyapp.json"), Some(("project", "projects/pyapp".to_owned())));
        assert_eq!(table_item("state/deployments/a1b2c3d4.json"), Some(("deployment", "deployments/a1b2c3d4".to_owned())));
        for key in [
            ROUTES_KEY,
            SETTINGS_KEY,
            STORE_KEY,
            "state/locks/pyapp.json",
            "state/routing/pyapp/routes.json",
            "state/certs/a.example.com/status.json",
            "state/deployments/a1b2c3d4.cancel",
            "state/projects/.json",
            "state/projects/a/b.json",
        ] {
            assert_eq!(table_item(key), None, "{key}");
        }
        assert_eq!(state_key("projects/pyapp"), project_key("pyapp"));
        assert_eq!(state_key("deployments/a1b2c3d4"), deployment_key("a1b2c3d4"));
        assert_eq!(table_kind(PROJECTS_PREFIX), Some("project"));
        assert_eq!(table_kind(DEPLOYMENTS_PREFIX), Some("deployment"));
        assert_eq!(table_kind("state/"), None);
        assert_eq!(table_kind("state/certs/"), None);
    }

    #[test]
    fn other_buckets_and_partitions() {
        let install = Install::new("1", "aws-cn", "cn-north-1", Some("mine"));
        assert_eq!(install.code_bucket("cn-northwest-1"), "mine-cn-northwest-1");
        assert_eq!(install.role_arn(None), "arn:aws-cn:iam::1:role/forkrail-worker");
        assert_eq!(partition_of("arn:aws:sts::1:assumed-role/r/s"), Some("aws"));
        assert_eq!(partition_of("arn:aws-us-gov:iam::1:user/u"), Some("aws-us-gov"));
        assert_eq!(partition_of("not an arn"), None);
    }
}
