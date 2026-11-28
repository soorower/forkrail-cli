//! The install's own hosts, for the shims' loop guard (`forkrail_common::loops`): each invocation's
//! payload lists them, and the file the app is told of (`OWN_HOSTS_FILE_ENV`) is rewritten when
//! the list changes. The shims read it again when it changes; until a first invocation writes it
//! there's none, and calls go out unmarked.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use tracing::warn;

/// In the environment's own `/tmp`, named after this process: local runs share one.
pub fn path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| std::env::temp_dir().join(format!("forkrail-own-hosts-{}", std::process::id())))
}

/// What the app is told: where the file is.
pub fn app_env() -> (&'static str, String) {
    (forkrail_common::loops::OWN_HOSTS_FILE_ENV, path().display().to_string())
}

/// Write `own` unless it's what was written last. A file that can't be written leaves the last one.
pub fn note(own: &[String]) {
    static WRITTEN: Mutex<Option<Vec<String>>> = Mutex::new(None);
    let mut written = WRITTEN.lock().unwrap();
    if written.as_deref() == Some(own) {
        return;
    }
    let path = path();
    let partial = path.with_extension("partial");
    // Replaced by a rename, so a shim never reads half a list.
    match std::fs::write(&partial, forkrail_common::loops::own_hosts_file(own)).and_then(|()| std::fs::rename(&partial, path)) {
        Ok(()) => *written = Some(own.to_vec()),
        Err(e) => warn!(path = %path.display(), error = %e, "writing the install's own hosts failed"),
    }
}
