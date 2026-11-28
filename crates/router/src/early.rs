//! Starting a worker at the visitor's TLS handshake.
//!
//! A visitor's first request on a connection comes one round trip after its ClientHello (TLS 1.3;
//! two with TLS 1.2): tens of milliseconds nearby, up to half a second far away. The ClientHello
//! names the host (SNI), which the router reads before the handshake (`ja4::read_hello`). When
//! that host's deployment would need a cold start for the request, the router starts the worker
//! then (`Pool::start_early`), and the handshake runs alongside the start instead of before it.
//!
//! Only where it's needed and likely to be used:
//! - a host the routing table has, whose deployment has a function (one of static files alone
//!   answers without a worker), and not a domain that only redirects;
//! - no worker with a free slot, no environment believed frozen (the pool's own count of those its
//!   invocations left, and those keep-warm found ready lately), no start already coming for it,
//!   and at most one of these in flight per deployment;
//! - not for an address every request of which is answered before the deployment is asked: the
//!   system shield refused it before this (`Shield::admit`), the firewall holds it off or blocks
//!   it by address (`Firewalls::stops_address`), or the project is in attack mode and the address
//!   isn't exempt (its requests get the challenge first).
//!
//! The worker is kept for the request while the connection has not had its first answer, at most
//! `HOLD`. A start nobody used then retires by the usual idle rule, and its `invocation` line says
//! `early="unused"`; its environment stays frozen for the next request, which thaws it.
//!
//! Off unless `FORKRAIL_HANDSHAKE_START=1`: a start nobody uses is a cold start (by the rule
//! above), billed for its init and its hold, so the default is the author's to set from what it
//! saves (measured in docs/data/n4).

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub use crate::pool::Expected;
use crate::proxy::Proxy;

/// How long a worker started at a handshake is kept for the connection's first request: more
/// than a far visitor's round trip and its browser's first request after the handshake.
pub const HOLD: Duration = Duration::from_secs(2);

static ON: AtomicBool = AtomicBool::new(false);

pub fn set(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Start a worker for the request a ClientHello (`hello`, the bytes as read) from `ip` is about
/// to send, if it needs one (see above). The guard keeps the worker for that request: hold it
/// until the connection's first answer.
pub fn start(proxy: &Proxy, hello: &[u8], ip: IpAddr) -> Option<Expected> {
    if !on() {
        return None;
    }
    let host = crate::ja4::server_name(hello)?;
    if host.parse::<IpAddr>().is_ok() {
        return None;
    }
    let (pool, project, kept) = proxy.registry.at_handshake(&host)?;
    if proxy.firewalls.stops_address(&project, &host, ip) || (proxy.shield.attack_mode(&project) && !proxy.shield.exempt(ip, &host, &project)) {
        return None;
    }
    pool.start_early(kept, HOLD)
}
