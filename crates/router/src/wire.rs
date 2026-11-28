//! What crossed a TCP connection, as the kernel counted it.
//!
//! AWS bills the hop between a router and its Lambda workers by the byte ($0.01/GB each way),
//! and a request's body sizes miss much of it: TLS, HTTP/2 framing, headers, and the app's log
//! lines and call reports use the same tunnel. The kernel keeps both totals for every socket
//! anyway, so the router reads them once per worker, when its tunnel ends, and never counts a
//! read or a write itself.
//!
//! The totals are TCP payload: TLS records and everything inside them. TCP and IP headers and
//! bare acknowledgements, which the bill also counts (40-52 bytes a packet), aren't in them.
//! What's sent counts what's still queued too: the last read of a connection is usually the
//! moment its owner let go of it, before the end of the answer has gone out.
//!
//! Linux has them in `TCP_INFO` (since 4.2), macOS in `TCP_CONNECTION_INFO`, so a router run
//! locally records them too. Elsewhere nothing is recorded.

use std::os::fd::{AsFd, OwnedFd};

/// One connection's totals, from this end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bytes {
    pub received: u64,
    /// What this end sent, retransmissions included, and what it has queued to send (Linux); what
    /// was sent (macOS).
    pub sent: u64,
}

impl std::ops::AddAssign for Bytes {
    fn add_assign(&mut self, other: Self) {
        self.received += other.received;
        self.sent += other.sent;
    }
}

/// A second handle on a connection, for reading its totals once whoever owns the first has
/// dropped it: hyper owns a tunnel's stream and closes it on its own.
///
/// The connection isn't fully closed until this is dropped too. A side that shuts its writing
/// down first (as hyper does at the end of a tunnel) still sends its FIN at once, but one that
/// only drops its stream sends nothing until then: drop this as soon as the totals are read.
pub fn hold(socket: &impl AsFd) -> Option<OwnedFd> {
    socket.as_fd().try_clone_to_owned().ok()
}

/// The connection's totals so far: one system call. They stay readable after it closed, the
/// other end's reset included, for as long as a handle on it is open.
pub fn bytes(socket: &impl AsFd) -> Option<Bytes> {
    read(socket.as_fd())
}

#[cfg(target_os = "linux")]
fn read(fd: std::os::fd::BorrowedFd<'_>) -> Option<Bytes> {
    use std::os::fd::AsRawFd;
    let mut info = std::mem::MaybeUninit::<libc::tcp_info>::zeroed();
    let mut len = size_of::<libc::tcp_info>() as libc::socklen_t;
    // SAFETY: `info` is `len` writable bytes, and the kernel writes at most that many.
    let rc = unsafe { libc::getsockopt(fd.as_raw_fd(), libc::IPPROTO_TCP, libc::TCP_INFO, info.as_mut_ptr().cast(), &mut len) };
    // A kernel older than the totals fills in less of the struct.
    let has = |end: usize| rc == 0 && len as usize >= end;
    if !has(std::mem::offset_of!(libc::tcp_info, tcpi_segs_out)) {
        return None;
    }
    // SAFETY: zeroed to begin with, and every field is a plain integer.
    let info = unsafe { info.assume_init() };
    // What's still queued is counted with what's gone: it goes out after the owner lets go of the
    // socket, and nothing reads it again then. A connection the router closes as soon as it has
    // answered (a client's `Connection: close`) otherwise counts only what the other end had
    // acknowledged by that moment: next to nothing of a large answer.
    let queued = if has(std::mem::offset_of!(libc::tcp_info, tcpi_notsent_bytes) + 4) { u64::from(info.tcpi_notsent_bytes) } else { 0 };
    // Sent, retransmissions included (they leave the machine, and are billed); before 4.19, what
    // was acknowledged.
    let gone = if has(std::mem::offset_of!(libc::tcp_info, tcpi_bytes_sent) + 8) { info.tcpi_bytes_sent } else { info.tcpi_bytes_acked };
    Some(Bytes { received: info.tcpi_bytes_received, sent: gone + queued })
}

/// `struct tcp_connection_info` (netinet/tcp.h) is 112 bytes, with `tcpi_txbytes` at 64 and
/// `tcpi_rxbytes` at 88. libc's struct of that name gives each of its flag bits a whole word, so
/// its fields are at the wrong offsets: read by position.
#[cfg(target_os = "macos")]
fn read(fd: std::os::fd::BorrowedFd<'_>) -> Option<Bytes> {
    use std::os::fd::AsRawFd;
    let mut info = [0u64; 14];
    let mut len = size_of_val(&info) as libc::socklen_t;
    // SAFETY: `info` is `len` writable bytes, and the kernel writes at most that many.
    let rc = unsafe { libc::getsockopt(fd.as_raw_fd(), libc::IPPROTO_TCP, libc::TCP_CONNECTION_INFO, info.as_mut_ptr().cast(), &mut len) };
    (rc == 0 && len as usize == size_of_val(&info)).then(|| Bytes { received: info[11], sent: info[8] })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read(_: std::os::fd::BorrowedFd<'_>) -> Option<Bytes> {
    None
}

// The kernel's layout (`struct tcp_info` in linux/tcp.h), which only ever grows at the end.
#[cfg(target_os = "linux")]
const _: () = {
    assert!(std::mem::offset_of!(libc::tcp_info, tcpi_bytes_acked) == 120);
    assert!(std::mem::offset_of!(libc::tcp_info, tcpi_bytes_received) == 128);
    assert!(std::mem::offset_of!(libc::tcp_info, tcpi_notsent_bytes) == 144);
    assert!(std::mem::offset_of!(libc::tcp_info, tcpi_bytes_sent) == 200);
};

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) mod tests {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    use super::*;

    pub(crate) const UP: usize = 300_000;
    pub(crate) const DOWN: usize = 7_000;

    /// A connection over loopback whose first end has sent `UP` bytes and received `DOWN`.
    pub(crate) fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut b, _) = listener.accept().unwrap();
        let reader = std::thread::spawn(move || {
            let mut got = vec![0; UP];
            b.read_exact(&mut got).unwrap();
            b.write_all(&vec![7; DOWN]).unwrap();
            b
        });
        a.write_all(&vec![1; UP]).unwrap();
        let b = reader.join().unwrap();
        a.read_exact(&mut vec![0; DOWN]).unwrap();
        (a, b)
    }

    /// The totals count the payload, give or take the SYN and FIN, which some kernels count as a
    /// byte each.
    pub(crate) fn close_to(b: Bytes, received: usize, sent: usize) -> bool {
        let close = |got: u64, want: usize| (want as u64..=want as u64 + 2).contains(&got);
        close(b.received, received) && close(b.sent, sent)
    }

    /// Acknowledgements may still be on their way, so this waits for them.
    fn expect(socket: &impl AsFd, received: usize, sent: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let b = bytes(socket).expect("the kernel keeps a TCP connection's totals");
            if close_to(b, received, sent) {
                return;
            }
            assert!(Instant::now() < deadline, "{b:?}, expected {received} received and {sent} sent");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn both_ends_count_the_same_bytes() {
        let (a, b) = pair();
        expect(&a, DOWN, UP);
        expect(&b, UP, DOWN);
    }

    /// What a tunnel's end looks like: the stream's owner shuts it down and drops it, and the
    /// handle taken at the start still has the totals.
    #[test]
    fn a_held_socket_keeps_its_totals_after_the_stream_is_dropped() {
        let (a, b) = pair();
        let held = hold(&a).unwrap();
        a.shutdown(Shutdown::Write).unwrap();
        drop(a);
        expect(&held, DOWN, UP);
        // The FIN went out despite the handle: the other end reads the end of the stream.
        let mut b = b;
        assert_eq!(b.read(&mut [0; 16]).unwrap(), 0);
    }

    /// A worker that vanishes: the other end is gone, and the totals are read afterwards.
    #[test]
    fn totals_outlive_the_other_end() {
        let (a, b) = pair();
        let held = hold(&a).unwrap();
        drop(b);
        // Wait for the other end's FIN to arrive.
        let mut a = a;
        assert_eq!(a.read(&mut [0; 16]).unwrap(), 0);
        expect(&held, DOWN, UP);
        drop(a);
        expect(&held, DOWN, UP);
    }

    /// A connection the router closes as soon as it has answered (`Connection: close`): the end of
    /// the answer is still queued when it's read for the last time, the other end's window full,
    /// and it's counted with what's gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn what_is_still_queued_counts_as_sent() {
        use std::io::ErrorKind;
        use std::os::fd::AsRawFd;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // The accepted end takes a small window from the listener, and doesn't read.
        let small: libc::c_int = 4096;
        // SAFETY: a valid socket, and an int's worth of option.
        let rc = unsafe { libc::setsockopt(listener.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, (&raw const small).cast(), size_of::<libc::c_int>() as libc::socklen_t) };
        assert_eq!(rc, 0);
        let mut a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut b, _) = listener.accept().unwrap();
        a.set_nonblocking(true).unwrap();
        let mut written = 0;
        loop {
            match a.write(&[1; 65_536]) {
                Ok(n) => written += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        let held = hold(&a).unwrap();
        a.shutdown(Shutdown::Write).unwrap();
        drop(a);
        let got = bytes(&held).unwrap();
        assert!(close_to(got, 0, written), "{got:?}, {written} written");
        // Most of it hadn't gone: what the other end had acknowledged would have been far less.
        assert!(written > 64 * 1024, "{written}");
        let mut all = Vec::new();
        b.read_to_end(&mut all).unwrap();
        assert_eq!(all.len(), written);
        expect(&held, 0, written);
    }

    #[test]
    fn not_a_tcp_socket() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(bytes(&file), None);
    }

    #[test]
    fn totals_add_up() {
        let mut sum = Bytes::default();
        sum += Bytes { received: 5, sent: 7 };
        sum += Bytes { received: 1, sent: 2 };
        assert_eq!(sum, Bytes { received: 6, sent: 9 });
    }
}
