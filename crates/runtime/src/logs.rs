//! The app's output, kept for the router.
//!
//! The runtime copies every line the app prints to its own stderr, as before, and into a bounded
//! buffer that lives as long as the execution environment. The router reads the buffer through one
//! long-lived request per tunnel (`GET /_forkrail/logs`), which isn't counted as in flight. Lines
//! printed while no tunnel is open (during init, or while frozen between invocations) wait in the
//! buffer for the next one. When the buffer is full, the oldest lines go, and the next batch says
//! how many.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use forkrail_common::LogLine;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use tokio::sync::{Notify, mpsc, watch};

use crate::app::AppBody;

/// What the buffer holds at most, in bytes of line text: four times what one request may log
/// (1 MiB, as on Vercel), for output that comes faster than the router reads it, or while no
/// tunnel is open.
const CAPACITY: usize = 4 * 1024 * 1024;
/// Longer lines are cut to this, in bytes: Vercel's limit for one line.
pub const MAX_LINE: usize = 256 * 1024;
/// The most one frame of the log stream carries, in bytes of line text (at least one line).
const BATCH: usize = 256 * 1024;

static BUFFER: LazyLock<LogBuffer> = LazyLock::new(|| LogBuffer::new(CAPACITY));

/// Keep one line of the app's output for the router, with the request it was written for.
pub fn push(stream: &'static str, line: String, request: Option<String>) {
    BUFFER.push(stream, line, request);
}

/// The shims start each line an app writes while serving a request with the router's request id
/// between these two control characters (`\x1e<id>\x1f`).
const TAG_START: char = '\u{1e}';
const TAG_END: char = '\u{1f}';
const MAX_TAG: usize = 64;

/// A line without its request tag, and the request it names. A line that only looks tagged (an id
/// that's too long or has other characters) is left as it is.
pub fn split_tag(line: String) -> (String, Option<String>) {
    let Some(rest) = line.strip_prefix(TAG_START) else { return (line, None) };
    let Some(end) = rest.find(TAG_END) else { return (line, None) };
    let id = &rest[..end];
    if id.is_empty() || id.len() > MAX_TAG || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return (line, None);
    }
    let id = id.to_owned();
    let text = rest[end + TAG_END.len_utf8()..].to_owned();
    (text, Some(id))
}

struct Entry {
    seq: u64,
    ts: f64,
    stream: &'static str,
    line: String,
    request: Option<String>,
}

struct Inner {
    lines: VecDeque<Entry>,
    bytes: usize,
    /// Lines dropped since the router last heard.
    dropped: u64,
    next_seq: u64,
}

pub struct LogBuffer {
    capacity: usize,
    inner: Mutex<Inner>,
    changed: Notify,
}

/// Lines encoded for the stream, not yet removed from the buffer.
struct Batch {
    bytes: Bytes,
    /// Entries before this sequence number are in it.
    upto: u64,
    dropped: u64,
}

impl LogBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner { lines: VecDeque::new(), bytes: 0, dropped: 0, next_seq: 0 }),
            changed: Notify::new(),
        }
    }

    fn push(&self, stream: &'static str, line: String, request: Option<String>) {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let mut inner = self.inner.lock().unwrap();
        while inner.bytes + line.len() > self.capacity {
            let Some(old) = inner.lines.pop_front() else { break };
            inner.bytes -= old.line.len();
            inner.dropped += 1;
        }
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.bytes += line.len();
        inner.lines.push_back(Entry { seq, ts, stream, line, request });
        drop(inner);
        self.changed.notify_waiters();
    }

    /// The oldest lines, as JSON lines, up to `BATCH` bytes of text (at least one line). They stay
    /// in the buffer until `commit`, so a stream that breaks before taking them loses nothing.
    fn peek(&self) -> Option<Batch> {
        let inner = self.inner.lock().unwrap();
        if inner.lines.is_empty() && inner.dropped == 0 {
            return None;
        }
        let mut out = Vec::new();
        if inner.dropped > 0 {
            encode(&mut out, &LogLine::Dropped { dropped: inner.dropped });
        }
        let mut upto = inner.lines.front().map_or(inner.next_seq, |e| e.seq);
        let mut text = 0;
        for entry in &inner.lines {
            if text > 0 && text + entry.line.len() > BATCH {
                break;
            }
            text += entry.line.len();
            encode(
                &mut out,
                &LogLine::Line { ts: entry.ts, stream: entry.stream.to_owned(), line: entry.line.clone(), request: entry.request.clone() },
            );
            upto = entry.seq + 1;
        }
        Some(Batch { bytes: out.into(), upto, dropped: inner.dropped })
    }

    /// Remove what `batch` carried.
    fn commit(&self, batch: &Batch) {
        let mut inner = self.inner.lock().unwrap();
        while inner.lines.front().is_some_and(|e| e.seq < batch.upto) {
            let entry = inner.lines.pop_front().expect("checked");
            inner.bytes -= entry.line.len();
        }
        inner.dropped = inner.dropped.saturating_sub(batch.dropped);
    }

    /// Stream the buffer's lines as they come, until `closing` turns true or the router drops the
    /// stream. What's left stays for the next stream. With `background`, the stream also says how
    /// much `waitUntil` work is running whenever that changes (and at its start, if there is any):
    /// before the lines waiting, since the router decides by it whether to keep the worker.
    async fn pump(&self, tx: mpsc::Sender<Bytes>, mut closing: watch::Receiver<bool>, mut background: Option<watch::Receiver<u64>>, mut net: Option<watch::Receiver<u64>>) {
        let mut told = 0;
        // The environment's bytes out so far (`net.rs`), as told.
        let mut net_told = net.as_ref().map_or(0, |n| *n.borrow());
        loop {
            if *closing.borrow() {
                return;
            }
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(total) = net.as_mut().map(|n| *n.borrow_and_update()).filter(|total| *total > net_told) {
                let mut out = Vec::new();
                encode(&mut out, &LogLine::Net { net_out: total - net_told });
                tokio::select! {
                    sent = tx.send(out.into()) => if sent.is_err() { return },
                    _ = closing.changed() => return,
                }
                net_told = total;
                continue;
            }
            if let Some(now) = background.as_mut().map(|b| *b.borrow_and_update()).filter(|now| *now != told) {
                let mut out = Vec::new();
                encode(&mut out, &LogLine::Background { background: now });
                tokio::select! {
                    sent = tx.send(out.into()) => if sent.is_err() { return },
                    _ = closing.changed() => return,
                }
                told = now;
                continue;
            }
            let background_changed = async {
                let net_changed = async {
                    match net.as_mut() {
                        Some(n) => n.changed().await.is_ok(),
                        None => std::future::pending().await,
                    }
                };
                let background_changed = async {
                    match background.as_mut() {
                        Some(b) => b.changed().await.is_ok(),
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    changed = net_changed => changed,
                    changed = background_changed => changed,
                }
            };
            match self.peek() {
                Some(batch) => {
                    let permit = tokio::select! {
                        permit = tx.reserve() => match permit {
                            Ok(permit) => permit,
                            Err(_) => return,
                        },
                        _ = closing.changed() => return,
                        // The router reads slowly: what it must know first goes first.
                        true = background_changed => continue,
                    };
                    permit.send(batch.bytes.clone());
                    self.commit(&batch);
                }
                None => tokio::select! {
                    _ = notified => {}
                    _ = tx.closed() => return,
                    _ = closing.changed() => return,
                    true = background_changed => {}
                },
            }
        }
    }
}

fn encode(out: &mut Vec<u8>, line: &LogLine) {
    serde_json::to_writer(&mut *out, line).expect("log lines serialize");
    out.push(b'\n');
}

/// The body of the log stream: the buffer's lines until the worker closes. Ends cleanly when the
/// worker shuts down on its own, so the tunnel's graceful shutdown doesn't wait on it.
pub fn stream(closing: watch::Receiver<bool>, background: Option<watch::Receiver<u64>>, net: Option<watch::Receiver<u64>>) -> AppBody {
    // One batch in hand: lines leave the buffer only once hyper takes them.
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(BUFFER.pump(tx, closing, background, net));
    LogBody(rx).map_err(|never| match never {}).boxed()
}

struct LogBody(mpsc::Receiver<Bytes>);

impl Body for LogBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.get_mut().0.poll_recv(cx).map(|bytes| bytes.map(|b| Ok(Frame::data(b))))
    }
}

/// Splits the app's output into lines, cutting long ones at `MAX_LINE` bytes.
#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
    /// Bytes of the current line past `MAX_LINE`, left out.
    cut: usize,
}

impl LineSplitter {
    pub fn feed(&mut self, data: &[u8], mut emit: impl FnMut(String)) {
        for chunk in data.split_inclusive(|&b| b == b'\n') {
            let complete = chunk.last() == Some(&b'\n');
            let text = if complete { &chunk[..chunk.len() - 1] } else { chunk };
            let room = MAX_LINE - self.buf.len();
            if text.len() <= room {
                self.buf.extend_from_slice(text);
            } else {
                self.buf.extend_from_slice(&text[..room]);
                self.cut += text.len() - room;
            }
            if complete {
                emit(self.take());
            }
        }
    }

    /// At the end of the output: the last line, if it had no newline.
    pub fn finish(&mut self, mut emit: impl FnMut(String)) {
        if !self.buf.is_empty() || self.cut > 0 {
            emit(self.take());
        }
    }

    fn take(&mut self) -> String {
        if self.buf.last() == Some(&b'\r') {
            self.buf.pop();
        }
        let mut line = String::from_utf8_lossy(&self.buf).into_owned();
        if self.cut > 0 {
            line.push_str(&format!(" [cut {} bytes]", self.cut));
        }
        self.buf.clear();
        self.cut = 0;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(splitter: &mut LineSplitter, data: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        splitter.feed(data, |l| out.push(l));
        out
    }

    #[test]
    fn splits_lines_across_reads_and_cuts_long_ones() {
        let mut s = LineSplitter::default();
        assert_eq!(lines(&mut s, b"one\ntw"), ["one"]);
        assert_eq!(lines(&mut s, b"o\r\nthree\n"), ["two", "three"]);
        let long = vec![b'x'; MAX_LINE + 10];
        let got = lines(&mut s, &long);
        assert!(got.is_empty());
        let got = lines(&mut s, b"yy\nz");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], format!("{} [cut 12 bytes]", "x".repeat(MAX_LINE)));
        let mut rest = Vec::new();
        s.finish(|l| rest.push(l));
        assert_eq!(rest, ["z"]);
        // Invalid UTF-8 doesn't lose the line.
        assert_eq!(lines(&mut s, b"caf\xc3\n"), ["caf\u{fffd}"]);
    }

    fn decode(batch: &Batch) -> Vec<LogLine> {
        std::str::from_utf8(&batch.bytes).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn overflow_drops_the_oldest_and_says_so() {
        let buffer = LogBuffer::new(10);
        for line in ["aaaa", "bbbb", "cccc"] {
            buffer.push("stdout", line.into(), None);
        }
        let batch = buffer.peek().unwrap();
        let got = decode(&batch);
        assert_eq!(got[0], LogLine::Dropped { dropped: 1 });
        assert!(matches!(&got[1], LogLine::Line { line, stream, .. } if line == "bbbb" && stream == "stdout"));
        assert_eq!(got.len(), 3);
        // A line pushed between peek and commit stays.
        buffer.push("stderr", "dd".into(), None);
        buffer.commit(&batch);
        let got = decode(&buffer.peek().unwrap());
        assert_eq!(got.len(), 1);
        assert!(matches!(&got[0], LogLine::Line { line, .. } if line == "dd"));
    }

    #[test]
    fn batches_are_bounded() {
        let buffer = LogBuffer::new(CAPACITY);
        for _ in 0..10 {
            buffer.push("stdout", "x".repeat(MAX_LINE), None);
        }
        let batch = buffer.peek().unwrap();
        assert_eq!(decode(&batch).len(), BATCH / MAX_LINE);
        buffer.commit(&batch);
        let mut left = 0;
        while let Some(batch) = buffer.peek() {
            left += decode(&batch).len();
            buffer.commit(&batch);
        }
        assert_eq!(left, 10 - BATCH / MAX_LINE);
    }

    #[tokio::test]
    async fn stream_ends_when_closing() {
        let buffer: &'static LogBuffer = Box::leak(Box::new(LogBuffer::new(CAPACITY)));
        buffer.push("stdout", "early".into(), None);
        let (closing_tx, closing) = watch::channel(false);
        let (tx, mut rx) = mpsc::channel(1);
        let pump = tokio::spawn(buffer.pump(tx, closing, None, None));
        let first = rx.recv().await.unwrap();
        assert!(std::str::from_utf8(&first).unwrap().contains("early"));
        buffer.push("stderr", "later".into(), None);
        assert!(std::str::from_utf8(&rx.recv().await.unwrap()).unwrap().contains("later"));
        closing_tx.send(true).unwrap();
        assert!(rx.recv().await.is_none());
        pump.await.unwrap();
        // Lines after the stream ended wait for the next one.
        buffer.push("stdout", "kept".into(), Some("r1".into()));
        assert!(buffer.peek().is_some());
    }

    #[test]
    fn request_tags_come_off() {
        assert_eq!(split_tag("\u{1e}a1b2\u{1f}hello".into()), ("hello".into(), Some("a1b2".into())));
        assert_eq!(split_tag("\u{1e}a1b2\u{1f}".into()), (String::new(), Some("a1b2".into())));
        assert_eq!(split_tag("plain".into()), ("plain".into(), None));
        // Not a tag: left alone.
        assert_eq!(split_tag("\u{1e}no end".into()), ("\u{1e}no end".into(), None));
        assert_eq!(split_tag("\u{1e}bad id!\u{1f}x".into()), ("\u{1e}bad id!\u{1f}x".into(), None));
        assert_eq!(split_tag("\u{1e}\u{1f}x".into()), ("\u{1e}\u{1f}x".into(), None));
    }

}
