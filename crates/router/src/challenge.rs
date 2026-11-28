//! The challenge: a page the router answers with instead of the deployment, whose script must do a
//! little work (a proof of work) before the browser may go on. It's for browsers: anything that
//! doesn't run JavaScript stays at the page.
//!
//! 1. The page (403, `x-forkrail-mitigated: challenge`) carries a seed the router signed: when it
//!    was made, how hard the work is, and (inside the signature, not shown) the client's network
//!    (its /24, or /48 for IPv6), user agent and host.
//! 2. The script looks for a number whose SHA-256 with the seed starts with `bits` zero bits (2^18
//!    tries on average by default: well under a second on a phone), and posts it to
//!    `/.well-known/forkrail/challenge`.
//! 3. The router checks the signature, that the seed is under 10 minutes old, still the same
//!    network, agent and host, and the work, then sets `_forkrail_challenge`: a pass for an hour,
//!    signed the same way. The page reloads, and every rule that would challenge lets it by.
//!
//! Nothing is stored: seeds and passes are checked by their signatures. The key is the same on
//! every router (`/forkrail/router/challenge-key` in SSM, 32 random bytes as hex) so a pass from
//! one is good on the other; reading the code doesn't let anyone make a pass without it. Without
//! the key a router makes its own, and passes are good on it alone until it restarts.

use std::net::IpAddr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, SET_COOKIE};
use hyper::{Response, StatusCode};
use ring::hmac;
use sha2::{Digest, Sha256};

use crate::proxy::ResponseBody;

/// Where the script posts its answer, on any host the router serves.
pub const PATH: &str = "/.well-known/forkrail/challenge";
/// The pass.
pub const COOKIE_NAME: &str = "_forkrail_challenge";
/// How long a pass lasts.
pub const PASS_S: u64 = 3600;
/// How long a seed may be worked on.
const SEED_S: u64 = 600;
pub const DEFAULT_BITS: u8 = 18;
pub const MIN_BITS: u8 = 8;
pub const MAX_BITS: u8 = 28;

pub struct Challenger {
    key: hmac::Key,
    bits: u8,
    /// Whether the key is the one every router shares.
    pub shared: bool,
}

/// Why an answer was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Malformed,
    Forged,
    Expired,
    /// Another network, agent or host than the seed was made for.
    Elsewhere,
    NotSolved,
}

impl Challenger {
    /// With the routers' shared key.
    pub fn new(secret: &[u8], bits: u8) -> Self {
        Self { key: hmac::Key::new(hmac::HMAC_SHA256, secret), bits: bits.clamp(MIN_BITS, MAX_BITS), shared: true }
    }

    /// With a key of this process's own.
    pub fn local(bits: u8) -> Self {
        let mut secret = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut secret).expect("the system's randomness");
        Self { shared: false, ..Self::new(&secret, bits) }
    }

    /// A key for something else, made from this one (`label` keeps the two apart): shared by every
    /// router when this one is.
    pub fn derive(&self, label: &[u8]) -> hmac::Key {
        hmac::Key::new(hmac::HMAC_SHA256, hmac::sign(&self.key, label).as_ref())
    }

    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// The signature of `payload` for this client: `purpose` keeps seeds and passes apart.
    fn sign(&self, purpose: &[u8], payload: &[u8], ip: IpAddr, agent: &str, host: &str) -> [u8; 16] {
        let mut ctx = hmac::Context::with_key(&self.key);
        for part in [purpose, payload, &network(ip), &Sha256::digest(agent.as_bytes())[..], host.as_bytes()] {
            ctx.update(&(part.len() as u32).to_be_bytes());
            ctx.update(part);
        }
        let mut out = [0u8; 16];
        out.copy_from_slice(&ctx.sign().as_ref()[..16]);
        out
    }

    fn verify(&self, purpose: &[u8], payload: &[u8], mac: &[u8], ip: IpAddr, agent: &str, host: &str) -> bool {
        same(&self.sign(purpose, payload, ip, agent, host), mac)
    }

    /// Whether the request carries a pass, good now for this client and host.
    pub fn passed(&self, headers: &HeaderMap, ip: IpAddr, agent: &str, host: &str, now: u64) -> bool {
        // Every pass the browser sends is tried: another app on the install's domain can set a decoy
        // for the whole domain, which may come first.
        cookies(headers, COOKIE_NAME).any(|value| {
            let Some((payload, mac)) = value.split_once('.') else { return false };
            let (Ok(payload), Ok(mac)) = (B64.decode(payload), B64.decode(mac)) else { return false };
            let Ok(until) = <[u8; 4]>::try_from(payload.as_slice()) else { return false };
            u32::from_be_bytes(until) as u64 > now && self.verify(b"pass", &payload, &mac, ip, agent, host)
        })
    }

    /// A seed for this client, made now.
    fn seed(&self, ip: IpAddr, agent: &str, host: &str, now: u64) -> String {
        let mut payload = [0u8; 16];
        payload[..4].copy_from_slice(&(now as u32).to_be_bytes());
        payload[4] = self.bits;
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut payload[5..]).expect("the system's randomness");
        let mac = self.sign(b"seed", &payload, ip, agent, host);
        format!("{}.{}", B64.encode(payload), B64.encode(&mac[..12]))
    }

    /// Check an answer (`seed=…&nonce=…`); the pass cookie to set if it holds.
    pub fn solve(&self, form: &str, ip: IpAddr, agent: &str, host: &str, now: u64, https: bool) -> Result<HeaderValue, Refused> {
        let field = |name: &str| form.split('&').find_map(|pair| pair.strip_prefix(name).and_then(|v| v.strip_prefix('=')));
        let (Some(seed), Some(nonce)) = (field("seed"), field("nonce")) else { return Err(Refused::Malformed) };
        if nonce.is_empty() || nonce.len() > 12 || !nonce.bytes().all(|b| b.is_ascii_digit()) || seed.len() > 64 {
            return Err(Refused::Malformed);
        }
        let (payload_text, mac_text) = seed.split_once('.').ok_or(Refused::Malformed)?;
        let (payload, mac) = (B64.decode(payload_text).map_err(|_| Refused::Malformed)?, B64.decode(mac_text).map_err(|_| Refused::Malformed)?);
        if payload.len() != 16 || mac.len() != 12 {
            return Err(Refused::Malformed);
        }
        let full = self.sign(b"seed", &payload, ip, agent, host);
        if !same(&full[..12], &mac) {
            // Ours, made for someone else, or not ours at all: the same signature can't tell.
            return Err(if self.made_here(&payload) { Refused::Elsewhere } else { Refused::Forged });
        }
        let made = u32::from_be_bytes(payload[..4].try_into().unwrap()) as u64;
        if made > now + 60 || now > made + SEED_S {
            return Err(Refused::Expired);
        }
        let bits = payload[4];
        if !leading_zeros(&Sha256::digest(format!("{seed}{nonce}").as_bytes()), bits) {
            return Err(Refused::NotSolved);
        }
        let until = ((now + PASS_S) as u32).to_be_bytes();
        let pass_mac = self.sign(b"pass", &until, ip, agent, host);
        let secure = if https { "; Secure" } else { "" };
        let cookie = format!("{COOKIE_NAME}={}.{}; Path=/; Max-Age={PASS_S}; HttpOnly; SameSite=Lax{secure}", B64.encode(until), B64.encode(pass_mac));
        Ok(HeaderValue::from_str(&cookie).expect("base64 and fixed text"))
    }

    /// Whether a seed's payload is well formed enough to have been made by a router (for the
    /// refusal's reason only).
    fn made_here(&self, payload: &[u8]) -> bool {
        payload.len() == 16 && (MIN_BITS..=MAX_BITS).contains(&payload[4])
    }

    /// The challenge page for this client.
    pub fn page(&self, ip: IpAddr, agent: &str, host: &str, now: u64) -> Response<ResponseBody> {
        let seed = self.seed(ip, agent, host, now);
        let html = PAGE.replace("{{SEED}}", &seed).replace("{{BITS}}", &self.bits.to_string()).replace("{{PATH}}", PATH).replace("{{HOST}}", &escape(host));
        let mut response = Response::new(Full::new(Bytes::from(html)).map_err(|never| match never {}).boxed());
        *response.status_mut() = StatusCode::FORBIDDEN;
        let h = response.headers_mut();
        h.insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        h.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
        h.insert("x-forkrail-mitigated", HeaderValue::from_static("challenge"));
        h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
        response
    }
}

/// The answer to a solved challenge: 204 with the pass.
pub fn solved(cookie: HeaderValue) -> Response<ResponseBody> {
    let mut response = Response::new(Full::new(Bytes::new()).map_err(|never| match never {}).boxed());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response.headers_mut().insert(SET_COOKIE, cookie);
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

/// The client's network: an IPv4 /24, an IPv6 /48 (a pass survives an address change within it).
fn network(ip: IpAddr) -> Vec<u8> {
    match ip.to_canonical() {
        IpAddr::V4(a) => a.octets()[..3].to_vec(),
        IpAddr::V6(a) => a.octets()[..6].to_vec(),
    }
}

/// Equal, in time that doesn't depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn leading_zeros(hash: &[u8], bits: u8) -> bool {
    let mut left = bits as u32;
    for byte in hash {
        if left == 0 {
            return true;
        }
        let z = byte.leading_zeros().min(left);
        if z < left.min(8) {
            return false;
        }
        left -= z;
    }
    left == 0
}

fn cookies<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(move |pair| pair.trim().strip_prefix(name).and_then(|rest| rest.strip_prefix('=')))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The page. Its script is SHA-256 over one 64-byte block (seed and number together stay under 56
/// bytes), checked against the router's in the tests.
const PAGE: &str = include_str!("challenge.html");

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_800_000_000;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// Do the work a browser would.
    fn work(seed: &str, bits: u8) -> String {
        (0u64..).map(|n| n.to_string()).find(|n| leading_zeros(&Sha256::digest(format!("{seed}{n}").as_bytes()), bits)).unwrap()
    }

    fn seed_of(page: &str) -> String {
        let at = page.find("var seed = \"").unwrap() + "var seed = \"".len();
        page[at..at + page[at..].find('"').unwrap()].to_owned()
    }

    fn body(r: Response<ResponseBody>) -> String {
        let bytes = futures_body(r);
        String::from_utf8(bytes).unwrap()
    }

    fn futures_body(r: Response<ResponseBody>) -> Vec<u8> {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async { r.into_body().collect().await.unwrap().to_bytes().to_vec() })
    }

    #[test]
    fn a_solved_seed_gives_a_pass_for_this_client_only() {
        let c = Challenger::new(&[7u8; 32], 12);
        let page = c.page(ip("203.0.113.9"), "Mozilla/5.0", "app.example.com", T);
        assert_eq!(page.status(), StatusCode::FORBIDDEN);
        let seed = seed_of(&body(page));
        assert!(seed.len() + 12 < 56, "seed and number fit one block: {}", seed.len());
        let nonce = work(&seed, 12);
        let form = format!("seed={seed}&nonce={nonce}");
        // Another number doesn't do.
        let wrong = (0u64..).map(|n| n.to_string()).find(|n| !leading_zeros(&Sha256::digest(format!("{seed}{n}").as_bytes()), 12)).unwrap();
        assert_eq!(c.solve(&format!("seed={seed}&nonce={wrong}"), ip("203.0.113.9"), "Mozilla/5.0", "app.example.com", T, true), Err(Refused::NotSolved));
        // Same /24, same agent and host: a pass.
        let cookie = c.solve(&form, ip("203.0.113.200"), "Mozilla/5.0", "app.example.com", T + 5, true).unwrap();
        let cookie = cookie.to_str().unwrap();
        assert!(cookie.contains("HttpOnly") && cookie.contains("Secure") && cookie.contains("Max-Age=3600"));
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_str(&format!("a=1; {}", cookie.split(';').next().unwrap())).unwrap());
        assert!(c.passed(&headers, ip("203.0.113.1"), "Mozilla/5.0", "app.example.com", T + 10));
        // Not for another network, agent or host, not after an hour, not with another key.
        assert!(!c.passed(&headers, ip("198.51.100.1"), "Mozilla/5.0", "app.example.com", T + 10));
        assert!(!c.passed(&headers, ip("203.0.113.1"), "curl/8", "app.example.com", T + 10));
        assert!(!c.passed(&headers, ip("203.0.113.1"), "Mozilla/5.0", "other.example.com", T + 10));
        assert!(!c.passed(&headers, ip("203.0.113.1"), "Mozilla/5.0", "app.example.com", T + 5 + PASS_S));
        assert!(!Challenger::new(&[8u8; 32], 12).passed(&headers, ip("203.0.113.1"), "Mozilla/5.0", "app.example.com", T + 10));
        // The seed itself: only for whom it was made, and only for 10 minutes.
        assert_eq!(c.solve(&form, ip("198.51.100.1"), "Mozilla/5.0", "app.example.com", T, true), Err(Refused::Elsewhere));
        assert_eq!(c.solve(&form, ip("203.0.113.9"), "Mozilla/5.0", "app.example.com", T + SEED_S + 1, true), Err(Refused::Expired));
        assert_eq!(c.solve("seed=x.y&nonce=1", ip("203.0.113.9"), "Mozilla/5.0", "app.example.com", T, true), Err(Refused::Malformed));
        assert_eq!(c.solve("nonce=1", ip("203.0.113.9"), "Mozilla/5.0", "app.example.com", T, true), Err(Refused::Malformed));
        // Over plain HTTP (the tester's port), no Secure.
        let plain = c.solve(&form, ip("203.0.113.9"), "Mozilla/5.0", "app.example.com", T, false).unwrap();
        assert!(!plain.to_str().unwrap().contains("Secure"));
    }

    #[test]
    fn leading_zero_bits_are_counted_across_bytes() {
        assert!(leading_zeros(&[0, 0b0001_1111, 0xff], 11));
        assert!(!leading_zeros(&[0, 0b0001_1111, 0xff], 12));
        assert!(leading_zeros(&[0xff], 0));
        assert!(leading_zeros(&[0, 0, 0, 0], 32));
    }

    #[test]
    fn the_page_shows_the_host_safely() {
        let c = Challenger::local(DEFAULT_BITS);
        let page = body(c.page(ip("192.0.2.1"), "x", "a<b>.example.com", T));
        assert!(page.contains("a&lt;b&gt;.example.com") && !page.contains("a<b>"));
        assert!(page.contains(PATH) && page.contains("var bits = 18;"));
    }
}
