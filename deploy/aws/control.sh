#!/usr/bin/env bash
# The control function, forkrail-control (crates/control, deploy/install/control.yaml): what the
# dashboard invokes to promote, roll back, redeploy, and change settings and env vars.
#
#   deploy/aws/control.sh release [--yes]   build it for Lambda (arm64), upload the zip to the
#                                           install's bucket, and apply the stack with it
#                                           (stack.sh shows the changes and asks first)
#   deploy/aws/control.sh plan              build and upload, then show what applying would change
#   deploy/aws/control.sh upload            build and upload only; prints the zip's key for
#                                           `stack.sh apply control CodeKey=KEY` (release.sh)
#   deploy/aws/control.sh key               build only; prints the key the zip would have
#   deploy/aws/control.sh invoke JSON       call it with your own credentials, e.g.
#                                           '{"action": "ping"}' or '{"action": "source_list", "id": "a1b2c3d4"}'
#   deploy/aws/control.sh logs [MINUTES]    its audit lines from the last MINUTES (default 60)
#
# Needs cargo-zigbuild (Homebrew's), like the router and the runtime. FORKRAIL_PREBUILT=1 zips the
# binary target/ has, without compiling (release.sh compiles everything at once first).
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET=aarch64-unknown-linux-gnu
OUT="$ROOT/target/control"
FUNCTION=forkrail-control

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

# Build and zip it; prints the zip's path. The zip is the same for the same binary.
build() {
  [ "${FORKRAIL_PREBUILT:-0}" = 1 ] || (cd "$ROOT" && cargo zigbuild --release --target "$TARGET.2.34" -p forkrail-control >&2)
  [ -x "$ROOT/target/$TARGET/release/forkrail-control" ] || die "no $ROOT/target/$TARGET/release/forkrail-control"
  rm -rf "$OUT" && mkdir -p "$OUT"
  cp "$ROOT/target/$TARGET/release/forkrail-control" "$OUT/bootstrap"
  chmod 755 "$OUT/bootstrap"
  touch -t 198001010000 "$OUT/bootstrap"
  (cd "$OUT" && zip -X -q control.zip bootstrap)
  echo "$OUT/control.zip"
}

# The zip's key in the install's bucket: its hash.
key_of() { echo "control/forkrail-control-$(shasum -a 256 "$1" | cut -c1-16).zip"; }

# Upload the zip under its hash, unless it's there; prints the key.
upload() {
  local zip="$1" account bucket key
  account="$(aws sts get-caller-identity --query Account --output text)"
  bucket="forkrail-artifacts-$account"
  key="$(key_of "$zip")"
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
    "$ROOT/deploy/aws/stack.sh" apply control "CodeKey=$key" "$@"
    ;;
  plan)
    key="$(upload "$(build)")"
    "$ROOT/deploy/aws/stack.sh" plan control "CodeKey=$key"
    ;;
  upload) upload "$(build)" ;;
  key) key_of "$(build)" ;;
  invoke)
    [ $# -eq 1 ] || die "usage: control.sh invoke JSON"
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    aws lambda invoke --function-name "$FUNCTION" --cli-binary-format raw-in-base64-out \
      --payload "$1" "$out" --query 'FunctionError' --output text | grep -v '^None$' >&2 || true
    cat "$out"
    echo
    ;;
  logs)
    minutes="${1:-60}"
    [[ "$minutes" =~ ^[0-9]+$ ]] || die "minutes: a whole number"
    aws logs filter-log-events --log-group-name "/aws/lambda/$FUNCTION" \
      --start-time "$(( ($(date +%s) - minutes * 60) * 1000 ))" --query 'events[].message' --output text |
      tr '\t' '\n' | grep '^{' || true
    ;;
  *) usage ;;
esac
