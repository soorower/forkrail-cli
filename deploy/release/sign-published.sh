#!/usr/bin/env bash
# Signs and notarizes the macOS CLIs of a release that's already published, and puts them back:
# for a release published before its CLIs were signed. Each one is downloaded from the release,
# checked against its SHA-256, signed and notarized (sign-macos.sh), packed again with its licence
# files, and uploaded over the old one with a new .sha256.
#
#   FORKRAIL_NOTARY_PROFILE=<profile> deploy/release/sign-published.sh VERSION
#
# FORKRAIL_SIGN_IDENTITY defaults to this Mac's first "Developer ID Application" identity.
# REPO defaults to soorower/forkrail-cli. Needs gh, signed in, allowed to upload to its releases.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="${REPO:-soorower/forkrail-cli}"

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }

[ $# -eq 1 ] || die "usage: FORKRAIL_NOTARY_PROFILE=<profile> $0 VERSION"
version="${1#v}"
[ "$(uname -s)" = Darwin ] || die "signing needs macOS"
: "${FORKRAIL_NOTARY_PROFILE:?set FORKRAIL_NOTARY_PROFILE to a notarytool keychain profile}"
if [ -z "${FORKRAIL_SIGN_IDENTITY:-}" ]; then
  FORKRAIL_SIGN_IDENTITY="$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)"
  [ -n "$FORKRAIL_SIGN_IDENTITY" ] || die "this Mac has no Developer ID Application identity"
fi
export FORKRAIL_SIGN_IDENTITY FORKRAIL_NOTARY_PROFILE COPYFILE_DISABLE=1
command -v gh >/dev/null || die "needs gh"

base="https://github.com/$REPO/releases/download/v$version"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
for target in aarch64-apple-darwin x86_64-apple-darwin; do
  file="forkrail-$target.tar.gz"
  log "$file"
  curl -fsSL "$base/$file" -o "$work/$file"
  want="$(curl -fsSL "$base/$file.sha256" | cut -d' ' -f1)"
  [ "$(shasum -a 256 "$work/$file" | cut -d' ' -f1)" = "$want" ] || die "$file doesn't match its published SHA-256"
  mkdir "$work/$target"
  tar -xzf "$work/$file" -C "$work/$target"
  "$HERE/sign-macos.sh" "$work/$target/forkrail"
  (cd "$work/$target" && tar --no-xattrs -czf "../$file" forkrail LICENSE NOTICE THIRD-PARTY-NOTICES.txt)
  (cd "$work" && shasum -a 256 "$file" > "$file.sha256")
done
log "uploading to v$version in $REPO"
gh release upload "v$version" --repo "$REPO" --clobber "$work"/forkrail-*-apple-darwin.tar.gz "$work"/forkrail-*-apple-darwin.tar.gz.sha256
log "done: the macOS CLIs of v$version are signed and notarized"
