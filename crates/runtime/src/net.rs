//! What the environment sends to anywhere but its router: the app's own calls and uploads, which
//! AWS bills as internet egress ($0.09/GB) and which never pass the router, so only the runtime
//! can count them (spend protection, C15).
//!
//! While an invocation runs (Lambda freezes the environment between them, so nothing goes out
//! then), every `SAMPLE_EVERY` the bytes every interface but loopback has sent (`/proc/net/dev`)
//! are read, and what the runtime itself sent the router is taken off: the tunnel and the app's
//! connections to private addresses through the router (`egress.rs`), each by its kernel's count
//! (`TCP_INFO`). The rest, as the bytes it takes on the wire, goes to the router on the log stream
//! (`LogLine::Net`), only when it asked: a local worker shares its machine's network, whose counts
//! would be everyone's.
//!
//! The interface counts packets before they're cut to the wire's size (checked in a real Lambda
//! environment, 2026-11-19: its one interface, `vint_runtime`, counted 20 MB sent through the
//! tunnel as 0.57-0.60% more, in packets of about 29 KB, where the wire adds 3.6%). So the
//! router's share is taken off as the interface counts it: what the connection sent, plus a
//! packet's headers for each acknowledgement and each segment shorter than the MSS (each its own
//! packet), and for full segments as few packets as the largest the interface carries. That's
//! never more than the interface counted, so the rest never comes out short. What's left is then
//! counted as on the wire (headers per segment).
//!
//! It errs high: by a few hundredths of a percent of what the tunnel carries in bulk (its packets'
//! headers), by the acknowledgements of what the app downloads (which AWS bills too), and by calls
//! to AWS's own services in the region, which AWS doesn't charge for.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::watch;
use tracing::warn;

const SAMPLE_EVERY: Duration = Duration::from_secs(2);
const MSS: u64 = 1448;
/// IPv4 and TCP with timestamps, as each segment carries on the wire.
const HEADERS: u64 = 52;
/// The same and Ethernet's, as the interface counts each packet.
const PACKET_HEADERS: u64 = 66;
/// The most full segments one packet the interface counts can hold (64 KB).
const SEGMENTS_PER_PACKET: u64 = 65_536 / MSS;

/// The app's connections to private addresses through the router (`egress.rs`), live: a second
/// handle on each socket, and its counts when last read.
static TO_ROUTER: OnceLock<Mutex<HashMap<u64, (OwnedFd, Counts)>>> = OnceLock::new();
/// What those that have closed carried since they were last read.
static CLOSED: Mutex<u64> = Mutex::new(0);
static NEXT: AtomicU64 = AtomicU64::new(0);

fn to_router() -> &'static Mutex<HashMap<u64, (OwnedFd, Counts)>> {
    TO_ROUTER.get_or_init(Mutex::default)
}

/// A connection of the app's to the router: its bytes aren't the app's own egress.
pub struct ToRouter(u64);

pub fn to_router_socket(socket: &impl AsFd) -> Option<ToRouter> {
    let fd = socket.as_fd().try_clone_to_owned().ok()?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let counts = tunnel_counts(&fd).unwrap_or_default();
    to_router().lock().unwrap().insert(id, (fd, counts));
    Some(ToRouter(id))
}

impl Drop for ToRouter {
    fn drop(&mut self) {
        let Some((fd, last)) = to_router().lock().unwrap().remove(&self.0) else { return };
        if let Some(now) = tunnel_counts(&fd) {
            *CLOSED.lock().unwrap() += on_interface(&now.since(&last));
        }
    }
}

/// What the app's connections to the router put on the interface since the last call.
fn to_router_since() -> u64 {
    let mut out = std::mem::take(&mut *CLOSED.lock().unwrap());
    for (fd, last) in to_router().lock().unwrap().values_mut() {
        if let Some(now) = tunnel_counts(fd) {
            out += on_interface(&now.since(last));
            *last = now;
        }
    }
    out
}

/// Bytes sent to anywhere but the router, so far in this environment's life.
fn total() -> &'static watch::Sender<u64> {
    static TOTAL: OnceLock<watch::Sender<u64>> = OnceLock::new();
    TOTAL.get_or_init(|| watch::channel(0).0)
}

/// For the log stream.
pub fn watch() -> watch::Receiver<u64> {
    total().subscribe()
}

/// Every interface's bytes sent but loopback's, from `/proc/net/dev`'s text.
pub fn sent_on_interfaces(text: &str) -> Option<u64> {
    let mut sum = 0u64;
    let mut any = false;
    for line in text.lines().skip(2) {
        let (name, counts) = line.split_once(':')?;
        if name.trim() == "lo" {
            continue;
        }
        // Received: bytes packets errs drop fifo frame compressed multicast; then sent: bytes …
        let sent: u64 = counts.split_whitespace().nth(8)?.parse().ok()?;
        sum += sent;
        any = true;
    }
    any.then_some(sum)
}

/// What a connection's kernel says it sent: bytes (retransmissions too), segments, and those of
/// them with data.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Counts {
    bytes: u64,
    segments: u64,
    data_segments: u64,
}

impl Counts {
    fn since(&self, last: &Counts) -> Counts {
        Counts {
            bytes: self.bytes.saturating_sub(last.bytes),
            segments: self.segments.saturating_sub(last.segments),
            data_segments: self.data_segments.saturating_sub(last.data_segments),
        }
    }
}

/// The bytes the wire carries for `payload` bytes of TCP data.
fn on_wire(payload: u64) -> u64 {
    payload + payload.div_ceil(MSS) * HEADERS
}

/// The bytes a connection's `sent` put on the interface, at most: each acknowledgement and each
/// short segment one packet, full segments in packets as large as the interface takes.
fn on_interface(sent: &Counts) -> u64 {
    let acks = sent.segments.saturating_sub(sent.data_segments);
    let full = sent.data_segments.min(sent.bytes / MSS);
    let short = sent.data_segments - full;
    sent.bytes + (acks + short + full.div_ceil(SEGMENTS_PER_PACKET)) * PACKET_HEADERS
}

/// What a connection sent, as its kernel counts it.
#[cfg(target_os = "linux")]
fn tunnel_counts(fd: &OwnedFd) -> Option<Counts> {
    use std::os::fd::AsRawFd;
    let mut info = std::mem::MaybeUninit::<libc::tcp_info>::zeroed();
    let mut len = size_of::<libc::tcp_info>() as libc::socklen_t;
    // SAFETY: `info` is `len` writable bytes, and the kernel writes at most that many.
    let rc = unsafe { libc::getsockopt(fd.as_fd().as_raw_fd(), libc::IPPROTO_TCP, libc::TCP_INFO, info.as_mut_ptr().cast(), &mut len) };
    let has = |end: usize| len as usize >= end;
    if rc != 0 || !has(std::mem::offset_of!(libc::tcp_info, tcpi_data_segs_out) + 4) {
        return None;
    }
    // SAFETY: zeroed to begin with, and every field is a plain integer.
    let info = unsafe { info.assume_init() };
    // Retransmissions too, where the kernel says (5.5 and later).
    let bytes = if has(std::mem::offset_of!(libc::tcp_info, tcpi_bytes_sent) + 8) { info.tcpi_bytes_sent } else { info.tcpi_bytes_acked };
    Some(Counts { bytes, segments: info.tcpi_segs_out.into(), data_segments: info.tcpi_data_segs_out.into() })
}

#[cfg(not(target_os = "linux"))]
fn tunnel_counts(_: &OwnedFd) -> Option<Counts> {
    None
}

/// What one period adds, as the wire carries it: the interfaces' growth less what went to the
/// router (`to_router`, as the interface counted it), never below zero.
pub fn elsewhere(interfaces: u64, to_router: u64) -> u64 {
    match interfaces.saturating_sub(to_router) {
        0 => 0,
        rest => on_wire(rest),
    }
}

/// Count until `closing` turns true: `tunnel` is a second handle on the tunnel's socket.
pub async fn run(tunnel: OwnedFd, mut closing: watch::Receiver<bool>) {
    let read = || std::fs::read_to_string("/proc/net/dev").ok().as_deref().and_then(sent_on_interfaces);
    let Some(mut interfaces) = read() else {
        warn!("no /proc/net/dev here: the app's own traffic isn't counted");
        return;
    };
    let mut last = tunnel_counts(&tunnel).unwrap_or_default();
    // What went to the router before this invocation isn't this one's.
    to_router_since();
    let mut tick = tokio::time::interval(SAMPLE_EVERY);
    tick.tick().await;
    loop {
        let ending = tokio::select! {
            _ = tick.tick() => false,
            _ = closing.changed() => true,
        };
        let (Some(now), Some(counts)) = (read(), tunnel_counts(&tunnel)) else { return };
        let router = on_interface(&counts.since(&last)) + to_router_since();
        let more = elsewhere(now.saturating_sub(interfaces), router);
        (interfaces, last) = (now, counts);
        if more > 0 {
            total().send_modify(|n| *n += more);
        }
        if ending || *closing.borrow() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000      10    0    0    0     0          0         0     1000      10    0    0    0     0       0          0
vinternal_1: 5000  40    0    0    0     0          0         0     70000     60    0    0    0     0       0          0
vtarget_1: 100      1    0    0    0     0          0         0       300      2    0    0    0     0       0          0
";

    #[test]
    fn interfaces_but_loopback() {
        assert_eq!(sent_on_interfaces(DEV), Some(70_300));
        assert_eq!(sent_on_interfaces("header\nheader\n    lo: 1 1 0 0 0 0 0 0 5 1 0 0 0 0 0 0\n"), None, "loopback alone is nothing to count");
        assert_eq!(sent_on_interfaces("header\nheader\nbroken line\n"), None);
    }

    #[test]
    fn what_went_to_the_router_is_taken_off() {
        // Measured in a Lambda environment: 20 MB through the tunnel (TLS and HTTP/2 make it about
        // 21,011,000 bytes of TCP data) counted 21,090,188 on the interface. Taken off, it leaves
        // well under a tenth of a percent, counted as the function's own: never more than is real.
        let bulk = Counts { bytes: 21_011_000, segments: 14_520, data_segments: 14_511 };
        let router = on_interface(&bulk);
        assert!(router <= 21_090_188, "{router}: never more than the interface counted");
        let left = elsewhere(21_090_188, router);
        assert!(left < 21_011_000 / 300, "{left}: what the tunnel's packets' headers leave");
        // A small answer is one packet with its headers, and an acknowledgement another: exact.
        let small = Counts { bytes: 300, segments: 2, data_segments: 1 };
        assert_eq!(on_interface(&small), 300 + 2 * PACKET_HEADERS);
        assert_eq!(elsewhere(300 + 2 * PACKET_HEADERS, on_interface(&small)), 0);
        // The app uploading 50,000 bytes besides: counted as the wire carries them.
        let upload = 50_000 + 2 * PACKET_HEADERS;
        assert_eq!(elsewhere(on_interface(&small) + upload, on_interface(&small)), on_wire(upload));
        // Never below zero.
        assert_eq!(elsewhere(10, on_interface(&bulk)), 0);
    }
}
