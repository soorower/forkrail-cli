//! Who a function's call to its own project is for.
//!
//! Some frameworks fetch their own pages: Astro's `edgeMiddleware` runs the middleware, then
//! `fetch`es the page from the deployment's own URL with every header of the visitor's request
//! copied. On Vercel that call keeps the visitor's address and location; here it leaves the Lambda
//! and comes back through the accelerator, so the router saw a Lambda address (and `US`) as the
//! client, and set the app's `x-forwarded-for` and `x-vercel-ip-*` from it (seen on AWS
//! 2026-11-16).
//!
//! So the router vouches for each visitor in what it sends the app (`HEADER`: the visitor's
//! address, until when, and an HMAC of both, the project and its environment), and a request to
//! the same project and environment that carries a good one is treated as the visitor's own: its
//! address for the forwarded headers,
//! location, log, firewall and protection. Anything else in the header is ignored and replaced. The
//! key is derived from the routers' shared challenge key, so a call coming back to the other region
//! works too; with a key of a router's own it holds on that router only.
//!
//! A vouch someone gets hold of (an app that shows or logs its request headers) lets them pass for
//! that visitor on that project's environment until it expires (`LIFETIME_S` from the visitor's
//! own request), nothing more: a call that came with a vouch is sent on with one that expires when
//! that one did, never later, so passing it round doesn't keep it alive. A preview's code gets
//! vouches for the preview's visitors only, which production doesn't take (trusted addresses can
//! guard production alone).

use std::net::IpAddr;

use ring::hmac;

/// The header the router sends apps and reads back on their own calls.
pub const HEADER: &str = "x-forkrail-visitor";
/// How long a vouch holds: a function's own calls come within its request (5 minutes is
/// Vercel's default `maxDuration`).
pub const LIFETIME_S: u64 = 300;

pub struct Vouches {
    key: hmac::Key,
}

impl Vouches {
    pub fn new(key: hmac::Key) -> Self {
        Self { key }
    }

    /// With a key of this process's own.
    pub fn local() -> Self {
        let mut secret = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut secret).expect("the system's randomness");
        Self::new(hmac::Key::new(hmac::HMAC_SHA256, &secret))
    }

    /// `<address>~<expires>~<hex HMAC>` for the visitor at `ip` on `project`'s `environment`
    /// (`production`, `preview`, or empty for an entry that names none), holding until `expires`.
    pub fn vouch(&self, ip: IpAddr, project: &str, environment: &str, expires: u64) -> String {
        let ip = ip.to_canonical();
        let tag = hmac::sign(&self.key, message(ip, expires, project, environment).as_bytes());
        format!("{ip}~{expires}~{}", forkrail_common::hex(tag.as_ref()))
    }

    /// The visitor a vouch names and when it expires, if it's this router's, for `project`'s
    /// `environment`, and not expired.
    pub fn vouched(&self, value: &str, project: &str, environment: &str, now: u64) -> Option<(IpAddr, u64)> {
        let mut parts = value.trim().splitn(3, '~');
        let ip: IpAddr = parts.next()?.parse().ok()?;
        let expires: u64 = parts.next()?.parse().ok()?;
        let tag = unhex(parts.next()?)?;
        if expires < now || expires > now + LIFETIME_S {
            return None;
        }
        hmac::verify(&self.key, message(ip, expires, project, environment).as_bytes(), &tag).ok()?;
        Some((ip, expires))
    }
}

fn message(ip: IpAddr, expires: u64, project: &str, environment: &str) -> String {
    format!("forkrail-visitor\0{project}\0{environment}\0{ip}\0{expires}")
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const UNTIL: u64 = NOW + LIFETIME_S;

    #[test]
    fn a_vouch_holds_for_its_project_and_environment_until_it_expires() {
        let vouches = Vouches::local();
        let visitor: IpAddr = "203.0.113.9".parse().unwrap();
        let v = vouches.vouch(visitor, "fw-astro", "production", UNTIL);
        assert_eq!(vouches.vouched(&v, "fw-astro", "production", NOW), Some((visitor, UNTIL)));
        assert_eq!(vouches.vouched(&v, "fw-astro", "production", NOW + LIFETIME_S), Some((visitor, UNTIL)));
        assert_eq!(vouches.vouched(&v, "fw-astro", "production", NOW + LIFETIME_S + 1), None, "expired");
        assert_eq!(vouches.vouched(&v, "other", "production", NOW), None, "another project");
        // A preview's visitors don't pass for themselves on production, nor the other way round.
        assert_eq!(vouches.vouched(&v, "fw-astro", "preview", NOW), None, "another environment");
        let preview = vouches.vouch(visitor, "fw-astro", "preview", UNTIL);
        assert_eq!(vouches.vouched(&preview, "fw-astro", "production", NOW), None);
        let v6: IpAddr = "2001:db8::7".parse().unwrap();
        assert_eq!(vouches.vouched(&vouches.vouch(v6, "p", "", UNTIL), "p", "", NOW), Some((v6, UNTIL)));
        let mapped: IpAddr = "::ffff:198.51.100.4".parse().unwrap();
        assert_eq!(vouches.vouched(&vouches.vouch(mapped, "p", "", UNTIL), "p", "", NOW), Some(("198.51.100.4".parse().unwrap(), UNTIL)));
    }

    #[test]
    fn nothing_else_is_a_vouch() {
        let vouches = Vouches::local();
        let v = vouches.vouch("203.0.113.9".parse().unwrap(), "p", "", UNTIL);
        let (rest, tag) = v.rsplit_once('~').unwrap();
        let (_, expires) = rest.split_once('~').unwrap();
        // Another address, a later expiry, or the key of another router.
        assert_eq!(vouches.vouched(&format!("198.51.100.1~{expires}~{tag}"), "p", "", NOW), None);
        assert_eq!(vouches.vouched(&format!("203.0.113.9~{}~{tag}", NOW + 200), "p", "", NOW), None);
        assert_eq!(Vouches::local().vouched(&v, "p", "", NOW), None);
        // Too far ahead to be one of ours, whatever its tag.
        let far = Vouches { key: vouches.key.clone() }.vouch("203.0.113.9".parse().unwrap(), "p", "", NOW + 11 * LIFETIME_S);
        assert_eq!(vouches.vouched(&far, "p", "", NOW), None);
        for junk in ["", "~", "203.0.113.9", "203.0.113.9~x~y", "a~1~00", &format!("{rest}~{}", &tag[..62])] {
            assert_eq!(vouches.vouched(junk, "p", "", NOW), None, "{junk}");
        }
    }
}
