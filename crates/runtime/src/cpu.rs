//! CPU time of the runtime and the user's app, so the router can learn what each route costs in
//! CPU. Lambda gives the whole environment one CPU share in proportion to its memory, so the
//! runtime's work counts as well as the app's.

use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Samples kept per request; past this, every other one is dropped and the interval doubles.
const MAX_SAMPLES: usize = 256;
const SAMPLE_EVERY: Duration = Duration::from_millis(5);

/// CPU time used so far by the runtime and the app, in microseconds.
pub fn cpu_us() -> u64 {
    own_us() + app_us()
}

fn clock_us(clock: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec for the call to write into.
    if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

fn own_us() -> u64 {
    clock_us(libc::CLOCK_PROCESS_CPUTIME_ID)
}

/// The app's CPU time, all threads. Linux lets a process read any other's CPU clock.
#[cfg(target_os = "linux")]
fn app_us() -> u64 {
    let pid = crate::mem::app_pid();
    if pid == 0 {
        return 0;
    }
    let mut clock: libc::clockid_t = 0;
    // SAFETY: `clock` is a valid clockid_t for the call to write into.
    if unsafe { libc::clock_getcpuclockid(pid as libc::pid_t, &mut clock) } != 0 {
        return 0;
    }
    clock_us(clock)
}

/// macOS has no CPU clock for other processes, so local runs count only the runtime.
#[cfg(not(target_os = "linux"))]
fn app_us() -> u64 {
    0
}

/// Records a request's CPU use over time while it runs: `(elapsed_us, cpu_us)` since it arrived.
pub struct Sampler {
    started: Instant,
    cpu_start: u64,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Vec<(u64, u64)>>,
}

impl Sampler {
    pub fn start() -> Self {
        let started = Instant::now();
        let cpu_start = cpu_us();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut samples = Vec::new();
            let mut every = SAMPLE_EVERY;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {}
                    _ = &mut stopped => break,
                }
                samples.push((started.elapsed().as_micros() as u64, cpu_us().saturating_sub(cpu_start)));
                // The samples are cumulative, so dropping some loses detail, not totals.
                if samples.len() >= MAX_SAMPLES {
                    samples = samples.into_iter().skip(1).step_by(2).collect();
                    every *= 2;
                }
            }
            samples
        });
        Self { started, cpu_start, stop, task }
    }

    /// Stop sampling and format the samples, ending with one taken now.
    pub async fn finish(self) -> String {
        let _ = self.stop.send(());
        let mut samples = self.task.await.unwrap_or_default();
        samples.push((self.started.elapsed().as_micros() as u64, cpu_us().saturating_sub(self.cpu_start)));
        let parts: Vec<String> = samples.iter().map(|(t, cpu)| format!("{t}:{cpu}")).collect();
        parts.join(",")
    }
}
