//! JA4: a fingerprint of the TLS client, from its ClientHello (FoxIO's JA4 for TLS clients,
//! BSD 3-Clause; written here from its specification,
//! https://github.com/FoxIO-LLC/ja4/blob/main/technical_details/JA4.md). The other JA4+
//! fingerprints are under a non-commercial licence and aren't used.
//!
//! `t13d1516h2_8daaf6152771_e5627efa2ab1`: TCP, the highest TLS version offered, SNI or not, how
//! many cipher suites and extensions, the first ALPN's first and last characters; then a hash of the
//! cipher suites sorted, and a hash of the extensions sorted (without SNI and ALPN) followed by the
//! signature algorithms in their order. GREASE values are left out everywhere. A browser, a script
//! library or a scanner each have their own, whatever user agent they claim, and it doesn't change
//! with the client's address.
//!
//! The router reads the ClientHello off the connection before rustls does (`read_hello`), keeps the
//! bytes, and hands them to rustls first (`Rewind`): the handshake is untouched. A connection that
//! doesn't start with a ClientHello gets no fingerprint, and rustls refuses it as before.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

/// The most read before giving up on a ClientHello (post-quantum key shares make them about 2 KB).
const MAX_HELLO: usize = 64 << 10;

/// Read the ClientHello from `io`: the bytes read (to give to rustls) and the fingerprint, if they
/// were one.
pub async fn read_hello<S: AsyncRead + Unpin>(io: &mut S) -> io::Result<(Vec<u8>, Option<String>)> {
    let mut buf = Vec::with_capacity(2048);
    let mut message = Vec::new();
    loop {
        // One record: a 5-byte header, then its payload.
        let start = buf.len();
        if !fill(io, &mut buf, start + 5).await? {
            return Ok((buf, None));
        }
        let header = &buf[start..start + 5];
        if header[0] != 0x16 {
            return Ok((buf, None));
        }
        let len = u16::from_be_bytes([header[3], header[4]]) as usize;
        if len == 0 || buf.len() + len > MAX_HELLO || !fill(io, &mut buf, start + 5 + len).await? {
            return Ok((buf, None));
        }
        message.extend_from_slice(&buf[start + 5..start + 5 + len]);
        // The handshake message: type 1 (ClientHello) and a 3-byte length; it may span records.
        if message.len() >= 4 {
            if message[0] != 1 {
                return Ok((buf, None));
            }
            let need = 4 + (u32::from_be_bytes([0, message[1], message[2], message[3]]) as usize);
            if need > MAX_HELLO {
                return Ok((buf, None));
            }
            if message.len() >= need {
                let fingerprint = fingerprint(&message[4..need]);
                return Ok((buf, fingerprint));
            }
        }
    }
}

/// The server name (SNI) asked for in the ClientHello `read_hello` read (`records`, as it read
/// them): the host name, lowercased, without a trailing dot. None when there's none, or the bytes
/// aren't a ClientHello. For starting a worker before the handshake ends (`early.rs`).
pub fn server_name(records: &[u8]) -> Option<String> {
    let mut message = Vec::new();
    let mut rest = records;
    while rest.len() >= 5 && rest[0] == 0x16 {
        let len = u16::from_be_bytes([rest[3], rest[4]]) as usize;
        message.extend_from_slice(rest.get(5..5 + len)?);
        rest = &rest[5 + len..];
    }
    if message.len() < 4 || message[0] != 1 {
        return None;
    }
    let need = 4 + u32::from_be_bytes([0, message[1], message[2], message[3]]) as usize;
    let mut r = Reader(message.get(4..need)?);
    r.take(2 + 32)?; // version, random
    r.vec(1)?; // session id
    r.vec(2)?; // cipher suites
    r.vec(1)?; // compression methods
    let mut ext = Reader(r.vec(2)?);
    while !ext.0.is_empty() {
        let kind = ext.u16()?;
        let data = ext.vec(2)?;
        if kind != 0x0000 {
            continue;
        }
        let mut names = Reader(Reader(data).vec(2)?);
        while !names.0.is_empty() {
            let (name_type, name) = (names.u8()?, names.vec(2)?);
            if name_type == 0 {
                return std::str::from_utf8(name).ok().map(|n| n.trim_end_matches('.').to_ascii_lowercase()).filter(|n| !n.is_empty());
            }
        }
        return None;
    }
    None
}

/// Read until `buf` holds `want` bytes; false if the connection ended first.
async fn fill<S: AsyncRead + Unpin>(io: &mut S, buf: &mut Vec<u8>, want: usize) -> io::Result<bool> {
    while buf.len() < want {
        let have = buf.len();
        buf.resize(want, 0);
        let n = io.read(&mut buf[have..]).await?;
        buf.truncate(have + n);
        if n == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// GREASE values (RFC 8701): 0x0a0a, 0x1a1a ... 0xfafa.
fn grease(v: u16) -> bool {
    v & 0x0f0f == 0x0a0a && v >> 8 == v & 0xff
}

/// A cursor over a ClientHello.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    /// A vector with a `len_bytes`-byte length in front.
    fn vec(&mut self, len_bytes: usize) -> Option<&'a [u8]> {
        let len = match len_bytes {
            1 => self.u8()? as usize,
            _ => self.u16()? as usize,
        };
        self.take(len)
    }
}

fn u16s(bytes: &[u8]) -> impl Iterator<Item = u16> + '_ {
    bytes.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]]))
}

/// The JA4 of a ClientHello's body (after the 4-byte handshake header).
pub fn fingerprint(body: &[u8]) -> Option<String> {
    let mut r = Reader(body);
    let legacy_version = r.u16()?;
    r.take(32)?; // random
    r.vec(1)?; // session id
    let ciphers: Vec<u16> = u16s(r.vec(2)?).filter(|c| !grease(*c)).collect();
    r.vec(1)?; // compression methods
    let mut extensions = Vec::new();
    let (mut versions, mut sni, mut alpn, mut signatures) = (None, false, None, Vec::new());
    if !r.0.is_empty() {
        let mut ext = Reader(r.vec(2)?);
        while !ext.0.is_empty() {
            let kind = ext.u16()?;
            let data = ext.vec(2)?;
            if grease(kind) {
                continue;
            }
            extensions.push(kind);
            match kind {
                0x0000 => sni = true,
                0x0010 => alpn = Reader(data).vec(2).and_then(|list| Reader(list).vec(1)).map(<[u8]>::to_vec),
                0x000d => signatures = Reader(data).vec(2).map(|list| u16s(list).filter(|s| !grease(*s)).collect()).unwrap_or_default(),
                0x002b => versions = Reader(data).vec(1).map(|list| u16s(list).filter(|v| !grease(*v)).max()),
                _ => {}
            }
        }
    }
    let version = match versions.flatten().unwrap_or(legacy_version) {
        0x0304 => "13",
        0x0303 => "12",
        0x0302 => "11",
        0x0301 => "10",
        0x0300 => "s3",
        0x0002 => "s2",
        0xfeff => "d1",
        0xfefd => "d2",
        0xfefc => "d3",
        _ => "00",
    };
    let a = format!(
        "t{version}{}{:02}{:02}{}",
        if sni { 'd' } else { 'i' },
        ciphers.len().min(99),
        extensions.len().min(99),
        alpn_chars(alpn.as_deref())
    );
    let hex = |list: &[u16]| list.iter().map(|v| format!("{v:04x}")).collect::<Vec<_>>().join(",");
    let mut sorted_ciphers = ciphers.clone();
    sorted_ciphers.sort_unstable();
    let b = if sorted_ciphers.is_empty() { "000000000000".to_owned() } else { short_hash(&hex(&sorted_ciphers)) };
    let mut sorted_extensions: Vec<u16> = extensions.iter().copied().filter(|e| *e != 0x0000 && *e != 0x0010).collect();
    sorted_extensions.sort_unstable();
    let c = if sorted_extensions.is_empty() {
        "000000000000".to_owned()
    } else if signatures.is_empty() {
        short_hash(&hex(&sorted_extensions))
    } else {
        short_hash(&format!("{}_{}", hex(&sorted_extensions), hex(&signatures)))
    };
    Some(format!("{a}_{b}_{c}"))
}

/// The first ALPN value's first and last characters, as the specification says.
fn alpn_chars(first: Option<&[u8]>) -> String {
    let Some(value) = first.filter(|v| !v.is_empty()) else { return "00".to_owned() };
    let (head, tail) = (value[0], value[value.len() - 1]);
    if head.is_ascii_alphanumeric() && tail.is_ascii_alphanumeric() {
        format!("{}{}", head as char, tail as char)
    } else {
        let h = format!("{head:02x}");
        let t = format!("{tail:02x}");
        format!("{}{}", &h[..1], &t[1..])
    }
}

fn short_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    forkrail_common::hex(&digest[..6])
}

/// A stream that gives back bytes already read before reading on.
pub struct Rewind<S> {
    prefix: Vec<u8>,
    at: usize,
    inner: S,
}

impl<S> Rewind<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self { prefix, at: 0, inner }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Rewind<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.at < self.prefix.len() {
            let n = buf.remaining().min(self.prefix.len() - self.at);
            let at = self.at;
            buf.put_slice(&self.prefix[at..at + n]);
            self.at += n;
            if self.at == self.prefix.len() {
                self.prefix = Vec::new();
                self.at = 0;
            }
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Rewind<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ClientHello body with these values, GREASE sprinkled in as Chrome does.
    fn hello(ciphers: &[u16], extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut b = vec![0x03, 0x03];
        b.extend([7u8; 32]);
        b.push(32);
        b.extend([9u8; 32]);
        let cs: Vec<u8> = ciphers.iter().flat_map(|c| c.to_be_bytes()).collect();
        b.extend((cs.len() as u16).to_be_bytes());
        b.extend(cs);
        b.extend([1, 0]);
        let mut ext = Vec::new();
        for (kind, data) in extensions {
            ext.extend(kind.to_be_bytes());
            ext.extend((data.len() as u16).to_be_bytes());
            ext.extend(data);
        }
        b.extend((ext.len() as u16).to_be_bytes());
        b.extend(ext);
        b
    }

    fn list16(values: &[u16]) -> Vec<u8> {
        let body: Vec<u8> = values.iter().flat_map(|v| v.to_be_bytes()).collect();
        [(body.len() as u16).to_be_bytes().to_vec(), body].concat()
    }

    fn alpn(protocols: &[&[u8]]) -> Vec<u8> {
        let body: Vec<u8> = protocols.iter().flat_map(|p| [vec![p.len() as u8], p.to_vec()].concat()).collect();
        [(body.len() as u16).to_be_bytes().to_vec(), body].concat()
    }

    /// The specification's example: Chrome's ClientHello, in its original order (JA4_ro), with
    /// GREASE added.
    fn chrome() -> Vec<u8> {
        let ciphers = [0x1a1a, 0x1301, 0x1302, 0x1303, 0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8, 0xc013, 0xc014, 0x009c, 0x009d, 0x002f, 0x0035];
        let sni = {
            let name = b"example.com";
            let entry = [vec![0u8], (name.len() as u16).to_be_bytes().to_vec(), name.to_vec()].concat();
            [(entry.len() as u16).to_be_bytes().to_vec(), entry].concat()
        };
        let versions = [vec![6u8], [0x3a3au16, 0x0304, 0x0303].iter().flat_map(|v| v.to_be_bytes()).collect()].concat();
        let order: Vec<(u16, Vec<u8>)> = vec![
            (0x2a2a, vec![]),
            (0x001b, vec![2, 0, 2]),
            (0x0000, sni),
            (0x0033, vec![0, 0]),
            (0x0010, alpn(&[b"h2", b"http/1.1"])),
            (0x4469, vec![0, 3, 2, b'h', b'2']),
            (0x0017, vec![]),
            (0x002d, vec![1, 1]),
            (0x000d, list16(&[0x0403, 0x0804, 0x0401, 0x0503, 0x0805, 0x0501, 0x0806, 0x0601])),
            (0x0005, vec![1, 0, 0, 0, 0]),
            (0x0023, vec![]),
            (0x0012, vec![]),
            (0x002b, versions),
            (0xff01, vec![0]),
            (0x000b, vec![1, 0]),
            (0x000a, list16(&[0x4a4a, 0x001d, 0x0017])),
            (0x0015, vec![0; 4]),
            (0xbaba, vec![0]),
        ];
        hello(&ciphers, &order)
    }

    #[test]
    fn the_specifications_example() {
        assert_eq!(fingerprint(&chrome()).unwrap(), "t13d1516h2_8daaf6152771_e5627efa2ab1");
    }

    #[test]
    fn the_specifications_hashes_and_edge_cases() {
        assert_eq!(short_hash("002f,0035,009c,009d,1301,1302,1303,c013,c014,c02b,c02c,c02f,c030,cca8,cca9"), "8daaf6152771");
        assert_eq!(short_hash("0005,000a,000b,000d,0012,0015,0017,001b,0023,002b,002d,0033,4469,ff01"), "6d807ffa2a79");
        // ALPN as the specification lists it.
        for (value, want) in [
            (&b"h2"[..], "h2"),
            (b"http/1.1", "h1"),
            (b"h", "hh"),
            (b"", "00"),
            (&[0xab], "ab"),
            (&[0x20], "20"),
            (&[0xab, 0xcd], "ad"),
            (&[0x20, 0x61], "21"),
            (&[0x30, 0xab], "3b"),
            (&[0x61, 0x20], "60"),
            (&[0x30, 0x31, 0xab, 0xcd], "3d"),
            (&[0x30, 0xab, 0xcd, 0x31], "01"),
        ] {
            assert_eq!(alpn_chars(Some(value)), want, "{value:?}");
        }
        assert_eq!(alpn_chars(None), "00");
        // No SNI, no ALPN, TLS 1.2 from the legacy version, no signature algorithms.
        let bare = hello(&[0x0a0a, 0xc02f, 0x00ff], &[(0x000a, list16(&[0x001d]))]);
        let j = fingerprint(&bare).unwrap();
        assert!(j.starts_with("t12i020100_"), "{j}");
        assert_eq!(j.split('_').nth(2).unwrap(), short_hash("000a"));
        // Nothing at all.
        assert_eq!(fingerprint(&hello(&[], &[])).unwrap(), "t12i000000_000000000000_000000000000");
        assert!(grease(0x0a0a) && grease(0xfafa) && !grease(0x0a1a) && !grease(0x1301));
        assert!(fingerprint(&[3, 3, 0]).is_none());
    }

    #[tokio::test]
    async fn the_hello_is_read_across_records_and_given_back() {
        let body = chrome();
        let mut message = vec![1u8];
        message.extend(&(body.len() as u32).to_be_bytes()[1..]);
        message.extend(&body);
        // Split into two records, as a client may.
        let (one, two) = message.split_at(40);
        let mut wire = Vec::new();
        for part in [one, two] {
            wire.extend([0x16, 0x03, 0x01]);
            wire.extend((part.len() as u16).to_be_bytes());
            wire.extend(part);
        }
        wire.extend(b"after");
        let mut src = &wire[..];
        let (read, ja4) = read_hello(&mut src).await.unwrap();
        assert_eq!(ja4.as_deref(), Some("t13d1516h2_8daaf6152771_e5627efa2ab1"));
        // The name it asks for, from the same bytes.
        assert_eq!(server_name(&read).as_deref(), Some("example.com"));
        let mut back = Vec::new();
        let mut rewound = Rewind::new(read, src);
        rewound.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, wire);
        // Not TLS: nothing, and every byte given back.
        let mut plain = &b"GET / HTTP/1.1\r\n\r\n"[..];
        let (read, ja4) = read_hello(&mut plain).await.unwrap();
        assert_eq!((read.as_slice(), ja4), (&b"GET /"[..], None));
        assert_eq!(server_name(&read), None);
        // A hello without SNI names nothing; a cut-off one is no hello.
        let bare = hello(&[0xc02f], &[(0x000a, list16(&[0x001d]))]);
        let mut message = vec![1u8];
        message.extend(&(bare.len() as u32).to_be_bytes()[1..]);
        message.extend(&bare);
        let record = [vec![0x16, 0x03, 0x01], (message.len() as u16).to_be_bytes().to_vec(), message].concat();
        assert_eq!(server_name(&record), None);
        assert_eq!(server_name(&wire[..30]), None);
    }

    #[test]
    fn quickly() {
        let body = chrome();
        let n = 20_000;
        let start = std::time::Instant::now();
        for _ in 0..n {
            assert!(fingerprint(&body).is_some());
        }
        let each = start.elapsed() / n;
        eprintln!("ja4: {each:?} a ClientHello");
        if !cfg!(debug_assertions) {
            assert!(each < std::time::Duration::from_micros(20), "{each:?}");
        }
    }
}
