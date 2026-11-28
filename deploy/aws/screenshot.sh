#!/usr/bin/env bash
# The screenshot function, forkrail-screenshot (deploy/screenshot, deploy/install/screenshot.yaml):
# headless Chromium for the dashboard's page previews.
#
#   deploy/aws/screenshot.sh release [--yes]   install its packages for Lambda (x86_64), zip and
#                                              upload it to the install's bucket, and apply the stack
#                                              with it (stack.sh shows the changes and asks first)
#   deploy/aws/screenshot.sh plan              the same, but only show what applying would change
#   deploy/aws/screenshot.sh shot URL          shoot one URL with your own credentials; the JPEG
#                                              goes to ./shot.jpg
#   deploy/aws/screenshot.sh local URL         shoot it here with this Mac's Chrome (no AWS)
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SRC="$ROOT/deploy/screenshot"
OUT="$ROOT/target/screenshot"
FUNCTION=forkrail-screenshot
CHROME="${FORKRAIL_SCREENSHOT_CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

# Packages for Lambda's Linux on x86_64, then the zip; prints its path. The same packages give the
# same zip (fixed times, sorted names).
build() {
  rm -rf "$OUT" && mkdir -p "$OUT/pkg"
  cp "$SRC/index.mjs" "$SRC/package.json" "$OUT/pkg/"
  (cd "$OUT/pkg" && npm install --omit=dev --os=linux --cpu=x64 --no-audit --no-fund --silent >&2)
  find "$OUT/pkg" -exec touch -t 198001010000 {} +
  (cd "$OUT/pkg" && find . -type f | LC_ALL=C sort | zip -X -q "$OUT/screenshot.zip" -@)
  echo "$OUT/screenshot.zip"
}

# Upload the zip under its hash, unless it's there; prints the key.
upload() {
  local zip="$1" account bucket sha key
  account="$(aws sts get-caller-identity --query Account --output text)"
  bucket="forkrail-artifacts-$account"
  sha="$(shasum -a 256 "$zip" | cut -c1-16)"
  key="screenshot/forkrail-screenshot-$sha.zip"
  if aws s3api head-object --bucket "$bucket" --key "$key" >/dev/null 2>&1; then
    log "s3://$bucket/$key is there already"
  else
    aws s3 cp "$zip" "s3://$bucket/$key" --only-show-errors
    log "uploaded s3://$bucket/$key ($(wc -c <"$zip" | tr -d ' ') bytes)"
  fi
  echo "$key"
}

cmd="${1:-}"
[ -n "$cmd" ] && shift
case "$cmd" in
  # Build the zip only and print its path (deploy/aws/bundle.sh, for `forkrail install`).
  zip) build ;;
  release)
    key="$(upload "$(build)")"
    "$ROOT/deploy/aws/stack.sh" apply screenshot "CodeKey=$key" "$@"
    ;;
  plan)
    key="$(upload "$(build)")"
    "$ROOT/deploy/aws/stack.sh" plan screenshot "CodeKey=$key"
    ;;
  shot)
    [ $# -eq 1 ] || die "usage: screenshot.sh shot URL"
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    aws lambda invoke --function-name "$FUNCTION" --cli-binary-format raw-in-base64-out \
      --payload "$(jq -nc --arg url "$1" '{url: $url}')" "$out" >/dev/null
    jq -r 'if .ok then .image else error(.error) end' "$out" | base64 -d >shot.jpg
    jq -c 'del(.image)' "$out"
    ;;
  local)
    [ $# -eq 1 ] || die "usage: screenshot.sh local URL"
    [ -d "$SRC/node_modules" ] || (cd "$SRC" && npm install --no-audit --no-fund --silent)
    FORKRAIL_SCREENSHOT_CHROME="$CHROME" node "$SRC/index.mjs" "$1" | tee >(jq -r '.image // empty' | base64 -d >shot.jpg) | jq -c 'del(.image)'
    ;;
  *) usage ;;
esac
