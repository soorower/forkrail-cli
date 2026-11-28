#!/usr/bin/env bash
# Builds Forkrail.dmg signed and notarized (build-app.sh) and adds it to the published release of
# this checkout's version, as Forkrail.dmg with its .sha256: for a release published before the
# Mac app (publish.sh adds it to new ones). Its link for the newest release is
# https://forkrail.com/download/mac, a redirect to
# https://github.com/soorower/forkrail-cli/releases/latest/download/Forkrail.dmg.
#
#   FORKRAIL_NOTARY_PROFILE=<profile> deploy/macos/publish-app.sh
#
# FORKRAIL_SIGN_IDENTITY defaults to this Mac's first "Developer ID Application" identity.
# REPO defaults to soorower/forkrail-cli. Needs gh, signed in, allowed to upload to its releases.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
REPO="${REPO:-soorower/forkrail-cli}"

die() { echo "$*" >&2; exit 1; }

: "${FORKRAIL_NOTARY_PROFILE:?set FORKRAIL_NOTARY_PROFILE to a notarytool keychain profile}"
if [ -z "${FORKRAIL_SIGN_IDENTITY:-}" ]; then
  FORKRAIL_SIGN_IDENTITY="$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)"
  [ -n "$FORKRAIL_SIGN_IDENTITY" ] || die "this Mac has no Developer ID Application identity"
fi
export FORKRAIL_SIGN_IDENTITY FORKRAIL_NOTARY_PROFILE
command -v gh >/dev/null || die "needs gh"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/crates/cli/Cargo.toml" | head -1)"
gh release view "v$version" --repo "$REPO" >/dev/null || die "$REPO has no release v$version"

out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
"$ROOT/deploy/macos/build-app.sh" "$out"
gh release upload "v$version" --repo "$REPO" --clobber "$out/Forkrail.dmg" "$out/Forkrail.dmg.sha256"
echo "==> done: Forkrail.dmg is in v$version: https://github.com/$REPO/releases/latest/download/Forkrail.dmg" >&2
