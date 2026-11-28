//! A build's log: every line forkrail writes while building, and every line the project's own
//! commands print (the install, the framework's build, pip). It goes to the terminal, and with a
//! deployment recorded as building (track.rs) to its live build log too (report.rs).
//!
//! The project's commands run here too (`run`): with the build's env vars, without this machine's
//! AWS credentials, with no input, and with their output read line by line into the log.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// forkrail's own lines.
    Forkrail,
    Stdout,
    Stderr,
    /// The start of a step (`Building`): the dashboard lists them with their times.
    Step,
}

impl Stream {
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Forkrail => "forkrail",
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::Step => "step",
        }
    }

    /// A stream by its name in a kept log; anything else reads as forkrail's.
    pub fn parse(name: &str) -> Stream {
        match name {
            "stdout" => Stream::Stdout,
            "stderr" => Stream::Stderr,
            "step" => Stream::Step,
            _ => Stream::Forkrail,
        }
    }
}

/// The steps a deployment's log is divided into, in order. A build has the first four (some may
/// be skipped), a redeploy only the last.
pub const PREPARING: &str = "Preparing";
pub const INSTALLING: &str = "Installing dependencies";
pub const BUILDING: &str = "Building";
pub const PACKAGING: &str = "Packaging";
pub const PUBLISHING: &str = "Publishing";

pub trait BuildLog: Send + Sync {
    fn line(&self, stream: Stream, text: &str);

    fn info(&self, text: &str) {
        self.line(Stream::Forkrail, text);
    }

    fn step(&self, name: &str) {
        self.line(Stream::Step, name);
    }
}

/// The build's lines on this process's stderr, as a deploy has always printed them.
pub struct Terminal;

impl BuildLog for Terminal {
    fn line(&self, stream: Stream, text: &str) {
        print_line(stream, text);
    }
}

/// One line as the terminal shows it.
pub fn print_line(stream: Stream, text: &str) {
    match stream {
        Stream::Forkrail => eprintln!("==> {text}"),
        Stream::Step => eprintln!("==> {}", text.to_lowercase()),
        Stream::Stdout | Stream::Stderr => eprintln!("{text}"),
    }
}

/// A build's line as it's safe to print in a terminal: control characters (ESC above all, which
/// starts sequences that move the cursor, retitle the window or write the clipboard) shown as
/// `\x1b` and the like, tabs kept. For lines a build sent from elsewhere (the builder), whose
/// code is anyone's who can push.
pub fn printable(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(|c| c.is_control() && c != '\t') {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.chars().map(|c| if c.is_control() && c != '\t' { format!("\\x{:02x}", c as u32) } else { c.to_string() }).collect())
}

/// Hides env var values in text that leaves this machine: each value of 8 characters or more
/// becomes `[NAME hidden]`. Shorter ones (`1`, `true`, a port) would hide ordinary text and
/// are rarely secrets. Longer values are matched first, so one that contains another is hidden
/// whole.
#[derive(Debug, Clone, Default)]
pub struct Mask {
    values: Vec<(String, String)>,
}

/// Values shorter than this are left alone.
pub const MASK_MIN_LEN: usize = 8;

impl Mask {
    pub fn new(env: &BTreeMap<String, String>) -> Self {
        // Output is masked a line at a time, so a value of several lines (a PEM key, a service
        // account's JSON) is hidden line by line too, and as JSON writes it (`\n`, `\"`).
        let forms = |v: &String| {
            let mut forms = vec![v.clone()];
            if v.contains(['\n', '\r']) {
                forms.extend(v.lines().map(str::trim).filter(|l| l.chars().count() >= MASK_MIN_LEN).map(str::to_owned));
            }
            if let Ok(json) = serde_json::to_string(v)
                && let Some(inner) = json.strip_prefix('"').and_then(|j| j.strip_suffix('"'))
                && inner != v
            {
                forms.push(inner.to_owned());
            }
            forms
        };
        let mut values: Vec<(String, String)> = env
            .iter()
            .filter(|(_, v)| v.chars().count() >= MASK_MIN_LEN)
            .flat_map(|(k, v)| forms(v).into_iter().map(move |f| (f, format!("[{k} hidden]"))))
            .collect();
        values.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        values.dedup_by(|a, b| a.0 == b.0);
        Mask { values }
    }

    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (value, label) in &self.values {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), label);
            }
        }
        out
    }
}

/// The process groups of the project's commands running now, so a build that's stopped can stop
/// them (`stop_commands`).
static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Run one of the project's commands, its output into `log`. `env` (the build's: system variables
/// and the project's own) is added to this process's environment, less every `AWS_*` variable of
/// the machine's: a build has no business with its credentials, and in a shared builder they
/// wouldn't be the user's. The project's own `AWS_*` go through, as on Vercel. Each command runs in a process
/// group of its own, so stopping it stops what it started (`npm` starts `node`).
pub fn run(command: &mut Command, what: &str, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<()> {
    let status = run_to_end(command, what, env, log)?;
    if !status.success() {
        bail!("{what} failed ({status})");
    }
    Ok(())
}

/// `run`, for a command whose way of ending is its answer: how it exited, whatever that was.
pub fn run_to_end(command: &mut Command, what: &str, env: &BTreeMap<String, String>, log: &dyn BuildLog) -> Result<std::process::ExitStatus> {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("AWS_") {
            command.env_remove(&key);
        }
    }
    command.envs(env);
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    let mut child = command.spawn().with_context(|| format!("{what}: starting {:?}", command.get_program()))?;
    let pid = child.id();
    RUNNING.lock().expect("running lock").push(pid);
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    std::thread::scope(|scope| {
        scope.spawn(|| forward(stdout, Stream::Stdout, log));
        scope.spawn(|| forward(stderr, Stream::Stderr, log));
    });
    let status = child.wait().with_context(|| format!("{what}: waiting for it"));
    RUNNING.lock().expect("running lock").retain(|p| *p != pid);
    status
}

/// Stop the project's commands that are running, and everything they started.
pub fn stop_commands() {
    let groups = RUNNING.lock().expect("running lock").clone();
    for pid in groups {
        if let Ok(pid) = i32::try_from(pid) {
            signal(-pid, SIGTERM);
        }
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;

/// `kill(2)`, without a `kill` command (the builder's image has none). A negative `pid` is a
/// process group.
pub fn signal(pid: i32, sig: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(2) takes two integers and touches no memory of ours.
        unsafe { kill(pid, sig) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, sig);
        false
    }
}

/// Each line of `reader` into the log, as text even when it isn't quite UTF-8.
fn forward(reader: impl Read, stream: Stream, log: &dyn BuildLog) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    line.pop();
                }
                log.line(stream, &String::from_utf8_lossy(&line));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Collect(Mutex<Vec<(Stream, String)>>);

    /// Tests that run commands take turns: one stops every command running.
    static COMMANDS: Mutex<()> = Mutex::new(());

    impl BuildLog for Collect {
        fn line(&self, stream: Stream, text: &str) {
            self.0.lock().unwrap().push((stream, text.to_owned()));
        }
    }

    #[test]
    fn lines_env_and_failure() {
        let _turn = COMMANDS.lock().unwrap_or_else(|e| e.into_inner());
        let log = Collect::default();
        let env: BTreeMap<String, String> = [("GREETING".to_owned(), "hi".to_owned()), ("AWS_REGION".to_owned(), "nope".to_owned())].into();
        // SAFETY: tests in this module don't read the environment concurrently with this.
        unsafe { std::env::set_var("AWS_SECRET_ACCESS_KEY", "from-the-machine") };
        let script = r#"echo "out $GREETING"; echo "err" >&2; printf 'no newline'; echo "aws=${AWS_SECRET_ACCESS_KEY:-none} region=${AWS_REGION:-none}" >&2"#;
        run(Command::new("sh").args(["-c", script]), "the script", &env, &log).unwrap();
        let lines = log.0.lock().unwrap().clone();
        assert!(lines.contains(&(Stream::Stdout, "out hi".to_owned())), "{lines:?}");
        assert!(lines.contains(&(Stream::Stdout, "no newline".to_owned())), "{lines:?}");
        assert!(lines.contains(&(Stream::Stderr, "err".to_owned())), "{lines:?}");
        // The machine's AWS credentials never; the project's own AWS_REGION, as on Vercel.
        assert!(lines.contains(&(Stream::Stderr, "aws=none region=nope".to_owned())), "{lines:?}");

        let failed = run(Command::new("sh").args(["-c", "exit 3"]), "the failing step", &env, &log).unwrap_err();
        assert!(failed.to_string().contains("the failing step failed"), "{failed}");
    }

    #[test]
    fn stopping_ends_a_command_and_what_it_started() {
        let _turn = COMMANDS.lock().unwrap_or_else(|e| e.into_inner());
        let log = Collect::default();
        let started = std::time::Instant::now();
        let stopper = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            stop_commands();
        });
        // The shell starts a sleep of its own, which must end too, or its output pipe stays open.
        let stopped = run(Command::new("sh").args(["-c", "sleep 30; echo late"]), "the long step", &BTreeMap::new(), &log).unwrap_err();
        stopper.join().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        assert!(stopped.to_string().contains("the long step failed"), "{stopped}");
        assert!(RUNNING.lock().unwrap().is_empty());
    }

    #[test]
    fn a_line_from_elsewhere_prints_without_its_control_characters() {
        assert_eq!(printable("plain\ttext"), "plain\ttext");
        assert_eq!(printable("\x1b]52;c;ZWNobyBoaQ==\x07ok\x1b[2K"), "\\x1b]52;c;ZWNobyBoaQ==\\x07ok\\x1b[2K");
        assert_eq!(printable("a\u{9b}31mb"), "a\\x9b31mb");
    }

    #[test]
    fn masks_a_value_of_several_lines_line_by_line_and_as_json() {
        let pem = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\nAoIBAQC7VJTUt9Us8cKjMzEfYyjiWA4R\n-----END PRIVATE KEY-----\n";
        let env: BTreeMap<String, String> = [("KEY".to_owned(), pem.to_owned())].into_iter().collect();
        let mask = Mask::new(&env);
        for line in pem.lines() {
            assert_eq!(mask.apply(line), "[KEY hidden]", "{line}");
        }
        let json = serde_json::to_string(pem).unwrap();
        assert!(!mask.apply(&format!("config: {json}")).contains("MIIEvQ"), "{json}");
    }

    #[test]
    fn masks_values_long_enough_to_be_secrets() {
        let env: BTreeMap<String, String> = [
            ("API_KEY", "sk_live_0123456789"),
            ("LONGER", "sk_live_0123456789abcdef"),
            ("PORT", "8080"),
            ("DEBUG", "true"),
            ("SAME", "sk_live_0123456789"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let mask = Mask::new(&env);
        let line = "key=sk_live_0123456789abcdef other=sk_live_0123456789 port 8080 debug true";
        let masked = mask.apply(line);
        assert!(!masked.contains("sk_live"), "{masked}");
        assert!(masked.starts_with("key=[LONGER hidden] other=["), "{masked}");
        assert!(masked.ends_with("hidden] port 8080 debug true"), "{masked}");
        assert_eq!(Mask::default().apply(line), line);
    }
}
