"""The router's new log lines as JSON lines for S3, and what CloudWatch should see; then tidy up.

    python3 log-export.py collect LOG_DIR STATE OUT.jsonl
    python3 log-export.py tidy LOG_DIR STATE

`collect` reads each hourly file (router.YYYY-MM-DD-HH.log) from where STATE says the last export
stopped, up to its last whole line, and writes one object per line to OUT.jsonl: {"ts": seconds
since the epoch, "level": "INFO", "target": "forkrail_router::proxy", "msg": the event ("request",
"app log", ...), and its fields}; a line that isn't a tracing event is kept as {"ts", "msg"}. The
router's lines are JSON since 2026-11-09 and text before; both come out the same way. The new
positions go to STATE.next, which log-export.sh moves over STATE once the upload worked, so a failed
run is sent again and nothing twice. It prints {"requests": N, "errors_5xx": N, "p95_ms": N or null,
"lines": N, "cert_days_left": N or null, "mitigated": N} for requests on the public HTTPS listener,
the days left on the soonest certificate to expire from the issuing router's latest `certificates`
line, and the requests and connections the system mitigations refused (`mitigation` lines).

`tidy` compresses the files of past hours that were sent in full (.log.gz) and deletes compressed
ones older than KEEP_HOURS; S3 has them.

Runs on the router instance (stdlib only), from log-export.sh. Reads line by line: ten minutes of a
busy router's log is hundreds of megabytes.
"""

import datetime
import gzip
import json
import os
import re
import shutil
import sys

ANSI = re.compile(r"\x1b\[[0-9;]*m")
LINE = re.compile(r"^(\S+)\s+(\w+)\s+(\S+): (.*)$")
FIELD = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|\S+)')
NAME = re.compile(r"^router\.(\d{4}-\d{2}-\d{2}-\d{2})\.log$")
# Compressed hours kept on the instance.
KEEP_HOURS = 48
# A past hour's file is compressed only once it has been quiet this long, in seconds: a router may
# still be writing out the end of its buffer.
QUIET_S = 120


def value(text):
    if text.startswith('"'):
        try:
            return json.loads(text)
        except ValueError:
            return text[1:-1]
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


def record(message):
    """A log line as the object S3 keeps. Since 2026-11-09 the router writes JSON lines; before,
    text."""
    if message.startswith("{"):
        try:
            r = json.loads(message)
        except ValueError:
            r = None
        if isinstance(r, dict) and "timestamp" in r:
            stamp = r.pop("timestamp")
            try:
                ts = round(datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp(), 6)
            except ValueError:
                ts = 0.0
            out = {"ts": ts, "level": r.pop("level", None), "target": r.pop("target", None), "msg": r.pop("message", "")}
            out.update(r)
            return out
    message = ANSI.sub("", message)
    stamp = message.split(" ", 1)[0]
    try:
        ts = round(datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp(), 6)
    except ValueError:
        ts = 0.0
    match = LINE.match(message)
    if not match:
        return {"ts": ts, "msg": message}
    _, level, target, rest = match.groups()
    head = FIELD.search(rest)
    out = {"ts": ts, "level": level, "target": target, "msg": (rest[: head.start()] if head else rest).strip()}
    out.update((k, value(v)) for k, v in FIELD.findall(rest[head.start():] if head else ""))
    return out


def load(path):
    try:
        with open(path) as f:
            return json.load(f)
    except (FileNotFoundError, ValueError):
        return {}


def save(path, state):
    with open(path + ".tmp", "w") as f:
        json.dump(state, f)
    os.replace(path + ".tmp", path)


def collect(log_dir, state_path, out_path):
    state = load(state_path)
    positions = {}
    requests = errors = lines = mitigated = 0
    totals = []
    cert_days_left = None
    with open(out_path, "w") as out:
        for name in sorted(os.listdir(log_dir)):
            if not NAME.match(name):
                continue
            path = os.path.join(log_dir, name)
            start = state.get(name, 0)
            if os.path.getsize(path) < start:  # replaced: send it all
                start = 0
            with open(path, "rb") as f:
                f.seek(start)
                position = start
                for raw in f:
                    if not raw.endswith(b"\n"):  # still being written
                        break
                    position += len(raw)
                    if not raw.strip():
                        continue
                    line = record(raw.decode("utf-8", "replace").rstrip("\n"))
                    out.write(json.dumps(line, separators=(",", ":")) + "\n")
                    lines += 1
                    if line.get("msg") == "certificates" and isinstance(line.get("days_left"), (int, float)):
                        cert_days_left = line["days_left"]
                    if line.get("msg") == "mitigation":
                        for key in ("requests", "connections"):
                            if isinstance(line.get(key), int):
                                mitigated += line[key]
                    if line.get("msg") == "request" and line.get("entry") == "https":
                        requests += 1
                        if isinstance(line.get("status"), int) and line["status"] >= 500:
                            errors += 1
                        if isinstance(line.get("total_ms"), int):
                            totals.append(line["total_ms"])
            positions[name] = position
    save(state_path + ".next", positions)
    totals.sort()
    p95 = totals[min(len(totals) - 1, int(0.95 * len(totals)))] if totals else None
    print(json.dumps({"requests": requests, "errors_5xx": errors, "p95_ms": p95, "lines": lines, "cert_days_left": cert_days_left, "mitigated": mitigated}))


def tidy(log_dir, state_path):
    state = load(state_path)
    now = datetime.datetime.now(datetime.timezone.utc)
    current = now.strftime("%Y-%m-%d-%H")
    oldest = (now - datetime.timedelta(hours=KEEP_HOURS)).strftime("%Y-%m-%d-%H")
    for name in sorted(os.listdir(log_dir)):
        path = os.path.join(log_dir, name)
        match = NAME.match(name)
        if match and match.group(1) < current:
            quiet = now.timestamp() - os.path.getmtime(path) >= QUIET_S
            if quiet and state.get(name) == os.path.getsize(path):
                with open(path, "rb") as src, gzip.open(path + ".gz.tmp", "wb") as dst:
                    shutil.copyfileobj(src, dst)
                os.replace(path + ".gz.tmp", path + ".gz")
                os.remove(path)
                state.pop(name, None)
        elif name.startswith("router.") and name.endswith(".log.gz") and name[7:20] < oldest:
            os.remove(path)
    save(state_path, state)


if __name__ == "__main__":
    if len(sys.argv) == 5 and sys.argv[1] == "collect":
        collect(*sys.argv[2:5])
    elif len(sys.argv) == 4 and sys.argv[1] == "tidy":
        tidy(*sys.argv[2:4])
    else:
        raise SystemExit(__doc__)
