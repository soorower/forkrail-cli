//! The spend ledger (C15): what every project costs, counted here as it's spent, at AWS's list
//! price (`forkrail_common::prices`), in the units AWS bills.
//!
//! AWS's own bill trails by hours to a day. The router is the only caller of a project's functions
//! and carries every byte its visitors get, so it can count each cost the moment it's spent:
//!
//! - **Function time.** A worker is one invocation, billed from the invoke until it returns, start
//!   included. Each open one accrues its configured memory every `TICK`, so a worker that lives 13
//!   minutes counts as it runs, not when it ends; at its end the rest, rounded up a millisecond as
//!   Lambda rounds. Each invoke is a request. Warm-ups and the image optimizer are counted the same.
//! - **Visitors' bytes.** What the kernel counted on each client connection (`wire.rs`: TLS
//!   handshakes, headers, retransmits and all), read every 2 s and when it closes, with
//!   each segment's TCP/IP headers and the acknowledgements added (`prices::on_wire`). A
//!   connection's bytes in each period go to the projects whose requests it carried then, by how
//!   much of the requests' and responses' bytes each had; bytes of no request (a handshake for an
//!   unknown host, a connection that never asked anything) to the install itself (project `""`),
//!   so they count toward the install's budget. Priced by the visitor's zone (Global Accelerator's
//!   premium differs by where they are), on the dominant direction of each hour as it bills.
//! - **Tunnels.** A worker's tunnel to the router, sampled the same way ($0.01/GB each way).
//!
//! Every check (`budgets.rs`, 10 s) the counts since the last go into ClickHouse's `usage` table
//! (`store`, waiting for its answer, so the month to date read next has them) and this router's
//! month-to-date per project. Rows ClickHouse doesn't take go to the sink, which sends them later
//! and keeps them on disk meanwhile; until it has, they're counted here (`unconfirmed`).

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use forkrail_common::prices::{self, Meter, Nanos, Zone};

/// How often open invocations accrue.
pub const TICK: Duration = Duration::from_secs(1);
/// How often connections' kernel counts are read (one system call each).
const SAMPLE_EVERY: Duration = Duration::from_secs(2);

/// Who a cost is for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Owner {
    /// Empty for the install itself.
    pub project: Arc<str>,
    pub deployment: Arc<str>,
    /// Where it was spent: the function's region for function time, the router's for bytes.
    pub region: Arc<str>,
}

impl Owner {
    pub fn new(project: &str, deployment: &str, region: &str) -> Self {
        Self { project: project.into(), deployment: deployment.into(), region: region.into() }
    }

    fn install(region: &str) -> Self {
        Self::new("", "", region)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Key {
    owner: Owner,
    meter: Meter,
    zone: Option<Zone>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub quantity: u64,
    pub cost: Nanos,
}

/// One flushed count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub project: Arc<str>,
    pub deployment: Arc<str>,
    pub region: Arc<str>,
    pub meter: Meter,
    pub zone: Option<Zone>,
    pub quantity: u64,
    pub cost: Nanos,
}

struct Open {
    owner: Owner,
    memory_mb: u64,
    meter: Meter,
    last: Instant,
}

/// The month's dominant-direction state of one project's traffic in one zone, this hour.
#[derive(Default)]
struct Hour {
    hour: u64,
    in_wire: u64,
    out_wire: u64,
    /// Premium charged so far this hour for what came in beyond what went out.
    extra: Nanos,
}

pub struct Ledger {
    /// The router's region: where bytes to visitors and tunnels are priced.
    region: String,
    counts: Mutex<HashMap<Key, Tally>>,
    open: Mutex<HashMap<u64, Open>>,
    next: AtomicU64,
    conns: Mutex<Vec<Weak<Conn>>>,
    hours: Mutex<HashMap<(Owner, Zone), Hour>>,
    /// This router's own month to date, per project (the install's under `""`), and the month.
    month: Mutex<(u32, HashMap<Arc<str>, Nanos>)>,
    /// Spend stored with the sink to send later, not in ClickHouse yet, per project.
    unconfirmed: Mutex<HashMap<Arc<str>, Nanos>>,
}

static LEDGER: OnceLock<Arc<Ledger>> = OnceLock::new();

/// The router's ledger, for its region: set once at start (`init`); tests get one without.
pub fn ledger() -> &'static Arc<Ledger> {
    LEDGER.get_or_init(|| Arc::new(Ledger::new(String::new())))
}

/// Make the router's ledger, for the router's region, before anything is counted.
pub fn init(region: &str) -> &'static Arc<Ledger> {
    LEDGER.get_or_init(|| Arc::new(Ledger::new(region.to_owned())))
}

impl Ledger {
    pub fn new(region: String) -> Self {
        Self {
            region,
            counts: Mutex::default(),
            open: Mutex::default(),
            next: AtomicU64::new(1),
            conns: Mutex::default(),
            hours: Mutex::default(),
            month: Mutex::new((month_of(unix_now()), HashMap::new())),
            unconfirmed: Mutex::default(),
        }
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    /// Add `quantity` of `meter` for `owner`, priced as the table prices it.
    pub fn add(&self, owner: &Owner, meter: Meter, zone: Option<Zone>, quantity: u64) {
        if quantity == 0 {
            return;
        }
        let cost = prices::cost(meter, &owner.region, zone, quantity);
        self.add_priced(owner, meter, zone, quantity, cost);
    }

    fn add_priced(&self, owner: &Owner, meter: Meter, zone: Option<Zone>, quantity: u64, cost: Nanos) {
        let mut counts = self.counts.lock().unwrap();
        let tally = counts.entry(Key { owner: owner.clone(), meter, zone }).or_default();
        tally.quantity += quantity;
        tally.cost += cost;
    }

    /// An invocation of a function with `memory_mb` has begun: one request now, and its time from
    /// now until the guard is dropped. `x86` for the builder's architecture.
    pub fn invocation(self: &Arc<Self>, owner: Owner, memory_mb: u32, x86: bool) -> Invocation {
        self.add(&owner, Meter::FnRequests, None, 1);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let meter = if x86 { Meter::FnX86MbMs } else { Meter::FnMbMs };
        self.open.lock().unwrap().insert(id, Open { owner, memory_mb: u64::from(memory_mb.max(1)), meter, last: Instant::now() });
        Invocation { ledger: Arc::downgrade(self), id }
    }

    /// One S3 request this router made for the install itself (its routing table, certificates,
    /// manifests and files): in the router's region, where its buckets are.
    pub fn s3(&self, meter: Meter) {
        self.add(&Owner::install(&self.region), meter, None, 1);
    }

    /// Accrue every open invocation's time to now.
    pub fn accrue(&self) {
        let now = Instant::now();
        let mut due = Vec::new();
        for open in self.open.lock().unwrap().values_mut() {
            let ms = now.duration_since(open.last).as_millis() as u64;
            if ms > 0 {
                open.last += Duration::from_millis(ms);
                due.push((open.owner.clone(), open.meter, ms * open.memory_mb));
            }
        }
        for (owner, meter, mb_ms) in due {
            self.add(&owner, meter, None, mb_ms);
        }
    }

    fn end(&self, id: u64) {
        let Some(open) = self.open.lock().unwrap().remove(&id) else { return };
        // Lambda bills to the millisecond, rounded up.
        let ms = open.last.elapsed().as_millis() as u64 + 1;
        self.add(&open.owner, open.meter, None, ms * open.memory_mb);
    }

    /// Invocations open now.
    pub fn open_invocations(&self) -> usize {
        self.open.lock().unwrap().len()
    }

    /// A client connection: `socket` a second handle on it (`wire::hold`), `zone` where the
    /// visitor comes in (None if unknown: priced as the dearest).
    pub fn client(&self, socket: Option<OwnedFd>, zone: Option<Zone>) -> Arc<Conn> {
        self.track(Conn::new(socket, Side::Client { zone }, Owner::install(&self.region)))
    }

    /// A worker's tunnel, all of it `owner`'s (whose region is the function's: a tunnel from
    /// another region is priced as traffic between regions).
    pub fn tunnel(&self, socket: Option<OwnedFd>, owner: Owner) -> Arc<Conn> {
        let side = Side::Tunnel { meter: self.tunnel_meter(&owner) };
        self.track(Conn::new(socket, side, owner))
    }

    fn tunnel_meter(&self, owner: &Owner) -> Meter {
        match self.region.is_empty() || owner.region.is_empty() || *owner.region == *self.region {
            true => Meter::TunnelBytes,
            false => Meter::InterRegionBytes,
        }
    }

    /// `sent` and `received` bytes of TCP payload between this router and `owner`'s worker on a
    /// connection that has ended (an egress connection: `tunnel.rs`).
    pub fn tunnel_bytes(&self, owner: &Owner, sent: u64, received: u64) {
        let wire = prices::on_wire(sent) + prices::acks_for(received) + prices::on_wire(received) + prices::acks_for(sent);
        self.add(owner, self.tunnel_meter(owner), None, wire);
    }

    fn track(&self, conn: Conn) -> Arc<Conn> {
        let conn = Arc::new(conn);
        let mut conns = self.conns.lock().unwrap();
        if conns.len() >= 1024 && conns.len().is_power_of_two() {
            conns.retain(|c| c.strong_count() > 0);
        }
        conns.push(Arc::downgrade(&conn));
        conn
    }

    /// Read every live connection's counts.
    pub fn sample(&self) {
        let conns: Vec<Arc<Conn>> = {
            let mut conns = self.conns.lock().unwrap();
            conns.retain(|c| c.strong_count() > 0);
            conns.iter().filter_map(Weak::upgrade).collect()
        };
        for conn in conns {
            conn.sample(self);
        }
    }

    /// Bytes a client connection carried in a period: `out_wire` to the visitor, `in_wire` from
    /// them, for `owner`. Global Accelerator's premium goes on whichever direction carried more
    /// in the hour, so egress pays it always and ingress pays only what it adds beyond.
    fn visitor_bytes(&self, owner: &Owner, zone: Option<Zone>, out_wire: u64, in_wire: u64) {
        self.add(owner, Meter::EgressBytes, zone, out_wire);
        let from = Zone::of_region(&owner.region);
        let to = zone.unwrap_or_else(|| prices::dearest_zone(from));
        let hour = unix_now() / 3600;
        let extra = {
            let mut hours = self.hours.lock().unwrap();
            if hours.len() > 100_000 {
                hours.retain(|_, h| h.hour == hour);
            }
            let h = hours.entry((owner.clone(), to)).or_default();
            if h.hour != hour {
                *h = Hour { hour, ..Hour::default() };
            }
            h.in_wire += in_wire;
            h.out_wire += out_wire;
            let due = prices::cost(Meter::IngressBytes, &owner.region, Some(to), h.in_wire.saturating_sub(h.out_wire));
            let extra = due - h.extra;
            h.extra = due;
            extra
        };
        if in_wire > 0 || extra != 0 {
            self.add_priced(owner, Meter::IngressBytes, zone, in_wire, extra);
        }
    }

    /// Take what's been counted since the last call, and add it to the month's totals.
    pub fn flush(&self) -> Vec<Row> {
        let counts = std::mem::take(&mut *self.counts.lock().unwrap());
        let mut rows: Vec<Row> = counts
            .into_iter()
            .filter(|(_, t)| t.quantity > 0 || t.cost != 0)
            .map(|(k, t)| Row { project: k.owner.project, deployment: k.owner.deployment, region: k.owner.region, meter: k.meter, zone: k.zone, quantity: t.quantity, cost: t.cost })
            .collect();
        rows.sort_by(|a, b| (&a.project, &a.deployment, a.meter.name()).cmp(&(&b.project, &b.deployment, b.meter.name())));
        let now = month_of(unix_now());
        let mut month = self.month.lock().unwrap();
        if month.0 != now {
            *month = (now, HashMap::new());
        }
        for row in &rows {
            *month.1.entry(row.project.clone()).or_default() += row.cost;
        }
        rows
    }

    /// What's been counted and not flushed yet, per project.
    pub fn pending(&self) -> HashMap<Arc<str>, Nanos> {
        let mut by: HashMap<Arc<str>, Nanos> = HashMap::new();
        for (k, t) in self.counts.lock().unwrap().iter() {
            *by.entry(k.owner.project.clone()).or_default() += t.cost;
        }
        by
    }

    /// This router's month to date for `project` (`""`: the install's own), as of the last flush.
    #[cfg(test)]
    pub fn month_to_date(&self, project: &str) -> Nanos {
        self.month.lock().unwrap().1.get(project).copied().unwrap_or(0)
    }

    /// Every project's month to date on this router.
    pub fn months(&self) -> HashMap<Arc<str>, Nanos> {
        self.month.lock().unwrap().1.clone()
    }

    /// Accrue and sample on their schedules (the checks store what's counted: `store`).
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sampled = Instant::now();
        loop {
            tick.tick().await;
            self.accrue();
            if sampled.elapsed() >= SAMPLE_EVERY {
                sampled = Instant::now();
                self.sample();
            }
        }
    }

    /// Take what's counted and put it in ClickHouse now, waiting for the answer; what it doesn't
    /// take goes to the sink to send later, and is counted as `unconfirmed` until it has.
    pub async fn store(&self) {
        self.accrue();
        let rows = self.flush();
        if rows.is_empty() {
            return;
        }
        let Some(sink) = crate::sink::get() else {
            self.send(rows);
            return;
        };
        let time = unix_now();
        let mut body = Vec::with_capacity(rows.len() * 200);
        for row in &rows {
            body.extend_from_slice(&sink.usage_line(time, row));
        }
        match crate::sink::insert(crate::sink::Table::Usage, bytes::Bytes::from(body.clone())).await {
            Some(Ok(())) => {}
            failed => {
                tracing::debug!(error = ?failed.and_then(Result::err), rows = rows.len(), "spend rows left with the sink to send later");
                sink.usage_backlog(&body);
                let mut unconfirmed = self.unconfirmed.lock().unwrap();
                for row in &rows {
                    *unconfirmed.entry(row.project.clone()).or_default() += row.cost;
                }
            }
        }
    }

    /// Spend stored with the sink and not in ClickHouse yet, per project.
    pub fn unconfirmed(&self) -> HashMap<Arc<str>, Nanos> {
        let mut unconfirmed = self.unconfirmed.lock().unwrap();
        if crate::sink::get().is_none_or(|sink| sink.usage_drained()) {
            unconfirmed.clear();
        }
        unconfirmed.clone()
    }

    /// At shutdown, before the sink's last send: everything counted so far, open invocations to
    /// now and every connection's last bytes included, so a restart loses nothing.
    pub fn finish(&self) {
        self.accrue();
        self.sample();
        let rows = self.flush();
        tracing::info!(rows = rows.len(), open = self.open_invocations(), "spend counted to the end");
        self.send(rows);
    }

    fn send(&self, rows: Vec<Row>) {
        if let Some(sink) = crate::sink::get() {
            let time = unix_now();
            for row in &rows {
                sink.usage(time, row);
            }
        } else {
            // No ClickHouse (a local run): the rows go to the router's own log.
            for row in &rows {
                tracing::debug!(
                    project = &*row.project,
                    deployment = &*row.deployment,
                    region = &*row.region,
                    meter = row.meter.name(),
                    zone = row.zone.map(|z| z.name()),
                    quantity = row.quantity,
                    cost_n = row.cost,
                    "spend"
                );
            }
        }
    }
}

/// An open invocation's time, accrued until this is dropped.
pub struct Invocation {
    ledger: Weak<Ledger>,
    id: u64,
}

impl Drop for Invocation {
    fn drop(&mut self) {
        if let Some(ledger) = self.ledger.upgrade() {
            ledger.end(self.id);
        }
    }
}

enum Side {
    Client { zone: Option<Zone> },
    Tunnel { meter: Meter },
}

struct ConnState {
    socket: Option<OwnedFd>,
    /// The kernel's totals at the last read.
    read: crate::wire::Bytes,
    /// What each owner's requests and responses carried since the last read (HTTP-level bytes,
    /// only to share the connection's out between them).
    served: Vec<(Owner, u64)>,
    /// Whom the connection's bytes go to when no request carried any since the last read.
    last: Owner,
}

/// A connection whose bytes are on the ledger.
pub struct Conn {
    side: Side,
    state: Mutex<ConnState>,
}

impl Conn {
    fn new(socket: Option<OwnedFd>, side: Side, owner: Owner) -> Self {
        Self { side, state: Mutex::new(ConnState { socket, read: crate::wire::Bytes::default(), served: Vec::new(), last: owner }) }
    }

    /// A request on this connection, `owner`'s, carried `bytes` (request and response, as HTTP
    /// sees them): what the connection's bytes are shared by.
    pub fn served(&self, owner: &Owner, bytes: u64) {
        let mut state = self.state.lock().unwrap();
        state.last = owner.clone();
        match state.served.iter_mut().find(|(o, _)| o == owner) {
            Some((_, b)) => *b += bytes.max(1),
            None => state.served.push((owner.clone(), bytes.max(1))),
        }
    }

    fn sample(&self, ledger: &Ledger) {
        let (sent, received) = {
            let mut state = self.state.lock().unwrap();
            let Some(socket) = &state.socket else { return };
            let Some(now) = crate::wire::bytes(socket) else { return };
            let delta = (now.sent.saturating_sub(state.read.sent), now.received.saturating_sub(state.read.received));
            // The highest seen: what was queued and then dropped (a reset) is counted once, not
            // again as it's sent on an older kernel's counts.
            state.read = crate::wire::Bytes { sent: state.read.sent.max(now.sent), received: state.read.received.max(now.received) };
            delta
        };
        self.charge(ledger, sent, received);
    }

    /// `sent` and `received` more bytes of TCP payload, shared by what the period's requests carried.
    fn charge(&self, ledger: &Ledger, sent: u64, received: u64) {
        let mut state = self.state.lock().unwrap();
        let served = std::mem::take(&mut state.served);
        if sent == 0 && received == 0 {
            return;
        }
        // On the wire: each direction's segments with their headers, and the acknowledgements of
        // the other's.
        let out_wire = prices::on_wire(sent) + prices::acks_for(received);
        let in_wire = prices::on_wire(received) + prices::acks_for(sent);
        let shares: Vec<(Owner, u64)> = if served.is_empty() { vec![(state.last.clone(), 1)] } else { served };
        drop(state);
        let total: u64 = shares.iter().map(|(_, w)| w).sum();
        let (mut out_left, mut in_left) = (out_wire, in_wire);
        for (i, (owner, weight)) in shares.iter().enumerate() {
            // The last share takes what rounding left, so nothing is lost or counted twice.
            let (out, inn) = match i + 1 == shares.len() {
                true => (out_left, in_left),
                false => (
                    (u128::from(out_wire) * u128::from(*weight) / u128::from(total)) as u64,
                    (u128::from(in_wire) * u128::from(*weight) / u128::from(total)) as u64,
                ),
            };
            out_left -= out;
            in_left -= inn;
            match &self.side {
                Side::Client { zone } => ledger.visitor_bytes(owner, *zone, out, inn),
                Side::Tunnel { meter } => ledger.add(owner, *meter, None, out + inn),
            }
        }
    }

    /// The connection ended: its last counts, and its handle on the socket let go (the
    /// connection isn't fully closed while it's held).
    pub fn close(&self, ledger: &Ledger) {
        self.sample(ledger);
        self.state.lock().unwrap().socket = None;
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `YYYYMM` of a Unix time, in UTC, as AWS's billing months go.
pub use forkrail_common::prices::month_of;

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(p: &str) -> Owner {
        Owner::new(p, &format!("{p}-1"), "us-east-1")
    }

    fn by_meter(rows: &[Row], project: &str, meter: Meter) -> Tally {
        rows.iter().filter(|r| &*r.project == project && r.meter == meter).fold(Tally::default(), |t, r| Tally { quantity: t.quantity + r.quantity, cost: t.cost + r.cost })
    }

    #[test]
    fn months() {
        assert_eq!(month_of(0), 197001);
        assert_eq!(month_of(1_795_132_800), 202611); // 2026-11-19
        assert_eq!(month_of(1_798_761_599), 202612); // 2026-12-31 23:59:59
        assert_eq!(month_of(1_798_761_600), 202701);
        assert_eq!(month_of(1_772_323_200), 202603); // 2026-03-01, after a short February
    }

    #[test]
    fn an_invocation_costs_its_time_and_a_request() {
        let ledger = Arc::new(Ledger::new("us-east-1".into()));
        let open = ledger.invocation(owner("a"), 1024, false);
        std::thread::sleep(Duration::from_millis(30));
        ledger.accrue();
        assert_eq!(ledger.open_invocations(), 1);
        drop(open);
        assert_eq!(ledger.open_invocations(), 0);
        let rows = ledger.flush();
        let time = by_meter(&rows, "a", Meter::FnMbMs);
        let ms = time.quantity / 1024;
        assert!((31..200).contains(&ms), "{ms} ms");
        assert_eq!(by_meter(&rows, "a", Meter::FnRequests).quantity, 1);
        assert_eq!(ledger.month_to_date("a"), rows.iter().map(|r| r.cost).sum::<Nanos>());
        // Counted once: a second flush has nothing new.
        assert!(ledger.flush().is_empty());
    }

    #[test]
    fn a_long_invocation_counts_while_it_runs() {
        let ledger = Arc::new(Ledger::new("us-east-1".into()));
        let _open = ledger.invocation(owner("a"), 2048, false);
        std::thread::sleep(Duration::from_millis(20));
        ledger.accrue();
        let first = by_meter(&ledger.flush(), "a", Meter::FnMbMs).quantity;
        assert!(first >= 20 * 2048, "{first}");
        std::thread::sleep(Duration::from_millis(20));
        ledger.accrue();
        let second = by_meter(&ledger.flush(), "a", Meter::FnMbMs).quantity;
        assert!(second >= 20 * 2048, "{second}");
    }

    #[test]
    fn a_connection_is_shared_by_what_its_requests_carried() {
        let ledger = Ledger::new("us-east-1".into());
        let conn = Conn::new(None, Side::Client { zone: Some(Zone::NA) }, Owner::install("us-east-1"));
        conn.served(&owner("a"), 300);
        conn.served(&owner("b"), 100);
        // What a read of the kernel's counts finds: 4,000 bytes out, 400 in.
        conn.charge(&ledger, 4000, 400);
        let rows = ledger.flush();
        let (a, b) = (by_meter(&rows, "a", Meter::EgressBytes).quantity, by_meter(&rows, "b", Meter::EgressBytes).quantity);
        let total = prices::on_wire(4000) + prices::acks_for(400);
        assert_eq!(a + b, total, "nothing lost or doubled");
        assert_eq!(a, total * 3 / 4);
        // A period with no request goes to whoever had the connection last.
        conn.charge(&ledger, 100, 0);
        let rows = ledger.flush();
        assert_eq!(by_meter(&rows, "b", Meter::EgressBytes).quantity, prices::on_wire(100));
    }

    #[test]
    fn bytes_of_no_request_are_the_installs() {
        let ledger = Ledger::new("us-east-1".into());
        let conn = Conn::new(None, Side::Client { zone: None }, Owner::install("us-east-1"));
        conn.charge(&ledger, 4200, 300);
        let rows = ledger.flush();
        let install = by_meter(&rows, "", Meter::EgressBytes);
        assert_eq!(install.quantity, prices::on_wire(4200) + prices::acks_for(300));
        // An unknown visitor is priced as the dearest zone's.
        assert_eq!(install.cost, prices::cost(Meter::EgressBytes, "us-east-1", Some(Zone::AU), install.quantity));
    }

    #[test]
    fn ingress_pays_only_what_it_adds_over_egress_each_hour() {
        let ledger = Ledger::new("us-east-1".into());
        let a = owner("a");
        // More out than in: ingress adds nothing.
        ledger.visitor_bytes(&a, Some(Zone::EU), 10_000, 4_000);
        assert_eq!(by_meter(&ledger.flush(), "a", Meter::IngressBytes).cost, 0);
        // Then 10,000 more in: 4,000 past what went out, which pays the premium.
        ledger.visitor_bytes(&a, Some(Zone::EU), 0, 10_000);
        let t = by_meter(&ledger.flush(), "a", Meter::IngressBytes);
        assert_eq!(t.quantity, 10_000);
        assert_eq!(t.cost, prices::cost(Meter::IngressBytes, "us-east-1", Some(Zone::EU), 4_000));
    }
}
