"""Usage, latency, errors and estimated AWS cost per project, from the router's journal:

    python3 report.py DEPLOYMENTS_B64 SINCE [UNTIL]

DEPLOYMENTS_B64 is base64 JSON, deployment id -> {project, memory_mb}, from state/routes.json
(test-env.sh report passes it). SINCE and UNTIL take journalctl's syntax. Per project:
- requests by listener and status, p50/p95/p99 of total time, retried requests and 5xx
- workers spawned, cold starts (an environment's first invocation), and billed worker time
- estimated cost: Lambda GB-seconds and requests, internet egress for the response bytes, and the
  Lambda <-> router hop for request and response bytes
For projects with requests on the public HTTPS listener, also:
- by hour (UTC): requests, 4xx, 5xx, p50/p95/p99, the slowest, peak requests in flight, peak
  workers, cold starts
- the busiest paths with their latency and errors
- the latest failures (5xx, or a response that didn't finish) and the slowest requests, with
  query, client and user agent, so a user's report can be matched to its request id
- router warnings by kind, and how many lines the app printed
App output itself isn't printed here, since apps may print secrets: see `test-env.sh logs`.

The cost figures are estimates from the router's view: billed time is measured from invoke to
return, Lambda rounds up to the millisecond, and protocol overhead isn't counted. Cross-check them
with Cost Explorer. Runs on the router instance (stdlib only).
"""

import base64
import collections
import datetime
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import forkrail_log  # noqa: E402 - beside this file on the instance


LINE = re.compile(r"^(\S+)\s+(\w+)\s+\S+: (.*)$")
FIELD = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|\S+)')

# us-east-1, arm64, checked 2026-11-04.
LAMBDA_GB_SECOND = 0.0000133334
LAMBDA_REQUEST = 0.20 / 1e6
EGRESS_GB = 0.09
REGIONAL_GB = 0.01  # Lambda outside a VPC <-> the router's public IP, each direction
GB = 2**30
TOP_PATHS = 15
LATEST_FAILURES = 20
SLOWEST = 10


def value(text):
    if text.startswith('"'):
        return text[1:-1].replace('\\"', '"')
    if text.startswith("{"):
        try:
            return json.loads(text)
        except ValueError:
            return text
    for kind in (int, float):
        try:
            return kind(text)
        except ValueError:
            pass
    return text


def pool_of(worker):
    return worker.split(".")[0] if "." in worker else "default"


def pct(values, q):
    return values[min(len(values) - 1, int(q * len(values)))] if values else None


def epoch(stamp):
    return datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()


def hour_of(ts):
    return datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).strftime("%m-%d %H:00")


def clock(ts):
    return datetime.datetime.fromtimestamp(ts, datetime.timezone.utc).strftime("%m-%d %H:%M:%S")


def peaks(intervals):
    """Peak overlap of (start, end) intervals, per hour of the moment it happened."""
    events = sorted([(s, 1) for s, _ in intervals] + [(e, -1) for _, e in intervals], key=lambda e: (e[0], e[1]))
    level, best = 0, collections.Counter()
    for ts, step in events:
        level += step
        hour = hour_of(ts)
        best[hour] = max(best[hour], level)
    return best


def short(text, n):
    text = str(text) if text is not None else "-"
    return text if len(text) <= n else text[: n - 1] + "…"


def main():
    deployments = json.loads(base64.b64decode(sys.argv[1]) or b"{}")

    project = lambda deployment: deployments.get(deployment, {}).get("project", deployment)
    stats = collections.defaultdict(lambda: {
        "requests": collections.Counter(), "status": collections.Counter(), "total_ms": [], "retried": 0,
        "bytes_in": 0, "bytes_out": 0, "spawned": 0, "cold": 0, "invocations": 0, "billed_gb_s": 0.0,
        "billed_s": 0.0, "deployments": set(),
        # HTTPS requests only, for the detail sections.
        "public": [], "workers": [], "cold_at": [], "app_lines": 0,
    })
    limit_mb = {}
    connected = {}
    warnings = collections.Counter()
    first = last = None
    for line in forkrail_log.lines(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None):
        match = LINE.match(line)
        if not match:
            continue
        stamp, level, rest = match.groups()
        head = FIELD.search(rest)
        message = (rest[: head.start()] if head else rest).strip()
        kv = {k: value(v) for k, v in FIELD.findall(rest)}
        if level in ("WARN", "ERROR"):
            warnings[f"{level} {message}"] += 1
        if message == "request":
            deployment = kv.get("deployment") or "default"
            s = stats[project(deployment)]
            s["deployments"].add(deployment)
            s["requests"][kv.get("entry", "tester")] += 1
            s["status"][kv.get("status")] += 1
            if isinstance(kv.get("total_ms"), int):
                s["total_ms"].append(kv["total_ms"])
            s["retried"] += kv.get("attempts", 1) > 1
            for key in ("bytes_in", "bytes_out"):
                if isinstance(kv.get(key), int):
                    s[key] += kv[key]
            if kv.get("entry") == "https" and isinstance(kv.get("total_ms"), int):
                kv["ts"] = epoch(stamp)
                s["public"].append(kv)
            first, last = first or stamp, stamp
        elif message == "app log":
            stats[project(kv.get("deployment") or "default")]["app_lines"] += 1
        elif message == "spawning worker":
            stats[project(pool_of(kv["worker"]))]["spawned"] += 1
        elif message == "worker connected":
            limit_mb[kv["worker"]] = kv.get("limit_mb", 0)
            connected[kv["worker"]] = epoch(stamp)
        elif message in ("worker ended", "worker failed"):
            deployment = pool_of(kv["worker"])
            s = stats[project(deployment)]
            secs = kv.get("secs", 0.0)
            memory = limit_mb.get(kv["worker"]) or deployments.get(deployment, {}).get("memory_mb", 0)
            s["invocations"] += 1
            s["billed_s"] += secs
            s["billed_gb_s"] += secs * memory / 1024
            ended = epoch(stamp)
            s["workers"].append((connected.pop(kv["worker"], ended - secs), ended))
            report = kv.get("report")
            if isinstance(report, dict) and report.get("env_invocations") == 1:
                s["cold"] += 1
                s["cold_at"].append(ended)

    print(f"router journal {sys.argv[2]} .. {sys.argv[3] if len(sys.argv) > 3 else 'now'}; requests {first} .. {last}")
    totals = collections.Counter()
    ordered = sorted(stats.items(), key=lambda kv: -sum(kv[1]["requests"].values()))
    for name, s in ordered:
        requests = sum(s["requests"].values())
        totals_ms = sorted(s["total_ms"])
        five = sum(n for status, n in s["status"].items() if isinstance(status, int) and status >= 500)
        lambda_cost = s["billed_gb_s"] * LAMBDA_GB_SECOND + s["invocations"] * LAMBDA_REQUEST
        egress = s["bytes_out"] / GB * EGRESS_GB
        regional = (s["bytes_in"] + s["bytes_out"]) / GB * REGIONAL_GB
        print(f"\n{name} ({', '.join(sorted(s['deployments'])) or 'no requests'})")
        print(f"  requests {requests}: {dict(s['requests'])}, status {dict(sorted(s['status'].items(), key=str))}")
        print(f"  total_ms p50={pct(totals_ms, .5)} p95={pct(totals_ms, .95)} p99={pct(totals_ms, .99)}; "
              f"retried {s['retried']}, 5xx {five}")
        print(f"  workers spawned {s['spawned']}, invocations ended {s['invocations']}, cold starts {s['cold']}, "
              f"billed {s['billed_s']:.1f} s = {s['billed_gb_s']:.1f} GB-s")
        print(f"  bytes in {s['bytes_in'] / 1e6:.1f} MB, out {s['bytes_out'] / 1e6:.1f} MB")
        print(f"  est. cost: Lambda ${lambda_cost:.4f}, egress ${egress:.4f}, Lambda<->router ${regional:.4f}, "
              f"total ${lambda_cost + egress + regional:.4f}")
        print(f"  app printed {s['app_lines']} lines (test-env.sh logs {name})")
        totals.update({"lambda": lambda_cost, "egress": egress, "regional": regional, "requests": requests})
        if s["public"]:
            details(s)
    if totals:
        print(f"\nall projects: {totals['requests']} requests, est. Lambda ${totals['lambda']:.4f}, egress "
              f"${totals['egress']:.4f}, Lambda<->router ${totals['regional']:.4f} "
              f"(plus the router: t4g.small, its disk and IPv4, about $0.55 a day)")
    if warnings:
        print("\nrouter warnings and errors:")
        for kind, n in warnings.most_common():
            print(f"  {n:>7}  {kind}")


def details(s):
    public = s["public"]
    in_flight = peaks([(r["ts"] - r["total_ms"] / 1000, r["ts"]) for r in public])
    workers = peaks(s["workers"])
    cold = collections.Counter(hour_of(ts) for ts in s["cold_at"])
    by_hour = collections.defaultdict(list)
    for r in public:
        by_hour[hour_of(r["ts"])].append(r)
    print("\n  HTTPS by hour (UTC)   reqs   4xx   5xx    p50    p95    p99    max  in-flight  workers  cold")
    for hour in sorted(by_hour):
        rows = by_hour[hour]
        ms = sorted(r["total_ms"] for r in rows)
        four = sum(1 for r in rows if 400 <= r.get("status", 0) < 500)
        five = sum(1 for r in rows if r.get("status", 0) >= 500)
        print(f"  {hour:<19} {len(rows):>6} {four:>5} {five:>5} {pct(ms, .5):>6} {pct(ms, .95):>6} {pct(ms, .99):>6} "
              f"{ms[-1]:>6} {in_flight.get(hour, 0):>10} {workers.get(hour, 0):>8} {cold.get(hour, 0):>5}")

    by_path = collections.defaultdict(list)
    for r in public:
        by_path[r.get("path", "?")].append(r)
    print(f"\n  busiest paths{' ' * 27}reqs    p50    p95    p99    max   4xx   5xx")
    for path, rows in sorted(by_path.items(), key=lambda kv: -len(kv[1]))[:TOP_PATHS]:
        ms = sorted(r["total_ms"] for r in rows)
        four = sum(1 for r in rows if 400 <= r.get("status", 0) < 500)
        five = sum(1 for r in rows if r.get("status", 0) >= 500)
        print(f"  {short(path, 38):<38} {len(rows):>6} {pct(ms, .5):>6} {pct(ms, .95):>6} {pct(ms, .99):>6} "
              f"{ms[-1]:>6} {four:>5} {five:>5}")

    def row(r):
        target = r.get("path", "?") + ("?" + r["query"] if r.get("query") else "")
        return (f"  {clock(r['ts'])} {r.get('status')} {short(r.get('outcome'), 12):<12} tries={r.get('attempts')} "
                f"{r['total_ms']:>6} ms queue={r.get('queue_ms')} {r.get('proto', '-')} {short(target, 70)} "
                f"client={r.get('client', '-')} agent={short(r.get('agent'), 40)} request={r.get('request')}")

    failures = [r for r in public if r.get("status", 0) >= 500 or r.get("outcome") not in ("ok", None)]
    if failures:
        print(f"\n  failures: {len(failures)} (5xx, or not finished: client_gone, timeout, worker_failed); latest:")
        for r in failures[-LATEST_FAILURES:]:
            print(row(r))
    print("\n  slowest:")
    for r in sorted(public, key=lambda r: -r["total_ms"])[:SLOWEST]:
        print(row(r))


if __name__ == "__main__":
    main()
