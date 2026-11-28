//! What the router and the runtime share for the loop guard (the router's `loops.rs`).
//!
//! A request a function makes back to its own install (its own project or another one, by a
//! deployment's host or a custom domain) carries the mark of the request it was made for, so the
//! router can tell how deep a chain of such calls has gone and answer 508 past a limit, as Vercel
//! answers `INFINITE_LOOP_DETECTED`. The shims add the mark only to calls to the install's own
//! hosts, which the router lists in each worker's payload (`InvokePayload::own_hosts`) and the
//! runtime writes to a file the shims read: a call anywhere else goes out as the app made it.

/// The header: `<chain>.<depth>.<hex HMAC>`, from the router only.
pub const HEADER: &str = "x-forkrail-loop";

/// The app's variable naming the file of the install's own hosts, one per line: a name, or
/// `*.<domain>` for every name under that domain.
pub const OWN_HOSTS_FILE_ENV: &str = "FORKRAIL_OWN_HOSTS_FILE";

/// Whether `host` (a name, any case, without a port) is one of `own`'s entries, by its name or
/// under a `*.` entry's domain (at any depth: everything under the install's base domain is its).
pub fn own_host(own: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    own.iter().any(|entry| match entry.strip_prefix("*.") {
        Some(domain) => host.len() > domain.len() + 1 && host.ends_with(domain) && host.as_bytes()[host.len() - domain.len() - 1] == b'.',
        None => *entry == host,
    })
}

/// The file's contents for `own`.
pub fn own_hosts_file(own: &[String]) -> String {
    let mut text = own.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own() -> Vec<String> {
        vec!["*.forkrail.com".into(), "atlas.example.net".into()]
    }

    #[test]
    fn names_and_domains() {
        let own = own();
        assert!(own_host(&own, "orbit.forkrail.com"));
        assert!(own_host(&own, "Orbit.Forkrail.com."));
        assert!(own_host(&own, "a.b.forkrail.com"));
        assert!(own_host(&own, "atlas.example.net"));
        assert!(!own_host(&own, "forkrail.com"));
        assert!(!own_host(&own, "evilforkrail.com"));
        assert!(!own_host(&own, "harbor.example.net"));
        assert!(!own_host(&own, "www.example.org"));
        assert!(!own_host(&own, ""));
    }

    #[test]
    fn file() {
        assert_eq!(own_hosts_file(&own()), "*.forkrail.com\natlas.example.net\n");
    }
}
