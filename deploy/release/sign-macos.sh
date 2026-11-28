#!/usr/bin/env bash
# Signs a macOS forkrail CLI with a Developer ID and has Apple notarize it, so macOS opens it even
# when it was downloaded in a browser (Gatekeeper checks the notarization online: a bare program
# can't have the ticket stapled to it).
#
#   deploy/release/sign-macos.sh PATH/TO/forkrail
#
# FORKRAIL_SIGN_IDENTITY: the "Developer ID Application: …" identity, from
#   `security find-identity -v -p codesigning`.
# FORKRAIL_NOTARY_PROFILE: a notarytool keychain profile (`xcrun notarytool store-credentials`).
# Without it the program is signed but not notarized.
set -euo pipefail

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }

[ $# -eq 1 ] && [ -f "$1" ] || die "usage: $0 PATH/TO/forkrail"
[ "$(uname -s)" = Darwin ] || die "signing needs macOS"
BIN="$1"
SIGN="${FORKRAIL_SIGN_IDENTITY:?set FORKRAIL_SIGN_IDENTITY to a Developer ID Application identity}"
NOTARY="${FORKRAIL_NOTARY_PROFILE:-}"

log "signing $BIN as $SIGN"
# Hardened runtime and a secure timestamp: both are what notarization asks for.
codesign --force --options runtime --timestamp --identifier com.forkrail.cli --sign "$SIGN" "$BIN"
codesign --verify --strict --verbose=2 "$BIN"

if [ -z "$NOTARY" ]; then
  log "FORKRAIL_NOTARY_PROFILE isn't set: signed, not notarized"
  exit 0
fi
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
ditto -c -k --keepParent "$BIN" "$work/forkrail.zip"
log "notarizing (Apple usually answers within a few minutes)"
xcrun notarytool submit "$work/forkrail.zip" --keychain-profile "$NOTARY" --wait --output-format json > "$work/result.json"
status="$(sed -n 's/.*"status" *: *"\([^"]*\)".*/\1/p' "$work/result.json")"
if [ "$status" != Accepted ]; then
  id="$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' "$work/result.json")"
  die "notarization: ${status:-no answer}. Why: xcrun notarytool log ${id:-<id>} --keychain-profile $NOTARY"
fi
log "notarized"
