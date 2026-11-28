#!/usr/bin/env bash
# The spend guard, forkrail-guard (crates/guard, deploy/install/guard.yaml): every few minutes it
# holds AWS's own numbers against the budgets, and pauses what the routers missed.
#
#   deploy/aws/guard.sh release [PARAM=VALUE...]   build it for Lambda (arm64), upload the zip to
#                                           the install's bucket, and apply the stack with it
#                                           (stack.sh shows the changes and asks first), e.g.
#                                           Regions=us-east-1,us-west-1 Accelerator=<id>
#   deploy/aws/guard.sh plan                build and upload, then show what applying would change
#   deploy/aws/guard.sh upload              build and upload only; prints the zip's key
#   deploy/aws/guard.sh check               run it once without acting ({"dry_run": true}): what
#                                           it reads and what it would do
#   deploy/aws/guard.sh logs [MINUTES]      its lines from the last MINUTES (default 60)
#
# Needs cargo-zigbuild (Homebrew's), like the router and the runtime. FORKRAIL_PREBUILT=1 zips the
# binary target/ has, without compiling (release.sh compiles everything at once first).
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET=aarch64-unknown-linux-gnu
OUT="$ROOT/target/guard"
FUNCTION=forkrail-guard

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

# Build and zip it; prints the zip's path. The zip is the same for the same binary.
build() {
  [ "${FORKRAIL_PREBUILT:-0}" = 1 ] || (cd "$ROOT" && cargo zigbuild --release --target "$TARGET.2.34" -p forkrail-guard >&2)
  [ -x "$ROOT/target/$TARGET/release/forkrail-guard" ] || die "no $ROOT/target/$TARGET/release/forkrail-guard"
  rm -rf "$OUT" && mkdir -p "$OUT"
  cp "$ROOT/target/$TARGET/release/forkrail-guard" "$OUT/bootstrap"
  chmod 755 "$OUT/bootstrap"
  touch -t 198001010000 "$OUT/bootstrap"
  (cd "$OUT" && zip -X -q guard.zip bootstrap)
  echo "$OUT/guard.zip"
}

# The zip's key in the install's bucket: its hash.
key_of() { echo "guard/forkrail-guard-$(shasum -a 256 "$1" | cut -c1-16).zip"; }

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
    "$ROOT/deploy/aws/stack.sh" apply guard "CodeKey=$key" "$@"
    ;;
  plan)
    key="$(upload "$(build)")"
    "$ROOT/deploy/aws/stack.sh" plan guard "CodeKey=$key"
    ;;
  upload) upload "$(build)" ;;
  check)
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    aws lambda invoke --function-name "$FUNCTION" --cli-binary-format raw-in-base64-out \
      --payload '{"dry_run": true}' "$out" --query 'FunctionError' --output text | grep -v '^None$' >&2 || true
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
