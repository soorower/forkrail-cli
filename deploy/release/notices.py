#!/usr/bin/env python3
"""THIRD-PARTY-NOTICES.txt for a forkrail release: the licences of the code forkrail's programs are
built with, which ask for their notices to go with every copy.

    python3 -I deploy/release/notices.py ROOT OUT [WEB_DIR]

- Code adapted into forkrail itself (deploy/release/licenses/): Vercel's routing-utils and builders
  (Apache-2.0), path-to-regexp (MIT), JA4 (BSD-3-Clause).
- The Rust crates ROOT's workspace depends on (`cargo metadata`, normal dependencies), each with the
  licence files its package has.
- With WEB_DIR, the dashboard's npm packages (`npm ls --omit=dev`, in a folder they're installed
  in), the same way.

Identical licence texts are printed once, under every package that has them. The other programs a
release carries (Vector, Chromium for screenshots, the builder image's toolchains) keep their own
licence files inside their archives.
"""

import hashlib
import json
import os
import re
import subprocess
import sys

LICENCE_FILE = re.compile(r"^(licen[cs]e|copying|copyright|notice|unlicense)([-._].*)?$", re.IGNORECASE)
ADAPTED = [
    ("vercel-Apache-2.0.txt", "Vercel's @vercel/routing-utils and @vercel/remix-builder (Apache-2.0), Copyright Vercel, Inc.",
     "forkrail's routing rules follow routing-utils, and its React Router packaging follows remix-builder."),
    ("path-to-regexp-MIT.txt", "path-to-regexp 6.1.0 (MIT)", "Ported to Rust for forkrail's routing rules."),
    ("ja4-BSD-3-Clause.txt", "JA4 TLS client fingerprinting (BSD-3-Clause), FoxIO", "Implemented from its specification in forkrail's router."),
]


def licence_texts(folder):
    """The licence files in a package's folder, by name."""
    texts = []
    try:
        names = sorted(os.listdir(folder))
    except OSError:
        return texts
    for name in names:
        path = os.path.join(folder, name)
        if LICENCE_FILE.match(name) and os.path.isfile(path) and not name.lower().endswith((".rs", ".js", ".ts", ".json", ".html")):
            with open(path, encoding="utf-8", errors="replace") as f:
                texts.append(f.read().strip("\n").rstrip())
    return texts


def rust_packages(root):
    out = subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=root, check=True, capture_output=True, text=True).stdout
    meta = json.loads(out)
    packages = {p["id"]: p for p in meta["packages"]}
    members = set(meta["workspace_members"])
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    seen, todo = set(), list(members)
    while todo:
        node = nodes.get(todo.pop())
        if not node:
            continue
        for dep in node["deps"]:
            normal = any(k.get("kind") is None for k in dep.get("dep_kinds", [{}]))
            if normal and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                todo.append(dep["pkg"])
    found = []
    for pid in seen - members:
        p = packages[pid]
        folder = os.path.dirname(p["manifest_path"])
        found.append((f'{p["name"]} {p["version"]}', p.get("license") or "see its licence files", p.get("repository") or "", licence_texts(folder)))
    return found


def npm_packages(web):
    out = subprocess.run(["npm", "ls", "--omit=dev", "--all", "--parseable"], cwd=web, capture_output=True, text=True).stdout
    root = os.path.realpath(web)
    found, seen = [], set()
    for line in out.splitlines():
        folder = os.path.realpath(line.strip())
        if not line.strip() or folder == root or folder in seen:
            continue
        seen.add(folder)
        try:
            with open(os.path.join(folder, "package.json"), encoding="utf-8") as f:
                pkg = json.load(f)
        except (OSError, ValueError):
            continue
        if str(pkg.get("name", "")).startswith("@forkrail/"):
            continue
        licence = pkg.get("license") or pkg.get("licenses") or "see its licence files"
        if isinstance(licence, dict):
            licence = licence.get("type", "")
        elif isinstance(licence, list):
            licence = " OR ".join(str(x.get("type", x)) if isinstance(x, dict) else str(x) for x in licence)
        repo = pkg.get("repository") or ""
        if isinstance(repo, dict):
            repo = repo.get("url", "")
        found.append((f'{pkg.get("name")} {pkg.get("version")}', str(licence), str(repo), licence_texts(folder)))
    return found


def section(title, packages):
    """Packages grouped by their licence texts, each text once."""
    by_text, bare = {}, []
    for name, licence, repo, texts in packages:
        if not texts:
            bare.append((name, licence, repo))
            continue
        key = hashlib.sha256("\n\n".join(texts).encode()).hexdigest()
        by_text.setdefault(key, (texts, []))[1].append(f"{name} ({licence})")
    lines = [f"{'=' * 78}\n{title}: {len(packages)} packages\n{'=' * 78}\n"]
    for texts, names in sorted(by_text.values(), key=lambda v: sorted(v[1])[0].lower()):
        lines.append("\n".join(sorted(names, key=str.lower)))
        lines.append("-" * 78)
        lines.append("\n\n".join(texts))
        lines.append("\n" + "-" * 78 + "\n")
    if bare:
        lines.append("These packages have no licence file of their own; their licences are as named:")
        lines.extend(f"{n} ({lic}){' ' + r if r else ''}" for n, lic, r in sorted(bare, key=lambda b: b[0].lower()))
        lines.append("")
    return "\n".join(lines)


def main():
    if len(sys.argv) not in (3, 4):
        sys.exit("usage: notices.py ROOT OUT [WEB_DIR]")
    root, out = sys.argv[1], sys.argv[2]
    here = os.path.join(root, "deploy", "release", "licenses")
    parts = [
        "forkrail includes, and is built with, the open-source code below. Each piece is under its own licence,\n"
        "printed here as its licence asks.\n",
        f"{'=' * 78}\nCode adapted into forkrail\n{'=' * 78}\n",
    ]
    for file, what, how in ADAPTED:
        with open(os.path.join(here, file), encoding="utf-8") as f:
            text = f.read().strip("\n").rstrip()
        parts.append(f"{what}\n{how}\n{'-' * 78}\n{text}\n{'-' * 78}\n")
    parts.append(section("Rust crates in forkrail's programs", rust_packages(root)))
    if len(sys.argv) == 4:
        web = npm_packages(sys.argv[3])
        if not web:
            sys.exit(f"{sys.argv[3]} has no npm packages installed: build it first")
        parts.append(section("npm packages in the dashboard", web))
    with open(out, "w", encoding="utf-8") as f:
        f.write("\n".join(parts))


if __name__ == "__main__":
    main()
