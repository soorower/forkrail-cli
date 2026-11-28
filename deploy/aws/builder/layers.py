#!/usr/bin/env python3
"""The builder image's own layers, as tar files, for deploy/aws/builder.sh:

  layers.py node OUT.tar
      Node 24, npm and corepack, from a flattened image on stdin (`crane export` of AWS's Lambda
      Node 24 image), where that image has them: /var/lang, beside the Python image's own files
      (no file is in both).
  layers.py forkrail OUT.tar BUILDER RUNTIME
      forkrail's builder and runtime in /opt/forkrail, and in /opt/forkrail/bin shims that run pnpm
      and yarn through corepack (Lambda's /var/lang is read-only, so `corepack enable` can't).
  layers.py python VERSION OUT.tar
      Another Python (3.12, 3.14) from a flattened Lambda Python image on stdin: its versioned files
      in /var/lang (python3.12, lib/python3.12, libpython3.12), beside the base image's 3.13.
  layers.py node MAJOR OUT.tar
      Another Node (22) from a flattened Lambda Node image on stdin, moved to /opt/node<MAJOR>.
  layers.py go OUT.tar GO.tar.gz
      Go's official release in /opt/go.
  layers.py rust OUT.tar RUSTC.tar.xz CARGO.tar.xz RUST_STD.tar.xz ZIG.tar.xz CARGO_ZIGBUILD.tar.xz
      Rust (rustc, cargo and the standard library for aarch64 Linux) in /opt/rust, zig in /opt/zig
      and cargo-zigbuild beside cargo: zig links, and compiles C, which Lambda's images can't.

Every entry is owned by root, with one fixed time, so the same input makes the same layer: the
same digest, which ECR then has already.
"""

import io
import sys
import tarfile

MTIME = 315532800  # 1980-01-01
NODE_FILES = {"var/lang/bin/node", "var/lang/bin/npm", "var/lang/bin/npx", "var/lang/bin/corepack"}
NODE_TREES = ("var/lang/lib/node_modules", "var/lang/include/node")
# Folders the Python image has already: left as it has them.
BASE_DIRS = {"var", "var/lang", "var/lang/bin", "var/lang/lib", "var/lang/include", "opt"}
SHIMS = ("pnpm", "pnpx", "yarn", "yarnpkg")


def normalized(info):
    info.uid = info.gid = 0
    info.uname = info.gname = "root"
    info.mtime = MTIME
    info.pax_headers = {}
    return info


def wanted(name):
    if name in BASE_DIRS:
        return False
    return name in NODE_FILES or any(name == t or name.startswith(t + "/") for t in NODE_TREES)


def node(out):
    taken = set()
    with tarfile.open(fileobj=sys.stdin.buffer, mode="r|") as src, tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as dst:
        for member in src:
            name = member.name.removeprefix("./").rstrip("/")
            if not wanted(name):
                continue
            if member.islnk():
                target = member.linkname.removeprefix("./")
                if target not in taken:
                    sys.exit(f"{name} is a hard link to {target}, which the layer doesn't have")
                member.linkname = target
            member.name = name
            dst.addfile(normalized(member), src.extractfile(member) if member.isfile() else None)
            taken.add(name)
    missing = NODE_FILES - taken
    if missing:
        sys.exit(f"the Node image has no {', '.join(sorted(missing))}")
    print(f"{len(taken)} entries", file=sys.stderr)


def entry(name, mode, kind=tarfile.REGTYPE, size=0):
    info = tarfile.TarInfo(name)
    info.type, info.mode, info.size = kind, mode, size
    return normalized(info)


def forkrail(out, builder, runtime):
    with tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as dst:
        for folder in ("opt/forkrail", "opt/forkrail/bin"):
            dst.addfile(entry(folder, 0o755, tarfile.DIRTYPE))
        for name, path in (("forkrail-builder", builder), ("forkrail-runtime", runtime)):
            with open(path, "rb") as f:
                data = f.read()
            if data[:4] != b"\x7fELF" or data[18:20] != (183).to_bytes(2, "little"):
                sys.exit(f"{path} isn't a Linux arm64 executable")
            dst.addfile(entry(f"opt/forkrail/{name}", 0o755, size=len(data)), io.BytesIO(data))
        for tool in SHIMS:
            script = f'#!/bin/sh\nexec corepack {tool} "$@"\n'.encode()
            dst.addfile(entry(f"opt/forkrail/bin/{tool}", 0o755, size=len(script)), io.BytesIO(script))


def copy_member(src, dst, member, name, taken):
    """`member` of the tar `src` into `dst` as `name`: a hard link only to what's in the layer."""
    if member.islnk():
        target = taken.get(member.linkname.removeprefix("./"))
        if target is None:
            sys.exit(f"{name} is a hard link to {member.linkname}, which the layer doesn't have")
        member.linkname = target
    member.name = name
    dst.addfile(normalized(member), src.extractfile(member) if member.isfile() else None)


# What a build never needs of another Python: its own tests, the static library for embedding it,
# its IDE and Tk, and the image's preinstalled packages (boto3: a build's venv starts empty).
PYTHON_LEFT_OUT = ("/test/", "/config-", "/idlelib/", "/tkinter/", "/turtledemo/", "/site-packages/")


def python(version, out):
    """The files of Python `version` in a Lambda Python image: none shared with another version."""
    wanted = (f"var/lang/bin/python{version}", f"var/lang/lib/python{version}", f"var/lang/include/python{version}", f"var/lang/lib/libpython{version}")
    taken = {}
    with tarfile.open(fileobj=sys.stdin.buffer, mode="r|") as src, tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as dst:
        for member in src:
            name = member.name.removeprefix("./").rstrip("/")
            if not any(name == w or name.startswith(w + "/") or (w.endswith(version) and w.split("/")[-1].startswith("libpython") and name.startswith(w)) for w in wanted):
                continue
            if any(part in name + "/" for part in PYTHON_LEFT_OUT):
                continue
            copy_member(src, dst, member, name, taken)
            taken[name] = name
    if f"var/lang/bin/python{version}" not in taken:
        sys.exit(f"the image has no python{version}")
    print(f"python {version}: {len(taken)} entries", file=sys.stderr)


def node_major(major, out):
    """Node, npm and corepack from a Lambda Node image, moved from /var/lang to /opt/node<major>.
    Their symlinks are relative, so they still point at the right files."""
    home = f"opt/node{major}"
    taken = {}
    with tarfile.open(fileobj=sys.stdin.buffer, mode="r|") as src, tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as dst:
        for folder in (home, f"{home}/bin", f"{home}/lib", f"{home}/include"):
            dst.addfile(entry(folder, 0o755, tarfile.DIRTYPE))
        for member in src:
            name = member.name.removeprefix("./").rstrip("/")
            if not wanted(name):
                continue
            moved = home + name.removeprefix("var/lang")
            if member.islnk():
                member.linkname = home + member.linkname.removeprefix("./").removeprefix("var/lang")
            copy_member(src, dst, member, moved, taken)
            taken[moved] = moved
    if f"{home}/bin/node" not in taken:
        sys.exit("the image has no node")
    print(f"node {major}: {len(taken)} entries", file=sys.stderr)


def unpack(dst, archive, place, taken):
    """Every entry of `archive` whose `place(name)` is a path, there."""
    with tarfile.open(archive, "r:*") as src:
        for member in src:
            name = place(member.name.removeprefix("./").rstrip("/"))
            if not name or name in taken:
                continue
            if member.islnk():
                member.linkname = place(member.linkname.removeprefix("./"))
            copy_member(src, dst, member, name, {v: v for v in taken})
            taken.add(name)


def go(out, archive):
    taken = set()
    with tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as dst:
        dst.addfile(entry("opt", 0o755, tarfile.DIRTYPE))
        # Its own tests and API notes aren't needed to build.
        left_out = ("go/test/", "go/api/", "go/doc/")
        unpack(dst, archive, lambda n: "opt/" + n if (n == "go" or n.startswith("go/")) and not n.startswith(left_out) else None, taken)
    if "opt/go/bin/go" not in taken:
        sys.exit(f"{archive} has no go/bin/go")
    print(f"go: {len(taken)} entries", file=sys.stderr)


def rust(out, rustc, cargo, rust_std, zig, zigbuild):
    """A component's files are under `<package>/<component>/`; rustc finds its sysroot from its own
    place, so they all go in /opt/rust."""
    taken = set()

    def component(n):
        parts = n.split("/")
        if len(parts) < 3 or parts[-1] in ("manifest.in",) or parts[2] == "share":
            return None
        return "opt/rust/" + "/".join(parts[2:])

    def zig_place(n):
        parts = n.split("/", 1)
        return "opt/zig/" + parts[1] if len(parts) == 2 and parts[1] else None

    with tarfile.open(out, "w", format=tarfile.PAX_FORMAT) as dst:
        for folder in ("opt", "opt/rust", "opt/rust/bin", "opt/rust/lib", "opt/zig"):
            dst.addfile(entry(folder, 0o755, tarfile.DIRTYPE))
            taken.add(folder)
        for archive in (rustc, cargo, rust_std):
            unpack(dst, archive, component, taken)
        unpack(dst, zig, zig_place, taken)
        unpack(dst, zigbuild, lambda n: "opt/rust/bin/cargo-zigbuild" if n.split("/")[-1] == "cargo-zigbuild" else None, taken)
        # Cargo links build scripts with the host's `cc`, which the image hasn't got: zig's, in a
        # folder the builder puts first on PATH for Cargo alone (FORKRAIL_RUST_CC), so that nothing
        # else (pip) finds a C compiler it never had.
        dst.addfile(entry("opt/rust/cc-bin", 0o755, tarfile.DIRTYPE))
        # Through cargo-zigbuild's own `zig cc`, which leaves out the flags zig refuses (rustc's
        # `--fix-cortex-a53-843419` on arm64).
        for tool in ("cc", "c++", "ar"):
            script = f'#!/bin/sh\nexec /opt/rust/bin/cargo-zigbuild zig {tool} -- "$@"\n'.encode()
            dst.addfile(entry(f"opt/rust/cc-bin/{tool}", 0o755, size=len(script)), io.BytesIO(script))
            taken.add(f"opt/rust/cc-bin/{tool}")
    for need in ("opt/rust/bin/rustc", "opt/rust/bin/cargo", "opt/zig/zig", "opt/rust/bin/cargo-zigbuild"):
        if need not in taken:
            sys.exit(f"the Rust layer has no {need}")
    print(f"rust: {len(taken)} entries", file=sys.stderr)


if __name__ == "__main__":
    match sys.argv[1:]:
        case ["node", out]:
            node(out)
        case ["forkrail", out, builder, runtime]:
            forkrail(out, builder, runtime)
        case ["python", version, out]:
            python(version, out)
        case ["node", major, out]:
            node_major(major, out)
        case ["go", out, archive]:
            go(out, archive)
        case ["rust", out, rustc, cargo, rust_std, zig, zigbuild]:
            rust(out, rustc, cargo, rust_std, zig, zigbuild)
        case _:
            sys.exit(__doc__)
