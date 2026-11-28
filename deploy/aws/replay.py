"""Load generator for tuning a function, run on the router instance (stdlib only):

    python3 replay.py PATHS_FILE MODE [options]

PATHS_FILE holds one request path with its query per line; blank lines and # comments are skipped.

Modes:
  sequential --rounds N --gap-ms G
      each path in turn, one at a time, N rounds. Every request has a worker to itself, so the
      worker reports its CPU, memory and (with FORKRAIL_PROFILE_PCT on the router) its CPU timeline.
  poisson --rate R --duration S
      random arrivals averaging R a second for S seconds, each a path picked at random, the way
      independent clients arrive.
  burst --size N --every S --duration S
      N requests at once every S seconds, the way one client fanning out arrives.

--host-header NAME sends `Host: NAME`, to reach one deployment through port 8080.

Prints the number of requests, their statuses and client-side latency percentiles.
"""

import argparse
import collections
import random
import sys
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor


HEADERS = {}


def fetch(url):
    started = time.monotonic()
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=HEADERS), timeout=60) as response:
            response.read()
            status = response.status
    except urllib.error.HTTPError as e:
        status = e.code
    except Exception as e:  # noqa: BLE001 - a load generator reports every failure the same way
        status = type(e).__name__
    return status, (time.monotonic() - started) * 1000


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("paths")
    parser.add_argument("mode", choices=["sequential", "poisson", "burst"])
    parser.add_argument("--host", default="http://127.0.0.1:8080")
    parser.add_argument("--host-header")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--gap-ms", type=float, default=200)
    parser.add_argument("--rate", type=float, default=1.0)
    parser.add_argument("--size", type=int, default=10)
    parser.add_argument("--every", type=float, default=5.0)
    parser.add_argument("--duration", type=float, default=60.0)
    parser.add_argument("--seed", type=int, default=1)
    args = parser.parse_args()
    if args.host_header:
        HEADERS["Host"] = args.host_header

    with open(args.paths) as f:
        paths = [line.strip() for line in f if line.strip() and not line.startswith("#")]
    rng = random.Random(args.seed)
    results, lock = [], threading.Lock()

    def run(path):
        result = fetch(args.host + path)
        with lock:
            results.append(result)

    if args.mode == "sequential":
        for _ in range(args.rounds):
            for path in paths:
                run(path)
                time.sleep(args.gap_ms / 1000)
    else:
        start = time.monotonic()
        with ThreadPoolExecutor(max_workers=512) as pool:
            at = 0.0
            while at < args.duration:
                delay = start + at - time.monotonic()
                if delay > 0:
                    time.sleep(delay)
                if args.mode == "poisson":
                    pool.submit(run, rng.choice(paths))
                    at += rng.expovariate(args.rate)
                else:
                    for _ in range(args.size):
                        pool.submit(run, rng.choice(paths))
                    at += args.every

    statuses = collections.Counter(status for status, _ in results)
    latencies = sorted(ms for _, ms in results)
    print(f"requests: {len(results)}, statuses: {dict(statuses)}")
    if latencies:
        at = lambda q: latencies[min(len(latencies) - 1, int(q * len(latencies)))]
        print(f"client ms: p50={at(.5):.0f} p95={at(.95):.0f} p99={at(.99):.0f} max={latencies[-1]:.0f}")


if __name__ == "__main__":
    sys.exit(main())
