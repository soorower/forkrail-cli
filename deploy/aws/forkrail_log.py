"""The router's log, for the scripts that run on its instance (stdlib only).

The router writes hourly files, /var/log/forkrail/router.YYYY-MM-DD-HH.log (FORKRAIL_LOG_DIR); the
log export compresses each once it's over and sent to S3 (.log.gz) and deletes them after two days,
while S3 keeps everything. Before 2026-11-06 the router wrote to the journal, which is read for a
window that no file covers.

Lines are read one at a time: a day of the log is gigabytes, on the instance the router serves from.

Since 2026-11-09 the router writes JSON lines ({"timestamp", "level", "message", fields...,
"target"}); before, tracing's text format. `lines` gives every line in the text format, so the
scripts that parse it work on both: a JSON line is written out as the router would have written it.
"""

import glob
import gzip
import json
import os
import re
import subprocess

LOG_DIR = os.environ.get("FORKRAIL_LOG_DIR", "/var/log/forkrail")
NAME = re.compile(r"^router\.(\d{4}-\d{2}-\d{2}-\d{2})\.log(\.gz)?$")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def when(text):
    """A time as journalctl takes it ("1 day ago", "-20s", "2026-11-04 10:00:00", in UTC), as
    YYYY-MM-DDTHH:MM:SS, which compares as text with the log's timestamps."""
    out = subprocess.run(["date", "-u", "-d", text, "+%Y-%m-%dT%H:%M:%S"], capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit(f"can't read the time {text!r}: {out.stderr.strip()}")
    return out.stdout.strip()


def hourly_files(since, until=None):
    """The files that may hold lines from since (YYYY-MM-DDTHH:MM:SS) to until, oldest first. An
    hour being compressed has both forms; the plain one is complete until it's deleted."""
    first, last = since[:13].replace("T", "-"), (until[:13].replace("T", "-") if until else "9999")
    hours = {}
    for path in glob.glob(os.path.join(LOG_DIR, "router.*.log*")):
        match = NAME.match(os.path.basename(path))
        if match and first <= match.group(1) <= last:
            current = hours.get(match.group(1))
            if current is None or current.endswith(".gz"):
                hours[match.group(1)] = path
    return [hours[hour] for hour in sorted(hours)]


def lines(since_text, until_text=None):
    """The router's log lines from since to until (journalctl syntax), without colour codes."""
    since = when(since_text)
    until = when(until_text) if until_text else None
    paths = hourly_files(since, until)
    if not paths:
        yield from journal(since_text, until_text)
        return
    for path in paths:
        opener = gzip.open if path.endswith(".gz") else open
        try:
            with opener(path, "rt", errors="replace") as f:
                for line in f:
                    if line.startswith("{"):
                        line = as_text(line)
                        if line is None:
                            continue
                    stamp = line[:19]
                    if stamp < since or (until and stamp > until):
                        continue
                    yield line.rstrip("\n")
        except FileNotFoundError:  # compressed and deleted meanwhile; its .gz comes next time
            continue


def journal(since, until=None):
    command = ["journalctl", "-u", "forkrail-router*", "--no-pager", "-o", "cat", "--since", since]
    if until:
        command += ["--until", until]
    with subprocess.Popen(command, stdout=subprocess.PIPE, text=True, errors="replace") as proc:
        for line in proc.stdout:
            yield ANSI.sub("", line.rstrip("\n"))


# Rust's Debug escapes, as the text format wrote quoted strings.
ESCAPES = {'"': '\\"', "\\": "\\\\", "\n": "\\n", "\r": "\\r", "\t": "\\t", "\0": "\\0"}


def quoted(text):
    out = []
    for ch in text:
        if ch in ESCAPES:
            out.append(ESCAPES[ch])
        elif ord(ch) < 0x20 or ord(ch) == 0x7F:
            out.append("\\u{%x}" % ord(ch))
        else:
            out.append(ch)
    return '"' + "".join(out) + '"'


def field(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return json.dumps(value)
    if isinstance(value, str):
        # Display fields were written bare (a report's JSON among them), strings quoted.
        if value.startswith("{") or (value and not any(c.isspace() or c in '"=' for c in value)):
            return value
        return quoted(value)
    return quoted(json.dumps(value))


def as_text(line):
    """A JSON line as tracing's text format: `TIMESTAMP  LEVEL target: message key=value ...`."""
    try:
        r = json.loads(line)
    except ValueError:
        return None
    if not isinstance(r, dict) or "timestamp" not in r:
        return None
    head = f"{r.pop('timestamp')} {r.pop('level', 'INFO'):>5} {r.pop('target', '?')}: {r.pop('message', '')}"
    return " ".join([head] + [f"{k}={field(v)}" for k, v in r.items()])
