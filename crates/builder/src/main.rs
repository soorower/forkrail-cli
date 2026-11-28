//! `forkrail-builder`: the install's builder, a Lambda function run from a container image with
//! Node 24, Python 3.13 and forkrail on Lambda's own Amazon Linux 2023 (deploy/aws/builder.sh,
//! deploy/install/builder.yaml). Each invocation builds one deployment (forkrail_cli::builder):
//! the control function or the CLI sends it, asynchronously, and Lambda reports how it ended to the
//! builds topic, which the control function publishes from (forkrail_cli::remote).
//!
//! Its role has no rights over the install: everything it reads and writes is through the URLs the
//! job brings, presigned for this build alone. It writes no CloudWatch logs; its lines go to the
//! deployment's build log.
//!
//! `forkrail-builder --job FILE` runs one job from a file and prints the outcome, for tests away
//! from Lambda (with FORKRAIL_BUILDER_ROOT set to a folder of its own).

use std::path::PathBuf;

use forkrail_cli::builder::{Builder, ROOT_ENV};
use forkrail_cli::remote::Job;
use forkrail_cli::state::now_ms;
use lambda_runtime::{Error, LambdaEvent, service_fn};

/// A job from a file gets Lambda's 15 minutes.
const LOCAL_DEADLINE_MS: u64 = 15 * 60 * 1000;

fn main() -> Result<(), Error> {
    // The project's code runs as this process's user: none of its /proc files (`environ` keeps
    // Lambda's keys for the role whatever is unset below; `mem`) may be read by it. A build's
    // processes, started by exec, are dumpable again.
    #[cfg(target_os = "linux")]
    // SAFETY: a plain syscall on this process.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        eprintln!("couldn't make the builder undumpable: {}", std::io::Error::last_os_error());
    }
    let root = std::env::var_os(ROOT_ENV).map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    // Where the project's tools keep their files (npm's cache, pip's, forkrail's own): all in the
    // scratch folder, the only one Lambda lets anything write to.
    // SAFETY: set before the runtime starts, while this is the only thread.
    unsafe {
        std::env::set_var("HOME", root.join("home"));
        std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
        std::env::set_var("TMPDIR", root.join("tmp"));
        std::env::set_var("npm_config_update_notifier", "false");
        std::env::set_var("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0");
        // The role's keys: the builder reads and writes through presigned URLs and has no use for
        // them, and every process it starts would inherit them (Lambda reports the end itself).
        for key in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_SECURITY_TOKEN", "AWS_CONTAINER_CREDENTIALS_FULL_URI", "AWS_CONTAINER_AUTHORIZATION_TOKEN"] {
            std::env::remove_var(key);
        }
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve(root))
}

async fn serve(root: PathBuf) -> Result<(), Error> {
    let bootstrap = std::env::var_os("FORKRAIL_BOOTSTRAP").map_or_else(|| PathBuf::from("/opt/forkrail/forkrail-runtime"), PathBuf::from);
    let python = std::env::var("FORKRAIL_PYTHON").unwrap_or_else(|_| "python3.13".to_owned());
    let builder: &'static Builder = Box::leak(Box::new(Builder::new(root, bootstrap, python)?));

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, path] = args.as_slice()
        && flag == "--job"
    {
        let job: Job = serde_json::from_slice(&std::fs::read(path)?)?;
        let outcome = builder.run(job, now_ms() + LOCAL_DEADLINE_MS).await;
        println!("{}", serde_json::to_string_pretty(&outcome)?);
        return Ok(());
    }
    lambda_runtime::run(service_fn(move |event: LambdaEvent<Job>| async move {
        let deadline = event.context.deadline;
        Ok::<_, Error>(builder.run(event.payload, deadline).await)
    }))
    .await
}
