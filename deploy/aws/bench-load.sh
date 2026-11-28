#!/usr/bin/env bash
# Runs on the bench router (deploy/aws/bench.sh load): oha against 127.0.0.1:8080 for a while,
# then one JSON line saying what was served and what it cost: the router's and Vector's CPU from
# their cgroups (Vector's until it has caught up with the log, since it may lag a burst), oha's
# own, and what the router wrote to its log. oha shares the two vCPUs, so "flat out" understates
# the router alone; the CPU per request is the number that carries over.
#
#   bench-load.sh NAME SECONDS CONNECTIONS HOST PATH RATE     RATE 0: flat out
set -euo pipefail
exec python3 - "$@" <<'PY'
import glob, json, os, resource, subprocess, sys, time

name, seconds, connections, host, path, rate = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4], sys.argv[5], int(sys.argv[6])
CG = "/sys/fs/cgroup/system.slice/"
ROUTER = CG + "system-forkrail\\x2drouter.slice/cpu.stat"
VECTOR = CG + "forkrail-vector.service/cpu.stat"


def cpu(path):
    try:
        for line in open(path):
            if line.startswith("usage_usec"):
                return int(line.split()[1]) / 1e6
    except OSError:
        pass
    return 0.0


def box():
    f = open("/proc/stat").readline().split()[1:]
    v = [int(x) for x in f]
    return sum(v) - v[3] - v[4], sum(v)  # busy, all (jiffies)


def log_size():
    files = glob.glob("/var/log/forkrail/router.*.log")
    return sum(os.path.getsize(f) for f in files)


def log_lines():
    n = 0
    for f in glob.glob("/var/log/forkrail/router.*.log"):
        n += int(subprocess.run(["wc", "-l", f], capture_output=True, text=True).stdout.split()[0])
    return n


def oha(secs, conns, rate):
    cmd = ["oha", "--no-tui", "--output-format", "json", "-z", f"{secs}s", "-c", str(conns), "-H", f"Host: {host}"]
    if rate:
        cmd += ["-q", str(rate)]
    out = subprocess.run(cmd + [f"http://127.0.0.1:8080{path}"], capture_output=True, text=True)
    return json.loads(out.stdout)


# Workers up and routes learned before anything is counted.
oha(6, min(connections, 20), min(rate, 200) if rate else 200)
time.sleep(3)

r0, v0, (b0, a0), s0, l0 = cpu(ROUTER), cpu(VECTOR), box(), log_size(), log_lines()
u0 = resource.getrusage(resource.RUSAGE_CHILDREN)
t0 = time.time()
got = oha(seconds, connections, rate)
t1 = time.time()
u1 = resource.getrusage(resource.RUSAGE_CHILDREN)
r1, v1, (b1, a1) = cpu(ROUTER), cpu(VECTOR), box()

# Vector until it is idle again: two seconds in a row under 2% of a core.
quiet, last, drain_started = 0, v1, time.time()
while quiet < 2 and time.time() - drain_started < 180:
    time.sleep(1)
    now = cpu(VECTOR)
    quiet = quiet + 1 if now - last < 0.02 else 0
    last = now
drain = time.time() - drain_started - 2
r2, v2, s2, l2 = cpu(ROUTER), cpu(VECTOR), log_size(), log_lines()

summary = got["summary"]
codes = got.get("statusCodeDistribution", {})
total = sum(codes.values())
ok = sum(n for code, n in codes.items() if code.startswith("2") or code.startswith("3"))
lat = got.get("latencyPercentiles", {})
ms = lambda key: round(1000 * lat[key], 1) if lat.get(key) is not None else None
per = lambda cpu_s: round(1e6 * cpu_s / total) if total else None
print(json.dumps({
    "test": name, "host": host, "path": path, "connections": connections, "rate_asked": rate or "flat out",
    "seconds": round(t1 - t0, 1), "requests": total, "ok": ok, "not_ok": total - ok,
    "errors": got.get("errorDistribution", {}),
    "rps": round(summary["requestsPerSec"]), "p50_ms": ms("p50"), "p99_ms": ms("p99"),
    "box_busy_pct": round(100 * (b1 - b0) / max(a1 - a0, 1)),
    "router_cpu_s": round(r2 - r0, 2), "router_us_per_request": per(r2 - r0),
    "router_cores_during": round((r1 - r0) / (t1 - t0), 2),
    "vector_cpu_s": round(v2 - v0, 2), "vector_us_per_request": per(v2 - v0),
    "vector_cores_during": round((v1 - v0) / (t1 - t0), 2), "vector_catch_up_s": round(max(drain, 0), 1),
    "oha_cpu_s": round(u1.ru_utime + u1.ru_stime - u0.ru_utime - u0.ru_stime, 2),
    "log_lines_per_request": round((l2 - l0) / total, 1) if total else None,
    "log_bytes_per_request": round((s2 - s0) / total) if total else None,
}))
PY
