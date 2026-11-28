#!/usr/bin/env bash
# Publishes a forkrail release where users download it, as a GitHub release of REPO tagged
# v<version>: the release folder deploy/aws/bundle.sh made, as one tar file that `forkrail
# install` downloads for its own version (crates/cli/src/install/download.rs); the CLI for macOS
# and Linux, which install.sh fetches, and for Windows, which install.ps1 fetches; and the two
# scripts themselves. Each file has a .sha256 beside it.
# REPO is also where forkrail's source is published, so its README is this repository's README.md.
# GitHub serves the downloads, so a release costs nothing to host.
#
#   deploy/release/publish.sh DIR            build the CLIs, pack DIR, and publish them
#   deploy/release/publish.sh DIR --dry-run  everything but publishing: the files are in DIR.out
#   deploy/release/publish.sh DIR --windows  only the Windows CLIs and install.ps1 (in DIR.windows),
#                                            added to the version's release that's out already
#
# Only the files manifest.json names are packed, and the dashboard's build is checked first: no
# source (bundle.sh builds it without), no source maps or .env files, nothing that looks like a key.
# The release is packed before the CLIs are built, and each CLI is built with its SHA-256
# (FORKRAIL_RELEASE_SHA256), so `forkrail install` takes that release and no other: run this
# script whole for every change to the release, never `gh release upload` a new tar by hand.
#
# REPO (default soorower/forkrail-cli) must be public for strangers to download from it.
# Needs gh (signed in, allowed to make releases in REPO), cargo, cargo-zigbuild, the Rust targets
# below (rustup target add ...), tar, zip and shasum. The version is crates/cli/Cargo.toml's: the CLI
# downloads the release of its own version, so publish a new version for every release.
#
# On a Mac it also builds the Mac app, Forkrail.dmg (deploy/macos/build-app.sh).
#
# The macOS CLIs are signed with the Developer ID Application identity FORKRAIL_SIGN_IDENTITY
# (`security find-identity -v -p codesigning` lists them) and notarized by Apple with the
# notarytool keychain profile FORKRAIL_NOTARY_PROFILE (`xcrun notarytool store-credentials`), so
# macOS opens them even when they're downloaded in a browser. Publishing needs both; a dry run
# without them builds the CLIs unsigned (`deploy/release/sign-macos.sh` signs a published one).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
REPO="${REPO:-soorower/forkrail-cli}"
MAC_TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
LINUX_TARGETS="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu"
# Windows on Intel and AMD, and on Arm. Built with zig like Linux's, as .zip files (Windows opens
# those itself). Unsigned: Windows says so the first time someone runs forkrail.exe it downloaded
# in a browser; install.ps1's download isn't marked as from the internet, so it isn't asked about.
WINDOWS_TARGETS="x86_64-pc-windows-gnu aarch64-pc-windows-gnullvm"

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }

usage="usage: $0 DIR [--dry-run] [--windows]"
[ $# -ge 1 ] || die "$usage"
DIR="$(cd "$1" && pwd)"
DRY=""
ONLY=""
shift
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY=--dry-run ;;
    --windows) ONLY=windows ;;
    *) die "$usage" ;;
  esac
done
[ -f "$DIR/manifest.json" ] || die "$DIR has no manifest.json: make it with deploy/aws/bundle.sh"
grep -q '"web_built"' "$DIR/manifest.json" || die "$DIR has no built dashboard: make it again with this checkout's bundle.sh"
! grep -q '"web":' "$DIR/manifest.json" || die "$DIR has the dashboard's source (web): make it again with this checkout's bundle.sh"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/crates/cli/Cargo.toml" | head -1)"
grep -q "\"version\": \"$version\"" "$DIR/manifest.json" || die "$DIR isn't version $version"
grep -q '"notices"' "$DIR/manifest.json" && [ -f "$DIR/LICENSE" ] && [ -f "$DIR/NOTICE" ] || die "$DIR has no LICENSE, NOTICE and notices: make it again with this checkout's bundle.sh"
for tool in cargo cargo-zigbuild tar zip shasum; do command -v "$tool" >/dev/null || die "needs $tool"; done
# As bundle.sh: no paths of this machine's in the binaries.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$HOME=~"
[ "$DRY" = --dry-run ] || command -v gh >/dev/null || die "needs gh"
SIGN="${FORKRAIL_SIGN_IDENTITY:-}"
NOTARY="${FORKRAIL_NOTARY_PROFILE:-}"
if [ "$(uname -s)" = Darwin ] && [ "$DRY" != --dry-run ] && [ "$ONLY" != windows ] && { [ -z "$SIGN" ] || [ -z "$NOTARY" ]; }; then
  die "set FORKRAIL_SIGN_IDENTITY and FORKRAIL_NOTARY_PROFILE: macOS CLIs are published signed and notarized"
fi

# A Windows-only run keeps its files apart, so the whole release's DIR.out stays as it was.
OUT="$DIR.out"
[ "$ONLY" != windows ] || OUT="$DIR.windows"
rm -rf "$OUT"
mkdir -p "$OUT"
# macOS tar keeps files' extended attributes (com.apple.provenance) and ._ files otherwise,
# which other tars warn about.
export COPYFILE_DISABLE=1
sum() { (cd "$(dirname "$1")" && shasum -a 256 "$(basename "$1")") > "$1.sha256"; }

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

if [ "$ONLY" != windows ]; then
log "checking the dashboard's build"
mkdir -p "$stage/web"
tar -xzf "$DIR/web-built.tar.gz" -C "$stage/web"
found="$(find "$stage/web" \( -name source.zip -o -name '*.map' -o -name '.env*' -o -name '*.pem' \) -print)"
[ -z "$found" ] || die "the dashboard's build has files that mustn't be published:
$found"
keys='AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----|sk_live_[0-9A-Za-z]{10,}|gh[pousr]_[0-9A-Za-z]{30,}|github_pat_[0-9A-Za-z_]{20,}|xox[abprs]-[0-9A-Za-z-]{10,}'
found="$(grep -rlE "$keys" "$stage/web" || true)"
[ -z "$found" ] || die "the dashboard's build has what looks like a key:
$found"
rm -rf "$stage/web"

# The manifest and the files it names, nothing else the folder has.
log "packing the release (version $version)"
mkdir -p "$stage/forkrail-release-$version"
ln -s "$DIR/manifest.json" "$stage/forkrail-release-$version/manifest.json"
sed -n 's/^ *"\([a-z_]*\)": "\([^"]*\)",\{0,1\}$/\1 \2/p' "$DIR/manifest.json" | grep -Ev '^(version|commit) ' | cut -d' ' -f2 |
  while IFS= read -r file; do
    case "$file" in */* | .*) die "manifest.json names $file, which isn't a file in $DIR" ;; esac
    [ -e "$DIR/$file" ] || die "$DIR has no $file"
    ln -s "$DIR/$file" "$stage/forkrail-release-$version/$file"
  done
tar --no-xattrs -C "$stage" -chf "$OUT/forkrail-release-$version.tar" "forkrail-release-$version"
sum "$OUT/forkrail-release-$version.tar"
log "packed: $(tar -tf "$OUT/forkrail-release-$version.tar" | grep -c .) entries, $(du -h "$OUT/forkrail-release-$version.tar" | cut -f1)"
fi
# Every CLI built below installs this release and no other (crates/cli/src/install/download.rs
# `pinned`): the .sha256 published beside it is only as trustworthy as the release itself. With
# --windows, the Windows CLIs install the release this version already published.
if [ "$ONLY" = windows ]; then
  FORKRAIL_RELEASE_SHA256="$(gh release download "v$version" --repo "$REPO" --pattern "forkrail-release-$version.tar.sha256" --output - | cut -d' ' -f1)"
else
  FORKRAIL_RELEASE_SHA256="$(cut -d' ' -f1 "$OUT/forkrail-release-$version.tar.sha256")"
fi
printf '%s' "$FORKRAIL_RELEASE_SHA256" | grep -Eq '^[0-9a-f]{64}$' || die "the release's SHA-256 isn't one: $FORKRAIL_RELEASE_SHA256"
export FORKRAIL_RELEASE_SHA256

build() {
  local target="$1" tool="$2"
  log "building the CLI for $target"
  case "$tool-$target" in
    # aws-lc's extra CPU-jitter entropy source needs POSIX headers that zig's MinGW doesn't have;
    # without it aws-lc seeds from Windows' own generator, as BoringSSL does everywhere.
    zig-*-windows-*) (cd "$ROOT" && AWS_LC_SYS_NO_JITTER_ENTROPY=1 cargo zigbuild --release --target "$target" -p forkrail-cli >&2) ;;
    zig-*) (cd "$ROOT" && cargo zigbuild --release --target "$target.2.17" -p forkrail-cli >&2) ;;
    *) (cd "$ROOT" && cargo build --release --target "$target" -p forkrail-cli >&2) ;;
  esac
  mkdir -p "$stage/$target"
  case "$target" in
    *-windows-*)
      cp "$ROOT/target/$target/release/forkrail.exe" "$DIR/LICENSE" "$DIR/NOTICE" "$DIR/THIRD-PARTY-NOTICES.txt" "$stage/$target/"
      rm -f "$OUT/forkrail-$target.zip"
      (cd "$stage/$target" && zip -q -X "$OUT/forkrail-$target.zip" forkrail.exe LICENSE NOTICE THIRD-PARTY-NOTICES.txt)
      sum "$OUT/forkrail-$target.zip"
      ;;
    *)
      cp "$ROOT/target/$target/release/forkrail" "$DIR/LICENSE" "$DIR/NOTICE" "$DIR/THIRD-PARTY-NOTICES.txt" "$stage/$target/"
      case "$target" in *-apple-darwin) [ -z "$SIGN" ] || "$ROOT/deploy/release/sign-macos.sh" "$stage/$target/forkrail" ;; esac
      tar --no-xattrs -C "$stage/$target" -czf "$OUT/forkrail-$target.tar.gz" forkrail LICENSE NOTICE THIRD-PARTY-NOTICES.txt
      sum "$OUT/forkrail-$target.tar.gz"
      ;;
  esac
}
if [ "$ONLY" = windows ]; then
  log "only Windows: the release's other files stay as they are"
elif [ "$(uname -s)" = Darwin ]; then
  for t in $MAC_TARGETS; do build "$t" cargo; done
  # The Mac app, with both CLIs in it, as Forkrail.dmg (signed and notarized when they are).
  lipo -create -output "$stage/forkrail-universal" "$stage/aarch64-apple-darwin/forkrail" "$stage/x86_64-apple-darwin/forkrail"
  FORKRAIL_CLI="$stage/forkrail-universal" "$ROOT/deploy/macos/build-app.sh" "$stage/app"
  cp "$stage/app/Forkrail.dmg" "$stage/app/Forkrail.dmg.sha256" "$OUT/"
else
  log "not on a Mac: no macOS CLI in this release"
fi
[ "$ONLY" = windows ] || for t in $LINUX_TARGETS; do build "$t" zig; done
for t in $WINDOWS_TARGETS; do build "$t" zig; done

# macOS's /bin/sh (bash 3.2) misreads non-ASCII text next to a variable, and Windows PowerShell
# reads a script with no byte order mark in the system's code page: keep both scripts ASCII.
scripts="install.sh install.ps1"
[ "$ONLY" != windows ] || scripts=install.ps1
for script in $scripts; do
  ! LC_ALL=C grep -n '[^[:print:][:space:]]' "$ROOT/deploy/release/$script" || die "$script has non-ASCII characters"
  cp "$ROOT/deploy/release/$script" "$OUT/$script"
  sum "$OUT/$script"
done
[ "$ONLY" = windows ] || cp "$DIR/LICENSE" "$DIR/NOTICE" "$DIR/THIRD-PARTY-NOTICES.txt" "$OUT/"

log "the release's files, in $OUT:"
ls -l "$OUT" >&2
if [ "$DRY" = --dry-run ]; then
  log "dry run: nothing published"
  exit 0
fi

tag="v$version"
commit="$(sed -n 's/.*"commit": "\([0-9a-f]*\)".*/\1/p' "$DIR/manifest.json")"
notes="forkrail $version, from commit ${commit:-unknown}. Install the CLI on macOS or Linux with:

    curl -fsSL https://forkrail.com/install | sh

or on Windows, in PowerShell:

    irm https://forkrail.com/install.ps1 | iex

then run \`forkrail install\`. On a Mac you can use the app instead: https://forkrail.com/download/mac (Forkrail.dmg). forkrail is open source under the Apache License 2.0."
if gh release view "$tag" --repo "$REPO" >/dev/null 2>&1; then
  log "updating the release $tag in $REPO"
  gh release upload "$tag" --repo "$REPO" --clobber "$OUT"/*
  # Its notes name every system the release has a CLI for.
  [ "$ONLY" != windows ] || gh release edit "$tag" --repo "$REPO" --notes "$notes" >/dev/null
elif [ "$ONLY" = windows ]; then
  die "$REPO has no release $tag to add Windows to: publish the whole release first (without --windows)"
else
  log "publishing $tag in $REPO"
  gh release create "$tag" --repo "$REPO" --title "forkrail $version" --notes "$notes" "$OUT"/*
fi
log "published: https://github.com/$REPO/releases/tag/$tag"
