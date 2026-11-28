#!/usr/bin/env python3
"""Find the cheapest Lambda memory size and CPU budget for one function, within a latency target.

Lambda bills memory x time and gives CPU in proportion to memory (1,769 MB is one vCPU). With
concurrent compute one worker serves many requests, so the best size depends on the function:
how much CPU each request uses and when (before a database call, or parsing JSON at the end),
how much memory, how long it waits on I/O, and how its traffic arrives. So it's measured, then
simulated, per function:

1. Profile: run the function's requests one at a time at a reference size (1,769 MB), with the
   router's FORKRAIL_PROFILE_PCT=100. For each request the worker reports the CPU it used over
   time (`cpu_profile`: samples a few ms apart), its memory cost and how long it took. A request
   becomes a list of phases: CPU work, then waiting, then CPU work, and so on.
2. Model Lambda's CPU: below 1,769 MB the environment gets a quota of s = MB / 1769 of a core in
   every period (CFS bandwidth control). `calibrate` fits the period from requests that burn a
   known amount of CPU at several sizes. The app runs one thread, so above one vCPU it gets no
   faster.
3. Simulate: replay the traffic (recorded arrivals, or random ones at a given rate) through an
   event-by-event model of the router's scheduler: packing onto the busiest worker that has room
   for the request's learned memory and CPU share, spawning on demand, headroom spares (none by
   default, as on the router since 2026-11-06), idle retirement (by default the router's `auto`:
   a worker is kept idle only while that's cheaper than a new invocation, so the time depends on
   the memory size), cold starts and warm thaws, and the workers' CPU shared among their
   requests' CPU phases under the quota.
4. Search: every memory size and CPU budget (FORKRAIL_CPU_TARGET_PCT). Keep those whose p50 and
   p95 latency, per route and overall, stay within the tolerance (default 20%, plus --slack-ms,
   default 50 ms) of the same requests run alone at the reference size. Pick the cheapest. The
   slack is for sparse traffic: there, most requests wake a frozen environment (15-25 ms), at
   any memory size, which alone would fail a 20 ms route's 20%.

Usage (journals come from `deploy/aws/test-env.sh fetch`):
  tools/tune-memory.py calibrate CALIBRATION.jsonl
  tools/tune-memory.py tune PROFILE.jsonl --rate 1.35 [--traffic RUN.jsonl] [--period-ms P]
  tools/tune-memory.py predict PROFILE.jsonl RUN.jsonl --memory 512 --cpu-target 60
      simulates the arrivals of a measured run, to check the model against it

Python 3.10+, stdlib only.
"""

import argparse
import collections
import heapq
import itertools
import json
import math
import multiprocessing
import random
import statistics
import sys

MB_PER_VCPU = 1769
GB_SECOND = 0.0000133334  # arm64, us-east-1
PER_INVOKE = 0.20 / 1e6
SECONDS_PER_MONTH = 30 * 24 * 3600

# The router's scheduler (crates/router/src/pool.rs), mirrored. Idle time and headroom are
# settings (--idle-ms, --headroom-pct), as on the router.
MAX_AGE_MS = 780_000
REPLACE_LEAD_MS = 30_000
MIN_HEADROOM_PCT_OF_WORKER = 30
PEAK_WINDOW_MS = 10_000
MEMORY_MARGIN = 0.9
DEFAULT_ROUTE_CPU = 0.1
WORKER_IDLE_GRACE_MS = 60_000  # the worker's own safety timer, past the router's idle
RETIRE_BEFORE_WORKER_MS = 5_000


def economic_idle_ms(memory_mb):
    """FORKRAIL_IDLE_MS=auto: how long an idle worker is worth keeping, as in
    crates/common/src/routes.rs (an invocation costs about what this much idle time does)."""
    return max(5, 15_360 // max(128, memory_mb))


# ---------------------------------------------------------------------------------------------
# Reading journals


def load(path):
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


def worker_limits(events):
    return {e["worker"]: e.get("limit_mb", 0) for e in events if e["event"] == "worker connected"}


def phases_of(profile, cpu_us, busy_us):
    """A request's phases at full speed: [(cpu_ms, wait_ms), ...], from its CPU samples, or
    without samples, its CPU split between the start and the end."""
    if not profile:
        cpu, busy = (cpu_us or 0) / 1000, (busy_us or 0) / 1000
        return [(cpu / 2, max(0.0, busy - cpu)), (cpu / 2, 0.0)]
    samples = [(0, 0)] + [tuple(map(int, s.split(":"))) for s in profile.split(",")]
    phases = []
    for (t0, c0), (t1, c1) in zip(samples, samples[1:]):
        dt, dc = (t1 - t0) / 1000, max(0, c1 - c0) / 1000
        dc = min(dc, dt)  # one thread can't use more than the time that passed
        if phases and phases[-1][1] < 0.05:  # nothing waited for since: still the same CPU run
            cpu, wait = phases.pop()
            phases.append((cpu + dc, dt - dc))
        else:
            phases.append((dc, dt - dc))
    return [(round(c, 3), round(w, 3)) for c, w in phases if c > 0 or w > 0]


class Template:
    """One profiled request."""

    def __init__(self, event, overhead_ms):
        self.route = event["path"]
        self.phases = phases_of(event.get("cpu_profile"), event.get("cpu_us"), event.get("busy_us"))
        self.cpu_ms = sum(c for c, _ in self.phases)
        self.busy_ms = sum(c + w for c, w in self.phases)
        self.mem_kb = event.get("mem_kb") or 1024
        self.ref_ms = self.busy_ms + overhead_ms


class Profile:
    def __init__(self, events, ref_mb=None):
        limits = worker_limits(events)
        requests = [e for e in events if e["event"] == "request" and e.get("status") == 200 and e.get("busy_us")]
        if ref_mb is None:
            ref_mb = collections.Counter(limits.get(e.get("worker"), 0) for e in requests).most_common(1)[0][0]
        self.ref_mb = ref_mb
        requests = [e for e in requests if limits.get(e.get("worker")) == ref_mb]
        if not requests:
            sys.exit(f"no profiled requests at {ref_mb} MB")
        # Router to worker and back, and the router's own work: what the worker didn't see.
        self.overhead_ms = statistics.median(max(0.0, e["total_ms"] - e["busy_us"] / 1000) for e in requests)
        self.templates = [Template(e, self.overhead_ms) for e in requests]
        self.by_route = collections.defaultdict(list)
        for t in self.templates:
            self.by_route[t.route].append(t)
        connected = [e for e in events if e["event"] == "worker connected"]
        self.base_kb = max((e.get("rss_mb", 0) * 1024 for e in connected), default=64 * 1024)
        ended = {e["worker"]: e for e in events if e["event"] == "worker ended"}
        cold, warm, app_start = [], [], []
        for e in connected:
            report = ended.get(e["worker"], {}).get("report", {})
            if not isinstance(report, dict):
                continue
            if report.get("app_start_ms"):
                cold.append(e["spawn_ms"] - report["app_start_ms"])
                app_start.append(report["app_start_ms"])
            elif report.get("env_invocations", 1) > 1:
                warm.append(e["spawn_ms"])
        # Lambda creating the environment, apart from the app's start; and a frozen one thawing.
        self.env_create_ms = statistics.median(cold) if cold else 250.0
        self.thaw_ms = statistics.median(warm) if warm else 25.0
        self.app_start_ms = statistics.median(app_start) if app_start else 500.0

    def summary(self):
        lines = [f"profile at {self.ref_mb} MB: {len(self.templates)} requests on {len(self.by_route)} routes; "
                 f"base memory {self.base_kb / 1024:.0f} MB, router overhead {self.overhead_ms:.1f} ms, "
                 f"cold start {self.env_create_ms:.0f} ms + app start {self.app_start_ms:.0f} ms, thaw {self.thaw_ms:.0f} ms"]
        for route, ts in sorted(self.by_route.items(), key=lambda kv: -len(kv[1])):
            busy = statistics.median(t.busy_ms for t in ts)
            cpu = statistics.median(t.cpu_ms for t in ts)
            mem = max(t.mem_kb for t in ts) / 1024
            lines.append(f"  {route:40} n={len(ts):3}  busy {busy:7.1f} ms  cpu {cpu:6.1f} ms ({cpu / busy * 100:4.1f}%)  "
                         f"mem {mem:5.1f} MB  phases {len(ts[0].phases)}")
        return "\n".join(lines)


# ---------------------------------------------------------------------------------------------
# Lambda's CPU, and one request alone on it


def cores(memory_mb):
    """The CPU the app's one thread can use: the memory's share of a vCPU, at most one."""
    return min(1.0, memory_mb / MB_PER_VCPU)


def alone_ms(phases, memory_mb, period_ms, offset_ms=0.0):
    """How long the phases take with a worker to themselves: CPU at full speed until the
    period's quota runs out, then nothing until the next period."""
    s = cores(memory_mb)
    t = offset_ms
    if s >= 1:
        return sum(c + w for c, w in phases)
    quota = s * period_ms
    used = 0.0  # in the current period
    period_end = (math.floor(t / period_ms) + 1) * period_ms
    for cpu, wait in phases:
        while cpu > 1e-9:
            if t >= period_end:
                periods = math.floor((t - period_end) / period_ms) + 1
                period_end += periods * period_ms
                used = 0.0
            if used >= quota - 1e-9:
                t = period_end
                continue
            step = min(cpu, quota - used, period_end - t)
            t += step
            used += step
            cpu -= step
        t += wait
    return t - offset_ms


def mean_alone_ms(phases, memory_mb, period_ms, tries=8):
    """Averaged over where in a period the request starts."""
    return statistics.fmean(alone_ms(phases, memory_mb, period_ms, period_ms * i / tries) for i in range(tries))


def app_start_ms(profile, memory_mb, init_exponent):
    """The app's start is CPU-bound (imports), so it stretches where CPU is throttled:
    by (1 / share) ** init_exponent, which `calibrate` fits (1: fully, 0: not at all)."""
    return profile.app_start_ms * (cores(profile.ref_mb) / cores(memory_mb)) ** init_exponent


# ---------------------------------------------------------------------------------------------
# The scheduler, simulated


class Request:
    __slots__ = ("arrival", "template", "phase", "done_at", "worker", "cold", "cpu_cost", "mem_kb")

    def __init__(self, arrival, template):
        self.arrival = arrival
        self.template = template
        self.phase = 0
        self.done_at = None
        self.worker = None
        self.cold = False


class Worker:
    __slots__ = ("id", "spawned", "ready", "inflight", "reserved_kb", "reserved_cpu", "idle_since",
                 "chunks", "last", "quota_used", "period", "version", "retired", "aging", "cpu_used", "cold")

    def __init__(self, wid, spawned):
        self.id = wid
        self.spawned = spawned
        self.ready = False
        self.inflight = set()
        self.reserved_kb = 0
        self.reserved_cpu = 0.0
        self.idle_since = None
        self.chunks = {}  # request -> CPU ms still to do in its current phase
        self.last = 0.0
        self.quota_used = 0.0
        self.period = -1
        self.version = 0
        self.retired = None
        self.aging = False
        self.cpu_used = 0.0
        self.cold = False


class Sim:
    def __init__(self, profile, model, memory_mb, cpu_target, max_inflight):
        self.p, self.memory_mb, self.max_inflight = profile, memory_mb, max_inflight
        self.period = model["period_ms"]
        self.s = cores(memory_mb)
        self.quota = self.s * self.period if self.s < 1 else math.inf
        self.cpu_budget = cpu_target / 100 * self.s if cpu_target else 0.0
        self.budget_kb = memory_mb * 1024 * MEMORY_MARGIN
        self.cold_ms = profile.env_create_ms + app_start_ms(profile, memory_mb, model["init_exponent"])
        self.thaw_ms = profile.thaw_ms
        self.env_ttl_ms = model["env_ttl_ms"]
        self.idle_ms = economic_idle_ms(memory_mb) if model["idle_ms"] == "auto" else float(model["idle_ms"])
        self.headroom_pct = model["headroom_pct"]
        self.spare_max_idle_ms = self.idle_ms + WORKER_IDLE_GRACE_MS - RETIRE_BEFORE_WORKER_MS
        # What the router learns per route: memory rises at once and falls slowly, so about the
        # largest seen; the CPU share rises halfway and falls slowly, so a high quantile.
        self.route_kb, self.route_cpu = {}, {}
        for route, ts in profile.by_route.items():
            self.route_kb[route] = max(t.mem_kb for t in ts)
            shares = sorted(t.cpu_ms / mean_alone_ms(t.phases, memory_mb, self.period) for t in ts)
            self.route_cpu[route] = shares[min(len(shares) - 1, int(0.9 * len(shares)))]
        self.events = []
        self.seq = itertools.count()
        self.workers = []  # taking requests
        self.starting = 0
        self.waiting = collections.deque()
        self.frozen = []  # times environments were frozen, still thawable
        self.all_workers = []
        self.done = []
        self.peaks = collections.deque()
        self.ids = itertools.count()
        self.now = 0.0
        self.last_demand = -math.inf

    # -- events

    def at(self, t, kind, obj=None, version=None):
        heapq.heappush(self.events, (t, next(self.seq), kind, obj, version))

    def run(self, arrivals):
        for t, template in arrivals:
            self.at(t, "arrive", Request(t, template))
        # The traffic's span, for the monthly cost. Not the last event: a worker's aging and
        # max_age timers still fire up to 13 minutes after the last request.
        self.span_ms = arrivals[-1][0] if arrivals else 0
        while self.events:
            t, _, kind, obj, version = heapq.heappop(self.events)
            self.now = t
            getattr(self, "on_" + kind)(obj, version)
        self.end = max(self.span_ms, self.now)
        return self

    # -- demand and spawning, as in Pool::scale

    def demand(self):
        return sum(len(w.inflight) for w in self.workers) + len(self.waiting)

    def per_worker(self):
        demand = self.demand()
        if not self.cpu_budget or not demand:
            return self.max_inflight
        cpu = sum(w.reserved_cpu for w in self.workers) + sum(r.cpu_cost for r in self.waiting)
        typical = max(cpu / demand, 0.001)
        return max(1, min(self.max_inflight, int(self.cpu_budget / typical)))

    def short_of_headroom(self, fewer, demand, per_worker):
        if not demand or not self.headroom_pct:
            return False
        fresh = sum(1 for w in self.workers if not w.aging) - fewer
        headroom = max(math.ceil(demand * self.headroom_pct / 100), math.ceil(per_worker * MIN_HEADROOM_PCT_OF_WORKER / 100))
        return (fresh + self.starting) * per_worker < demand + headroom

    def peak(self):
        while self.peaks and self.peaks[0][0] < self.now - 2 * PEAK_WINDOW_MS:
            self.peaks.popleft()
        return max((d for _, d in self.peaks), default=0)

    def scale(self):
        demand = self.demand()
        if demand:
            self.last_demand = self.now
        if not self.peaks or self.peaks[-1][1] != demand:
            self.peaks.append((self.now, demand))
        per_worker = self.per_worker()
        for _ in range(64):
            for_waiting = math.ceil(len(self.waiting) / per_worker) > self.starting
            if not for_waiting and not self.short_of_headroom(0, demand, per_worker):
                break
            self.spawn()

    def spawn(self):
        w = Worker(next(self.ids), self.now)
        self.all_workers.append(w)
        self.starting += 1
        self.frozen = [f for f in self.frozen if self.now - f < self.env_ttl_ms]
        if self.frozen:
            self.frozen.pop()
            self.at(self.now + self.thaw_ms, "ready", w)
        else:
            w.cold = True
            self.at(self.now + self.cold_ms, "ready", w)

    def on_ready(self, w, _):
        self.starting -= 1
        w.ready = True
        w.idle_since = self.now
        w.last = self.now
        self.workers.append(w)
        self.at(self.now + MAX_AGE_MS - REPLACE_LEAD_MS, "aging", w)
        self.at(self.now + MAX_AGE_MS, "max_age", w)
        self.at(self.now + self.idle_ms, "idle_check", w)
        self.place()
        self.scale()

    def on_aging(self, w, _):
        w.aging = True
        self.scale()

    def on_max_age(self, w, _):
        if w in self.workers:
            self.workers.remove(w)
            self.maybe_end(w)
            self.scale()

    def maybe_end(self, w):
        if w not in self.workers and not w.inflight and w.retired is None:
            w.retired = self.now
            self.frozen.append(self.now)

    def on_idle_check(self, w, _):
        if w not in self.workers or w.inflight or w.idle_since is None:
            return
        idle_for = self.now - w.idle_since
        if idle_for < self.idle_ms - 1e-6:
            self.at(w.idle_since + self.idle_ms, "idle_check", w)
            return
        # As in Pool::reap: spares ride out dips while there's demand.
        peak = self.peak() if self.demand() or self.now - self.last_demand < self.idle_ms else 0
        spare_needed = idle_for < self.spare_max_idle_ms and not w.aging and self.short_of_headroom(1, peak, self.per_worker())
        if spare_needed:
            self.at(self.now + 250, "idle_check", w)
            return
        self.workers.remove(w)
        self.maybe_end(w)

    # -- admission, as in Worker::has_room

    def has_room(self, w, r):
        if len(w.inflight) >= self.max_inflight:
            return False
        if not w.inflight:
            return True
        memory = self.p.base_kb + w.reserved_kb + r.mem_kb <= self.budget_kb
        cpu = not self.cpu_budget or w.reserved_cpu + r.cpu_cost <= self.cpu_budget + 1e-9
        return memory and cpu

    def place(self):
        placed = False
        for r in list(self.waiting):
            candidates = [w for w in self.workers if self.has_room(w, r)]
            if not candidates:
                continue
            w = max(candidates, key=lambda w: (not w.aging, len(w.inflight)))
            self.waiting.remove(r)
            self.admit(w, r)
            placed = True
        return placed

    def on_arrive(self, r, _):
        route = r.template.route
        r.mem_kb = self.route_kb.get(route, 64 * 1024)
        r.cpu_cost = self.route_cpu.get(route, DEFAULT_ROUTE_CPU)
        self.waiting.append(r)
        self.place()
        self.scale()

    def admit(self, w, r):
        if not w.inflight and w.idle_since is not None:
            w.idle_since = None
        # It waited for a worker to start.
        r.cold = self.now - r.arrival > 50
        r.worker = w
        w.inflight.add(r)
        w.reserved_kb += r.mem_kb
        w.reserved_cpu += r.cpu_cost
        self.start_phase(w, r)

    # -- a worker's CPU: shared equally among the requests in a CPU phase, under the quota

    def advance(self, w):
        """Bring the worker's CPU work up to now. Rates are constant between events."""
        elapsed = self.now - w.last
        w.last = self.now
        if elapsed <= 0 or not w.chunks:
            return
        if self.quota != math.inf and w.quota_used >= self.quota - 1e-9:
            return
        share = elapsed / len(w.chunks)
        for r in w.chunks:
            w.chunks[r] -= share
        w.cpu_used += elapsed
        if self.quota != math.inf:
            w.quota_used += elapsed

    def reschedule(self, w):
        w.version += 1
        if not w.chunks:
            return
        # The epsilon keeps an event at a period's end from counting as the period before.
        period = math.floor(self.now / self.period + 1e-9)
        if period != w.period:
            w.period, w.quota_used = period, 0.0
        period_end = (period + 1) * self.period
        if self.quota != math.inf and w.quota_used >= self.quota - 1e-9:
            self.at(period_end, "cpu", w, w.version)
            return
        n = len(w.chunks)
        step = min(w.chunks.values()) * n
        if self.quota != math.inf:
            step = min(step, self.quota - w.quota_used, period_end - self.now)
        self.at(self.now + max(step, 1e-6), "cpu", w, w.version)

    def on_cpu(self, w, version):
        if version != w.version:
            return
        self.advance(w)
        finished = [r for r, left in w.chunks.items() if left <= 1e-6]
        for r in finished:
            del w.chunks[r]
            self.after_cpu(w, r)
        self.reschedule(w)

    def start_phase(self, w, r):
        cpu, _ = r.template.phases[r.phase]
        if cpu > 0:
            self.advance(w)
            w.chunks[r] = cpu
            self.reschedule(w)
        else:
            self.after_cpu(w, r)

    def after_cpu(self, w, r):
        _, wait = r.template.phases[r.phase]
        if wait > 0:
            self.at(self.now + wait, "io", r)
        else:
            self.next_phase(w, r)

    def on_io(self, r, _):
        self.next_phase(r.worker, r)

    def next_phase(self, w, r):
        r.phase += 1
        if r.phase < len(r.template.phases):
            self.start_phase(w, r)
            return
        r.done_at = self.now + self.p.overhead_ms
        self.done.append(r)
        w.inflight.discard(r)
        w.reserved_kb -= r.mem_kb
        w.reserved_cpu -= r.cpu_cost
        if not w.inflight:
            w.idle_since = self.now
            if w in self.workers:
                self.at(self.now + self.idle_ms, "idle_check", w)
            else:
                self.maybe_end(w)
        self.place()
        self.scale()

    # -- results

    def results(self, tolerance, quantiles=(0.5, 0.95), slack_ms=0.0):
        billed_ms = sum((w.retired if w.retired is not None else self.end) - w.spawned for w in self.all_workers)
        cost = billed_ms / 1000 * self.memory_mb / 1024 * GB_SECOND + len(self.all_workers) * PER_INVOKE
        span_s = max(1.0, self.span_ms / 1000)
        latencies = [(r.template.route, r.done_at - r.arrival, r.template.ref_ms) for r in self.done]
        ok, worst = True, 0.0
        groups = collections.defaultdict(list)
        for route, ms, ref in latencies:
            groups[route].append((ms, ref))
            groups["(all)"].append((ms, ref))
        ratios = {}
        for route, pairs in groups.items():
            if len(pairs) < 30 and route != "(all)":
                continue
            ms = sorted(m for m, _ in pairs)
            ref = sorted(r for _, r in pairs)
            for q in quantiles:
                i = min(len(ms) - 1, int(q * len(ms)))
                ratio = ms[i] / max(ref[i], 1e-6)
                ratios[(route, q)] = ratio
                worst = max(worst, ratio)
                ok = ok and ms[i] <= ref[i] * (1 + tolerance) + slack_ms
        all_ms = sorted(m for m, _ in groups["(all)"])
        pct = lambda q: all_ms[min(len(all_ms) - 1, int(q * len(all_ms)))] if all_ms else 0
        busy_cpu = sum(w.cpu_used for w in self.all_workers)
        return {
            "memory_mb": self.memory_mb,
            "requests": len(self.done),
            "workers": len(self.all_workers),
            "cold": sum(1 for w in self.all_workers if w.cold),
            "cold_requests": sum(1 for r in self.done if r.cold),
            "billed_s": billed_ms / 1000,
            "cost_month": cost / span_s * SECONDS_PER_MONTH,
            "cost_per_m": cost / max(1, len(self.done)) * 1e6,
            "p50": pct(0.5), "p95": pct(0.95), "p99": pct(0.99),
            "worst_ratio": worst,
            "ok": ok and all_ms != [],
            "per_worker": len(self.done) / max(1, len(self.all_workers)),
            "in_flight": sum(r.done_at - r.arrival for r in self.done) / max(1.0, billed_ms),
            "cpu_util": busy_cpu / max(1.0, billed_ms * self.s),
        }


# ---------------------------------------------------------------------------------------------
# Traffic


def poisson(profile, rate, duration_s, seed, burst=None):
    rng = random.Random(seed)
    routes = list(profile.by_route)
    arrivals, t = [], 0.0
    while t < duration_s * 1000:
        t += rng.expovariate(rate) * 1000
        route = rng.choice(routes)
        arrivals.append((t, rng.choice(profile.by_route[route])))
    if burst:
        size, every = burst
        b = every * 1000 / 2
        while b < duration_s * 1000:
            arrivals += [(b, rng.choice(profile.templates)) for _ in range(size)]
            b += every * 1000
    arrivals.sort(key=lambda a: a[0])
    return arrivals


def replayed(profile, events, seed):
    """The arrivals of a measured run, each a profiled request of the same route."""
    rng = random.Random(seed)
    requests = [e for e in events if e["event"] == "request" and e.get("path") in profile.by_route]
    if not requests:
        sys.exit("no requests in the run match a profiled route")
    first = min(e["ts"] * 1000 - e["total_ms"] for e in requests)
    arrivals = sorted((e["ts"] * 1000 - e["total_ms"] - first, rng.choice(profile.by_route[e["path"]])) for e in requests)
    return arrivals


# ---------------------------------------------------------------------------------------------
# Commands


def model_from(args):
    idle = args.idle_ms if args.idle_ms == "auto" else float(args.idle_ms)
    return {"period_ms": args.period_ms, "init_exponent": args.init_exponent, "env_ttl_ms": args.env_ttl_s * 1000,
            "idle_ms": idle, "headroom_pct": args.headroom_pct}


SHARED = {}


def share(profile, model, arrivals, tolerance, slack_ms=0.0):
    """Set once per process, so each job sends only its own configuration."""
    SHARED.update(profile=profile, model=model, arrivals=arrivals, tolerance=tolerance, slack_ms=slack_ms)


def one(job):
    memory_mb, cpu_target, max_inflight = job
    g = SHARED
    result = Sim(g["profile"], g["model"], memory_mb, cpu_target, max_inflight).run(g["arrivals"]).results(g["tolerance"], slack_ms=g["slack_ms"])
    result.update(cpu_target=cpu_target, max_inflight=max_inflight)
    return result


def memory_grid(profile, lo=None):
    peak = max(t.mem_kb for t in profile.templates)
    floor = math.ceil((profile.base_kb + peak) / 1024 / MEMORY_MARGIN / 64) * 64
    sizes = set(range(max(128, lo or floor), 1025, 64)) | set(range(1152, 3009, 128)) | {MB_PER_VCPU}
    return sorted(s for s in sizes if s >= max(128, floor))


def cmd_tune(args):
    profile = Profile(window(load(args.profile), args.profile_since, args.profile_until), args.ref_mb)
    print(profile.summary())
    model = model_from(args)
    if args.traffic:
        arrivals = replayed(profile, window(load(args.traffic), args.since, args.until), args.seed)
        print(f"traffic: {len(arrivals)} recorded arrivals over {arrivals[-1][0] / 1000:.0f} s")
    else:
        burst = tuple(args.burst) if args.burst else None
        arrivals = poisson(profile, args.rate, args.duration, args.seed, burst)
        print(f"traffic: random arrivals at {args.rate}/s for {args.duration} s" + (f", bursts of {burst[0]} every {burst[1]} s" if burst else ""))
    targets = [0] + args.cpu_targets
    jobs = [(m, c, k) for m in memory_grid(profile) for c in targets for k in args.max_inflight]
    jobs += [(m, 0, 1) for m in memory_grid(profile)]  # classic Lambda: one request per worker
    with multiprocessing.Pool(initializer=share, initargs=(profile, model, arrivals, args.tolerance, args.slack_ms)) as pool:
        results = pool.map(one, jobs, chunksize=4)
    ok = [r for r in results if r["ok"]]
    print(f"\n{len(results)} configurations simulated, {len(ok)} within {args.tolerance:.0%} + {args.slack_ms:.0f} ms of the reference latency\n")
    header = f"{'MB':>5} {'cpu%':>4} {'max':>3}  {'$/month':>8} {'$/M req':>8}  {'p50':>6} {'p95':>6} {'worst':>5}  {'workers':>7} {'cold':>4} {'req/wkr':>7} {'inflight':>8} {'cpu use':>7}"
    row = lambda r: (f"{r['memory_mb']:>5} {r['cpu_target'] or '-':>4} {r['max_inflight']:>3}  {r['cost_month']:>8.2f} {r['cost_per_m']:>8.2f}  "
                     f"{r['p50']:>6.0f} {r['p95']:>6.0f} {r['worst_ratio']:>5.2f}  {r['workers']:>7} {r['cold']:>4} "
                     f"{r['per_worker']:>7.1f} {r['in_flight']:>8.2f} {r['cpu_util']:>6.0%}")
    print("cheapest within the latency target:")
    print(header)
    for r in sorted(ok, key=lambda r: r["cost_month"])[:12]:
        print(row(r))
    print("\nfor comparison:")
    print(header)
    cur_mb, cur_cpu = args.current
    for label, pick in [(f"current: {cur_mb} MB, 10 per worker, CPU budget {cur_cpu or 'none'}",
                         lambda r: r["memory_mb"] == cur_mb and r["cpu_target"] == cur_cpu and r["max_inflight"] == 10),
                        (f"classic Lambda: {cur_mb} MB, 1 per worker", lambda r: r["memory_mb"] == cur_mb and r["max_inflight"] == 1),
                        ("classic Lambda: 1,769 MB, 1 per worker", lambda r: r["memory_mb"] == MB_PER_VCPU and r["max_inflight"] == 1)]:
        for r in results:
            if pick(r):
                print(row(r) + f"   {label}")
                break
    best_any = min(results, key=lambda r: r["cost_month"])
    print(row(best_any) + "   cheapest at any latency")
    if ok:
        best = min(ok, key=lambda r: r["cost_month"])
        print(f"\nrecommended: --memory-size {best['memory_mb']}, FORKRAIL_CPU_TARGET_PCT={best['cpu_target']}, "
              f"FORKRAIL_MAX_INFLIGHT={best['max_inflight']}: ${best['cost_month']:.2f}/month at this traffic, "
              f"p50 {best['p50']:.0f} ms, p95 {best['p95']:.0f} ms")
    if args.json:
        with open(args.json, "w") as f:
            json.dump(results, f)


def utc(text):
    import datetime
    return datetime.datetime.strptime(text, "%Y-%m-%d %H:%M:%S").replace(tzinfo=datetime.timezone.utc).timestamp()


def window(events, since, until):
    """Events of one run: requests that arrived in it, and the workers it spawned."""
    if not since:
        return events
    lo, hi = utc(since), utc(until) if until else math.inf
    spawned = {e["worker"] for e in events if e["event"] == "spawning worker" and lo <= e["ts"] <= hi}
    keep = []
    for e in events:
        if e["event"] == "request":
            arrived = e["ts"] - e.get("total_ms", 0) / 1000
            if lo <= arrived <= hi:
                keep.append(e)
        elif e.get("worker") in spawned:
            keep.append(e)
    return keep


def cmd_predict(args):
    profile = Profile(window(load(args.profile), args.profile_since, args.profile_until), args.ref_mb)
    run = window(load(args.run), args.since, args.until)
    model = model_from(args)
    arrivals = replayed(profile, run, args.seed)
    share(profile, model, arrivals, args.tolerance, args.slack_ms)
    sim = one((args.memory, args.cpu_target, args.max_inflight[0]))
    requests = [e for e in run if e["event"] == "request" and e.get("path") in profile.by_route]
    total = sorted(e["total_ms"] for e in requests)
    pct = lambda q: total[min(len(total) - 1, int(q * len(total)))]
    spawned = [e for e in run if e["event"] == "spawning worker"]
    ended = [e for e in run if e["event"] == "worker ended"]
    measured = {
        "requests": len(requests), "workers": len(spawned),
        "cold": sum(1 for e in ended if isinstance(e.get("report"), dict) and e["report"].get("app_start_ms")),
        "billed_s": sum(e.get("secs", 0) for e in ended),
        "p50": pct(0.5), "p95": pct(0.95), "p99": pct(0.99),
    }
    print(f"{'':10} {'requests':>8} {'workers':>7} {'cold':>4} {'billed s':>9} {'p50':>6} {'p95':>6} {'p99':>6}")
    for label, r in [("measured", measured), ("simulated", sim)]:
        print(f"{label:10} {r['requests']:>8} {r['workers']:>7} {r['cold']:>4} {r['billed_s']:>9.1f} {r['p50']:>6.0f} {r['p95']:>6.0f} {r['p99']:>6.0f}")


def work_phases(path, cpu_ms):
    """The phases of a test app's `/work/cpu:5,io:50,...` request, with the CPU it really used
    (the steps plus the runtime's and the framework's own work, counted at the start)."""
    steps = [step.partition(":") for step in path.removeprefix("/work/").split(",")]
    phases = [(float(v), 0.0) if k == "cpu" else (0.0, float(v)) for k, _, v in steps]
    extra = max(0.0, cpu_ms - sum(c for c, _ in phases))
    phases.insert(0, (extra, 0.0))
    merged = []
    for cpu, wait in phases:
        if merged and merged[-1][1] == 0:
            merged[-1] = (merged[-1][0] + cpu, wait)
        else:
            merged.append((cpu, wait))
    return merged


def cmd_calibrate(args):
    """Fit Lambda's CPU period from the test apps' `/work/...` requests, run alone at several sizes."""
    events = load(args.calibration)
    limits = worker_limits(events)
    samples = []
    for e in events:
        path = e.get("path", "")
        if e["event"] != "request" or not e.get("busy_us") or not path.startswith("/work/"):
            continue
        memory_mb = limits.get(e.get("worker"))
        if memory_mb:
            samples.append((memory_mb, path, work_phases(path, e["cpu_us"] / 1000), e["busy_us"] / 1000))
    if not samples:
        sys.exit("no /work/... requests with CPU measurements")
    periods = [5, 10, 12, 15, 18, 20, 22, 25, 30, 40, 50, 100]
    errors = {p: [] for p in periods}
    for memory_mb, _, phases, busy in samples:
        for p in periods:
            errors[p].append(abs(mean_alone_ms(phases, memory_mb, p) - busy) / max(busy, 1))
    period = min(periods, key=lambda p: statistics.fmean(errors[p]))
    print(f"{'MB':>5} {'share':>5}  {'path':28} {'cpu ms':>7} {'busy ms':>8} {'model':>7}   (medians, model at period {period} ms)")
    groups = collections.defaultdict(list)
    for memory_mb, path, phases, busy in samples:
        groups[(memory_mb, path)].append((phases, busy))
    for (memory_mb, path), items in sorted(groups.items(), key=lambda kv: (kv[0][0], sum(c for c, _ in kv[1][0][0]))):
        cpu = statistics.median(sum(c for c, _ in ph) for ph, _ in items)
        busy = statistics.median(b for _, b in items)
        model = statistics.median(mean_alone_ms(ph, memory_mb, period) for ph, _ in items)
        print(f"{memory_mb:>5} {cores(memory_mb):>5.2f}  {path:28} {cpu:>7.1f} {busy:>8.1f} {model:>7.1f}")
    print("\nperiod fit, mean absolute error of the busy time:")
    for p in periods:
        print(f"  {p:>4} ms: {statistics.fmean(errors[p]):6.1%}")
    print(f"\nbest: --period-ms {period}")
    starts = [(limits.get(e["worker"]), e["report"]["app_start_ms"]) for e in events
              if e["event"] == "worker ended" and isinstance(e.get("report"), dict) and e["report"].get("app_start_ms")]
    if starts:
        print("\napp start (cold) by size: the closer these are, the closer --init-exponent is to 0")
        by = collections.defaultdict(list)
        for m, ms in starts:
            by[m].append(ms)
        for m in sorted(k for k in by if k):
            print(f"  {m:>5} MB: {statistics.median(by[m]):6.0f} ms (n={len(by[m])})")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--ref-mb", type=int, help="the profile's memory size (default: the most common)")
    common.add_argument("--period-ms", type=float, default=20.0, help="Lambda's CPU quota period, from `calibrate`")
    common.add_argument("--init-exponent", type=float, default=0.0, help="how much throttling slows the app's start, from `calibrate`")
    common.add_argument("--env-ttl-s", type=float, default=300.0, help="how long Lambda keeps a frozen environment")
    common.add_argument("--tolerance", type=float, default=0.2, help="allowed latency increase at p50 and p95")
    common.add_argument("--slack-ms", type=float, default=50.0, help="allowed on top of the tolerance, in ms (thaws on sparse traffic)")
    common.add_argument("--max-inflight", type=int, nargs="+", default=[10, 64])
    common.add_argument("--idle-ms", default="auto", help="the router's FORKRAIL_IDLE_MS: auto (by memory size) or milliseconds")
    common.add_argument("--headroom-pct", type=int, default=0, help="the router's FORKRAIL_HEADROOM_PCT (0: no spares)")
    common.add_argument("--seed", type=int, default=1)
    tune = sub.add_parser("tune", parents=[common])
    tune.add_argument("profile")
    tune.add_argument("--profile-since", help="the profiling run's start, UTC")
    tune.add_argument("--profile-until", help="the profiling run's end")
    tune.add_argument("--traffic", help="a run's journal whose arrivals to replay")
    tune.add_argument("--rate", type=float, default=1.0, help="requests per second, for random arrivals")
    tune.add_argument("--duration", type=float, default=3600)
    tune.add_argument("--burst", type=float, nargs=2, metavar=("SIZE", "EVERY_S"))
    tune.add_argument("--cpu-targets", type=int, nargs="+", default=[30, 40, 50, 60, 70, 80, 100, 120, 150, 200])
    tune.add_argument("--current", type=int, nargs=2, default=[768, 150], metavar=("MB", "CPU_PCT"),
                      help="the deployment's settings now, for the comparison row")
    tune.add_argument("--json")
    tune.add_argument("--since", help="with --traffic: the run's start, UTC")
    tune.add_argument("--until", help="with --traffic: the run's end")
    predict = sub.add_parser("predict", parents=[common])
    predict.add_argument("profile")
    predict.add_argument("--profile-since")
    predict.add_argument("--profile-until")
    predict.add_argument("run")
    predict.add_argument("--memory", type=int, required=True)
    predict.add_argument("--cpu-target", type=int, default=0)
    predict.add_argument("--since", help="the run's start, UTC, as replay prints it")
    predict.add_argument("--until", help="the run's end")
    calibrate = sub.add_parser("calibrate", parents=[common])
    calibrate.add_argument("calibration")
    args = parser.parse_args()
    if getattr(args, "burst", None):
        args.burst = (int(args.burst[0]), args.burst[1])
    {"tune": cmd_tune, "predict": cmd_predict, "calibrate": cmd_calibrate}[args.command](args)


if __name__ == "__main__":
    main()
