"""Print the router's journal between two times as JSON lines, for tools/tune-memory.py:

    python3 journal-json.py SINCE [UNTIL]      e.g. "2026-11-04 10:00:00"

One object per scheduler event or access-log line: {"ts": seconds since the epoch, "event": the
log message ("request", "spawning worker", "worker connected", "worker ended", ...), and its fields}.
Runs on the router instance (stdlib only).
"""

import datetime
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import forkrail_log  # noqa: E402 - beside this file on the instance

EVENTS = {"request", "spawning worker", "worker connected", "worker ended", "worker failed", "retiring worker",
          "worker died with requests in flight", "spawn failed", "route cpu share", "route memory cost", "router up"}
LINE = re.compile(r"^(\S+)\s+\w+\s+\S+: (.*)$")
FIELD = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|\S+)')


def value(text):
    if text.startswith('"'):
        return json.loads(text)
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


def main():
    for line in forkrail_log.lines(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else None):
        match = LINE.match(line)
        if not match:
            continue
        stamp, rest = match.groups()
        first = FIELD.search(rest)
        message = (rest[: first.start()] if first else rest).strip()
        if message not in EVENTS:
            continue
        ts = datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
        record = {"ts": round(ts, 6), "event": message}
        record.update((k, value(v)) for k, v in FIELD.findall(rest))
        print(json.dumps(record, separators=(",", ":")))


if __name__ == "__main__":
    main()
