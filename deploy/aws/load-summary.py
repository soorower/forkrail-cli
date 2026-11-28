"""Summarize the router's journal since a time: python3 load-summary.py "2026-11-03 09:40:00".

Runs on the router instance after a load test. Counts scheduler events and
access-log outcomes, and prints percentiles of the access-log timings. With more than one
pool, it also prints requests, workers and the peak of live workers per deployment.
"""
import collections
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import forkrail_log  # noqa: E402 - beside this file on the instance


EVENTS = ["spawning worker", "worker connected", "retiring worker", "spawn failed", "worker died",
          "retrying alone", "upstream failed", "worker failed", "route memory cost", "shutting down", "drained",
          "worker cap reached", "pool created", "pool closed", "routes loaded"]

fields = re.compile(r'(\w+)=("[^"]*"|\S+)')

def pool_of(worker):
    """Worker ids are <deployment>.<hex>; the default pool's have no prefix."""
    return worker.split(".")[0] if "." in worker else "default"


events, requests = collections.Counter(), []
# Invocations open per pool, from spawn to end, and the most at once.
live, peak = collections.Counter(), collections.Counter()
spawns = []
for line in forkrail_log.lines(sys.argv[1]):
    spawns += [int(v) for v in re.findall(r"spawn_ms=(\d+)", line)]
    events.update(e for e in EVENTS if e in line)
    kv = {k: v.strip('"') for k, v in fields.findall(line)}
    if "spawn failed" in line:
        events["spawn failed: " + kv.get("reason", "?")] += 1
    if "proxy: request " in line:
        requests.append(kv)
    if "spawning worker" in line and "worker" in kv:
        pool = pool_of(kv["worker"])
        live[pool] += 1
        peak[pool] = max(peak[pool], live[pool])
        peak["(router)"] = max(peak["(router)"], sum(v for k, v in live.items() if k != "(router)"))
    if ("worker ended" in line or "worker failed" in line) and "worker" in kv:
        live[pool_of(kv["worker"])] -= 1

print("events:", dict(events))
outcomes = collections.Counter((r.get("status"), r.get("outcome"), r.get("attempts")) for r in requests)
print(f"requests: {len(requests)}, by (status, outcome, attempts): {dict(outcomes)}")
for name in ["queue_ms", "ttfb_ms", "total_ms", "worker_ms"]:
    values = sorted(int(r[name]) for r in requests if r.get(name, "").isdigit())
    if values:
        at = lambda q: values[min(len(values) - 1, int(q * len(values)))]
        print(f"{name}: p50={at(.5)} p99={at(.99)} max={values[-1]}")
print("workers used:", len({r["worker"] for r in requests if r.get("worker")}))
spawns.sort()
if spawns:
    print(f"spawn_ms: min={spawns[0]} p50={spawns[len(spawns) // 2]} max={spawns[-1]} n={len(spawns)}")
by_pool = collections.defaultdict(list)
for r in requests:
    by_pool[r.get("deployment", "default")].append(r)
if len(by_pool) > 1 or "default" not in by_pool:
    for pool, rs in sorted(by_pool.items()):
        statuses = dict(collections.Counter(r.get("status") for r in rs))
        workers = {r["worker"] for r in rs if r.get("worker")}
        totals = sorted(int(r["total_ms"]) for r in rs if r.get("total_ms", "").isdigit())
        p = lambda q: totals[min(len(totals) - 1, int(q * len(totals)))] if totals else None
        print(f"pool {pool}: {len(rs)} requests {statuses}, {len(workers)} workers, peak live {peak.get(pool, 0)}, "
              f"total_ms p50={p(.5)} p99={p(.99)}")
    print("peak live workers, router-wide:", peak.get("(router)", 0))
