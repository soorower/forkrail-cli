#!/usr/bin/env bash
# Forkrail.app, the installer in a window of its own (deploy/macos/Forkrail/main.swift), with the
# forkrail CLI for Apple silicon and Intel inside it, and Forkrail.dmg to download it in.
#
#   deploy/macos/build-app.sh OUT
#
# Writes OUT/Forkrail.app and OUT/Forkrail.dmg. With FORKRAIL_SIGN_IDENTITY (a "Developer ID
# Application" identity; `security find-identity -v -p codesigning`) the CLI, the app and the .dmg
# are signed with it, hardened runtime and a secure timestamp; with FORKRAIL_NOTARY_PROFILE too (a
# notarytool keychain profile), Apple notarizes the .dmg and its ticket is stapled to it, so it
# opens on any Mac, even offline. Without them the app is signed only for this Mac (ad hoc): enough
# to try it here.
#
# FORKRAIL_CLI, if set, is a universal forkrail CLI to put in the app instead of building one.
# Needs swiftc (Xcode or its Command Line Tools), cargo with the aarch64-apple-darwin and
# x86_64-apple-darwin targets, lipo and hdiutil.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/deploy/macos"

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }

[ $# -eq 1 ] || die "usage: $0 OUT"
[ "$(uname -s)" = Darwin ] || die "the Mac app is built on a Mac"
mkdir -p "$1"
OUT="$(cd "$1" && pwd)"
SIGN="${FORKRAIL_SIGN_IDENTITY:-}"
NOTARY="${FORKRAIL_NOTARY_PROFILE:-}"
for tool in swiftc lipo hdiutil codesign; do command -v "$tool" >/dev/null || die "needs $tool"; done
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/crates/cli/Cargo.toml" | head -1)"
export COPYFILE_DISABLE=1
# As publish.sh: no paths of this machine's in the binaries.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }--remap-path-prefix=$HOME=~"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
# The CLI is forkrail-cli in here, never "forkrail": a Mac's disk takes that and "Forkrail" (the
# app's own program, below) for one name.

if [ -n "${FORKRAIL_CLI:-}" ]; then
  cp "$FORKRAIL_CLI" "$work/forkrail-cli"
else
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    log "building the CLI for $target"
    (cd "$ROOT" && cargo build --release --target "$target" -p forkrail-cli >&2)
  done
  lipo -create -output "$work/forkrail-cli" "$ROOT/target/aarch64-apple-darwin/release/forkrail" "$ROOT/target/x86_64-apple-darwin/release/forkrail"
fi

log "building the app"
for arch in arm64 x86_64; do
  swiftc -O -swift-version 5 -target "$arch-apple-macos12" -framework AppKit -framework WebKit \
    -o "$work/Forkrail-$arch" "$HERE/Forkrail/main.swift"
done
lipo -create -output "$work/Forkrail" "$work/Forkrail-arm64" "$work/Forkrail-x86_64"

app="$OUT/Forkrail.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Helpers" "$app/Contents/Resources"
cp "$work/Forkrail" "$app/Contents/MacOS/Forkrail"
cp "$work/forkrail-cli" "$app/Contents/Helpers/forkrail"
cp "$HERE/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
sed "s/VERSION/$version/g" "$HERE/Info.plist" > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"
for file in LICENSE NOTICE; do cp "$ROOT/$file" "$app/Contents/Resources/$file"; done

# Inside out: the CLI, then the app around it.
if [ -n "$SIGN" ]; then
  log "signing as $SIGN"
  codesign --force --options runtime --timestamp --identifier com.forkrail.cli --sign "$SIGN" "$app/Contents/Helpers/forkrail"
  codesign --force --options runtime --timestamp --sign "$SIGN" "$app"
else
  log "no FORKRAIL_SIGN_IDENTITY: signing for this Mac only"
  codesign --force --identifier com.forkrail.cli --sign - "$app/Contents/Helpers/forkrail"
  codesign --force --sign - "$app"
fi
codesign --verify --deep --strict --verbose=2 "$app"
helper="$("$app/Contents/Helpers/forkrail" --version 2>/dev/null || true)"
[ "$helper" = "forkrail $version" ] || die "Contents/Helpers/forkrail isn't the forkrail $version CLI (it says: ${helper:-nothing})"

log "making Forkrail.dmg"
stage="$work/dmg"
mkdir -p "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
dmg="$OUT/Forkrail.dmg"
rm -f "$dmg"
hdiutil create -quiet -volname "Forkrail" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg"
if [ -n "$SIGN" ]; then
  codesign --force --timestamp --sign "$SIGN" "$dmg"
  if [ -n "$NOTARY" ]; then
    log "notarizing Forkrail.dmg (Apple usually answers within a few minutes)"
    xcrun notarytool submit "$dmg" --keychain-profile "$NOTARY" --wait --output-format json > "$work/notary.json"
    status="$(sed -n 's/.*"status" *: *"\([^"]*\)".*/\1/p' "$work/notary.json")"
    if [ "$status" != Accepted ]; then
      id="$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' "$work/notary.json")"
      die "notarization: ${status:-no answer}. Why: xcrun notarytool log ${id:-<id>} --keychain-profile $NOTARY"
    fi
    xcrun stapler staple "$dmg"
    log "notarized and stapled"
  else
    log "no FORKRAIL_NOTARY_PROFILE: signed, not notarized"
  fi
fi
(cd "$OUT" && shasum -a 256 Forkrail.dmg > Forkrail.dmg.sha256)
log "Forkrail $version: $app and $dmg ($(du -h "$dmg" | cut -f1))"
