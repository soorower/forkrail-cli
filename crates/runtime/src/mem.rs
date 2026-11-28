//! Memory numbers from /proc, so the router can see how full a worker is. The worker's memory
//! is the runtime's plus the user's app, the process Lambda's limit really applies to.

use std::sync::atomic::{AtomicU32, Ordering};

static APP_PID: AtomicU32 = AtomicU32::new(0);

/// Count the app process with the runtime from now on.
pub fn set_app_pid(pid: u32) {
    APP_PID.store(pid, Ordering::Relaxed);
}

/// Several programs (a deployment of several functions): the first is the app's pid, the rest
/// counted with it.
pub fn set_app_pids(pids: &[u32]) {
    set_app_pid(pids.first().copied().unwrap_or(0));
    *MORE_PIDS.lock().unwrap() = pids.iter().skip(1).copied().collect();
}

static MORE_PIDS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// The app's process id, or zero before it runs.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn app_pid() -> u32 {
    APP_PID.load(Ordering::Relaxed)
}

/// The status files of the runtime and, once it runs, the app.
fn statuses() -> impl Iterator<Item = String> {
    let app = match APP_PID.load(Ordering::Relaxed) {
        0 => None,
        pid => Some(format!("/proc/{pid}/status")),
    };
    let more: Vec<String> = MORE_PIDS.lock().unwrap().iter().map(|pid| format!("/proc/{pid}/status")).collect();
    std::iter::once("/proc/self/status".to_owned())
        .chain(app)
        .chain(more)
        .map(|path| std::fs::read_to_string(path).unwrap_or_default())
}

/// Memory of this process and its environment, in KiB. Zero where /proc doesn't exist (macOS).
#[derive(Debug, Clone, Copy, Default)]
pub struct Memory {
    /// Resident memory of the runtime and the app now.
    pub rss_kb: u64,
    /// The sum of their peaks since the last `reset_peak`.
    pub rss_peak_kb: u64,
    /// The environment's memory in total, and how much is still available. On Lambda the
    /// VM has more than the function's memory size, so these overstate the headroom.
    pub total_kb: u64,
    pub available_kb: u64,
    /// The cgroup's limit and usage, where the environment exposes them (zero otherwise).
    pub cgroup_limit_kb: u64,
    pub cgroup_used_kb: u64,
    /// The function's configured memory size.
    pub configured_kb: u64,
}

pub fn snapshot() -> Memory {
    let (rss_kb, rss_peak_kb) = rss_and_peak_kb();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    // cgroup v2 first, then v1.
    let (cgroup_limit, cgroup_used) = match bytes("/sys/fs/cgroup/memory.max") {
        0 => (
            bytes("/sys/fs/cgroup/memory/memory.limit_in_bytes"),
            bytes("/sys/fs/cgroup/memory/memory.usage_in_bytes"),
        ),
        limit => (limit, bytes("/sys/fs/cgroup/memory.current")),
    };
    Memory {
        rss_kb,
        rss_peak_kb,
        total_kb: field(&meminfo, "MemTotal:"),
        available_kb: field(&meminfo, "MemAvailable:"),
        cgroup_limit_kb: cgroup_limit / 1024,
        cgroup_used_kb: cgroup_used / 1024,
        configured_kb: limit_kb(),
    }
}

/// Resident memory of the runtime and the app now, in KiB. Cheap enough to call per response.
pub fn rss_kb() -> u64 {
    statuses().map(|status| field(&status, "VmRSS:")).sum()
}

/// Resident memory of the runtime and the app now, and the sum of their peaks since the last
/// `reset_peak`, in KiB.
pub fn rss_and_peak_kb() -> (u64, u64) {
    // The runtime's own peak counter can't be reset once it's undumpable (main.rs): its
    // `clear_refs` belongs to root. Then its share of the peak is what it holds now.
    let own_peak = if SELF_RESETS.load(Ordering::Relaxed) { "VmHWM:" } else { "VmRSS:" };
    statuses().enumerate().fold((0, 0), |(rss, peak), (i, status)| (rss + field(&status, "VmRSS:"), peak + field(&status, if i == 0 { own_peak } else { "VmHWM:" })))
}

/// Whether the runtime's own peak counter could be reset, last time it was tried.
static SELF_RESETS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// The function's configured memory size in KiB, or zero outside Lambda.
pub fn limit_kb() -> u64 {
    std::env::var("AWS_LAMBDA_FUNCTION_MEMORY_SIZE").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024
}

/// A number of bytes from a cgroup file; zero if missing or "max".
fn bytes(path: &str) -> u64 {
    std::fs::read_to_string(path).ok().and_then(|text| text.trim().parse().ok()).unwrap_or(0)
}

/// Restart the peak counters of the runtime and the app. Best effort.
pub fn reset_peak() {
    // Only where there's a /proc: on macOS there's nothing to reset either way.
    if std::path::Path::new("/proc/self").exists() {
        SELF_RESETS.store(std::fs::write("/proc/self/clear_refs", "5").is_ok(), Ordering::Relaxed);
    }
    if let pid @ 1.. = APP_PID.load(Ordering::Relaxed) {
        let _ = std::fs::write(format!("/proc/{pid}/clear_refs"), "5");
    }
    for pid in MORE_PIDS.lock().unwrap().iter() {
        let _ = std::fs::write(format!("/proc/{pid}/clear_refs"), "5");
    }
}

fn field(text: &str, name: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(name))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}
