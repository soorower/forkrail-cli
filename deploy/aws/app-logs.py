"""Print apps' output from the router's journal: python3 app-logs.py SINCE DEPLOYMENT...

SINCE is anything journalctl's --since takes ("1 hour ago", "2026-11-04 10:00:00"); a DEPLOYMENT of
`default` means the default pool. One line per app log line:

    2026-11-04 16:23:33.495 node1111 stderr | node-hello on stderr

plus a note wherever the router or a worker dropped lines. Runs on the router instance (stdlib only).
"""

import datetime
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import forkrail_log  # noqa: E402 - beside this file on the instance


LINE = re.compile(r"^(\S+)\s+\w+\s+\S+: (.*)$")
FIELD = re.compile(r'(\w+)=("(?:[^"\\]|\\.)*"|\S+)')
# Rust's Debug escapes: \" \\ \n \r \t \0 \' and \u{1b}.
ESCAPE = re.compile(r"\\(u\{([0-9a-fA-F]+)\}|.)")
SIMPLE = {"n": "\n", "r": "\r", "t": "\t", "0": "\0"}


def unquote(text):
    if not text.startswith('"'):
        return text
    return ESCAPE.sub(lambda m: chr(int(m.group(2), 16)) if m.group(2) else SIMPLE.get(m.group(1), m.group(1)), text[1:-1])


def main():
    since, wanted = sys.argv[1], set(sys.argv[2:])
    shown = 0
    for raw in forkrail_log.lines(since):
        match = LINE.match(raw)
        if not match or ("app log" not in raw and "dropped" not in raw and "log buffer overflowed" not in raw):
            continue
        stamp, rest = match.groups()
        first = FIELD.search(rest)
        message = (rest[: first.start()] if first else rest).strip()
        fields = {k: unquote(v) for k, v in FIELD.findall(rest)}
        if fields.get("deployment") not in wanted:
            continue
        when = datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00")).strftime("%Y-%m-%d %H:%M:%S.%f")[:-3]
        if message == "app log":
            print(f"{when} {fields['deployment']} {fields.get('stream', '?'):<6} | {fields.get('line', '')}")
            shown += 1
        else:
            print(f"{when} {fields['deployment']} ------ | ({message})")
    if shown == 0:
        print(f"no app output since {since} for {' '.join(sorted(wanted))}")


if __name__ == "__main__":
    main()
