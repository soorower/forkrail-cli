//! The app's env vars. Most deployments have them in Lambda's environment, this process's own,
//! which the app inherits. One whose variables don't fit there (4 KB in all) has them in a sealed
//! file (forkrail_common::envfile): its key is in our environment, the file in a layer. `load`
//! opens it once, before the app starts; the app gets its variables, and never the key.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use forkrail_common::envfile::{FILE, KEY_VAR, open};

static FROM_FILE: OnceLock<BTreeMap<String, String>> = OnceLock::new();

/// Open the deployment's sealed file, if it has one. An error means the app can't run as it was
/// deployed: better no app than one without its variables.
pub fn load() -> Result<usize, String> {
    // Kept in the binary, where packaging looks for it (forkrail_common::envfile).
    std::hint::black_box(forkrail_common::envfile::RUNTIME_MARKER);
    let vars = match std::env::var(KEY_VAR) {
        Ok(key) => {
            // Elsewhere than Lambda (a test on a laptop), FORKRAIL_ENV_FILE says where it is.
            let path = std::env::var("FORKRAIL_ENV_FILE").unwrap_or_else(|_| FILE.to_owned());
            let file = std::fs::read(&path).map_err(|e| format!("{KEY_VAR} is set but {path} can't be read: {e}"))?;
            open(&key, &file)?
        }
        Err(_) => BTreeMap::new(),
    };
    let count = vars.len();
    let _ = FROM_FILE.set(vars);
    Ok(count)
}

/// The variables from the sealed file, for the app's process (none without one).
pub fn from_file() -> &'static BTreeMap<String, String> {
    FROM_FILE.get_or_init(BTreeMap::new)
}

/// One of the app's variables, wherever it is.
pub fn var(name: &str) -> Option<String> {
    from_file().get(name).cloned().or_else(|| std::env::var(name).ok())
}

/// Whether the project set its own AWS keys but no session token: then Lambda's token, which
/// belongs to the function role's keys, mustn't reach the app, or the SDK would pair the two.
pub fn own_keys_without_token() -> bool {
    // Kept in the binary, where packaging looks for it (forkrail_common::envfile).
    std::hint::black_box(forkrail_common::envfile::RUNTIME_AWS_KEYS_MARKER);
    let file = from_file();
    (file.contains_key("AWS_ACCESS_KEY_ID") || file.contains_key("AWS_SECRET_ACCESS_KEY")) && !file.contains_key("AWS_SESSION_TOKEN")
}

/// The region an environment runs in, as Vercel names its regions (`VERCEL_REGION`), and as AWS
/// does (`FORKRAIL_REGION`), unless the project set them.
pub fn region_vars() -> Vec<(&'static str, String)> {
    let Ok(aws) = std::env::var("AWS_REGION") else { return Vec::new() };
    let vercel = forkrail_common::vercel_region(&aws).unwrap_or(aws.as_str()).to_owned();
    [("VERCEL_REGION", vercel), ("FORKRAIL_REGION", aws)].into_iter().filter(|(name, _)| !is_set(name)).collect()
}

/// Whether the app sees `name` set.
pub fn is_set(name: &str) -> bool {
    from_file().contains_key(name) || std::env::var_os(name).is_some()
}
