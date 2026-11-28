#!/usr/bin/env bash
# A forkrail release as a folder, for `forkrail install --release DIR` (crates/cli/src/install/release.rs):
# the router's binary, the functions' zips, Vector for the routers, the dashboard already built,
# and the builder's image as an OCI image layout, named in manifest.json. Releases are published for
# anyone to download (deploy/release/publish.sh), so the dashboard's source isn't in one: it's built
# without it.
#
#   deploy/aws/bundle.sh DIR [BUILDER_IMAGE|keep]
#
# Builds the router and the Lambda binaries for arm64 (cargo-zigbuild), zips the functions with
# their own scripts (`zip`), builds web/ (the forkrail-dashboard checkout) at its HEAD with `forkrail
# build` (so an install publishes the dashboard without building it: a new AWS
# account's functions may get only 512 MB, and the build needs about 1.5 GB), and copies the
# builder image BUILDER_IMAGE (a registry reference, `repo@sha256:...`; crane must be signed in to
# its registry) into DIR/builder with crane. Without BUILDER_IMAGE, the image of the forkrail-builder
# function that AWS_PROFILE reaches is used: read only, nothing in that account changes. `keep`
# keeps the image DIR has from an earlier run and reads no AWS account at all.
#
# FORKRAIL_WEB_DIR is the forkrail-dashboard checkout to build (default web/ beside this repo's root).
#
# The release carries forkrail's LICENSE, NOTICE and THIRD-PARTY-NOTICES.txt (deploy/release/notices.py:
# the licences of the Rust crates and the dashboard's npm packages in it). Paths under $HOME are
# written as ~ in the binaries (--remap-path-prefix), so they don't name this machine's folders.
#
# Needs cargo-zigbuild, crane, npm, zip, git, curl (all from Homebrew) and python3.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET=aarch64-unknown-linux-gnu
RELEASE="$ROOT/target/$TARGET/release"
VECTOR_VERSION=0.60.0
VECTOR_TARBALL=vector-$VECTOR_VERSION-aarch64-unknown-linux-musl.tar.gz
VECTOR_SHA256=e2984f826aa61bf3ea98fba4cccde2437f645794b2f8a3d45c97df9594dcfbd1
VECTOR_URL="https://packages.timber.io/vector/$VECTOR_VERSION/$VECTOR_TARBALL"
WEB="${FORKRAIL_WEB_DIR:-$ROOT/web}"

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }

[ $# -ge 1 ] || die "usage: $0 DIR [BUILDER_IMAGE]"
DIR="$1"
IMAGE="${2:-}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"
for tool in cargo-zigbuild crane npm zip git curl shasum python3; do command -v "$tool" >/dev/null || die "needs $tool"; done
[ -e "$WEB/.git" ] || die "no forkrail-dashboard checkout at $WEB (set FORKRAIL_WEB_DIR)"
[ -f "$ROOT/LICENSE" ] && [ -f "$ROOT/NOTICE" ] || die "$ROOT has no LICENSE and NOTICE"
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$HOME=~"

# macOS tar keeps files' extended attributes (com.apple.provenance) and ._ files otherwise,
# which other tars warn about.
export COPYFILE_DISABLE=1
log "building the arm64 binaries"
(cd "$ROOT" && cargo zigbuild --release --target "$TARGET.2.34" -p forkrail-router -p forkrail-runtime -p forkrail-control -p forkrail-guard >&2)
cp "$RELEASE/forkrail-router" "$DIR/forkrail-router"

for fn in control guard notify image screenshot; do
  log "zipping $fn"
  zip="$(FORKRAIL_PREBUILT=1 "$ROOT/deploy/aws/$fn.sh" zip)"
  cp "$zip" "$DIR/$fn.zip"
done

if [ ! -f "$DIR/$VECTOR_TARBALL" ] || [ "$(shasum -a 256 "$DIR/$VECTOR_TARBALL" | cut -d' ' -f1)" != "$VECTOR_SHA256" ]; then
  log "downloading Vector $VECTOR_VERSION"
  curl -fsSL "$VECTOR_URL" -o "$DIR/$VECTOR_TARBALL"
  [ "$(shasum -a 256 "$DIR/$VECTOR_TARBALL" | cut -d' ' -f1)" = "$VECTOR_SHA256" ] || die "Vector's SHA-256 doesn't match"
fi

# The dashboard at its HEAD, built as the install's builder would build it: one build for every
# install, which each publishes with an id of its own. Its settings come at run time, from the
# function's environment, so nothing of an install is in it, and FORKRAIL_NO_SOURCE leaves its
# source out. A release made before this one had the source as web.tar.gz: it goes.
log "building the dashboard ($WEB at $(git -C "$WEB" rev-parse --short HEAD))"
rm -f "$DIR/web.tar.gz"
(cd "$ROOT" && cargo build --release -p forkrail-cli >&2)
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/src" "$work/out"
git -C "$WEB" archive --format=tar --prefix=forkrail-web/ HEAD | tar -xf - -C "$work/src"
cat > "$work/request.json" <<JSON
{"id": "00000000", "project": "forkrail-web", "target": "production", "base_domain": "forkrail.invalid", "deployer": "forkrail release"}
JSON
FORKRAIL_NO_SOURCE=1 FORKRAIL_BOOTSTRAP="$RELEASE/forkrail-runtime" "$ROOT/target/release/forkrail" build "$work/src/forkrail-web" --out "$work/out/forkrail-web-built" --request "$work/request.json" >&2
[ ! -e "$work/out/forkrail-web-built/source.zip" ] || die "the dashboard's build kept its source"
tar --no-xattrs -C "$work/out" -czf "$DIR/web-built.tar.gz" forkrail-web-built

log "writing the licences"
cp "$ROOT/LICENSE" "$ROOT/NOTICE" "$DIR/"
python3 -I "$ROOT/deploy/release/notices.py" "$ROOT" "$DIR/THIRD-PARTY-NOTICES.txt" "$work/src/forkrail-web"

if [ "$IMAGE" = keep ]; then
  [ -f "$DIR/builder/index.json" ] || die "keep: $DIR has no builder image from an earlier run"
  log "keeping the builder image in $DIR/builder"
elif [ -z "$IMAGE" ]; then
  IMAGE="$(aws lambda get-function --function-name forkrail-builder --query 'Code.ImageUri' --output text)"
  # Lambda reports the tag it was given; crane wants the digest it resolved to.
  resolved="$(aws lambda get-function --function-name forkrail-builder --query 'Code.ResolvedImageUri' --output text)"
  [ -n "$resolved" ] && [ "$resolved" != None ] && IMAGE="$resolved"
  registry="${IMAGE%%/*}"
  aws ecr get-login-password | crane auth login "$registry" -u AWS --password-stdin >&2
fi
if [ "$IMAGE" != keep ]; then
  log "copying the builder image $IMAGE"
  rm -rf "$DIR/builder"
  crane pull --format=oci --platform linux/arm64 "$IMAGE" "$DIR/builder"
fi

commit="$(git -C "$ROOT" rev-parse HEAD)"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/crates/cli/Cargo.toml" | head -1)"
cat > "$DIR/manifest.json" <<JSON
{
  "version": "$version",
  "commit": "$commit",
  "router": "forkrail-router",
  "control": "control.zip",
  "notify": "notify.zip",
  "guard": "guard.zip",
  "image": "image.zip",
  "screenshot": "screenshot.zip",
  "vector": "$VECTOR_TARBALL",
  "web_built": "web-built.tar.gz",
  "builder": "builder",
  "license": "LICENSE",
  "notice": "NOTICE",
  "notices": "THIRD-PARTY-NOTICES.txt"
}
JSON
log "release in $DIR ($(du -sh "$DIR" | cut -f1))"
