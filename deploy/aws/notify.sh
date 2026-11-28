#!/usr/bin/env bash
# The notifier, forkrail-notify (deploy/notify, deploy/install/notify.yaml): the install's events
# to the team's webhooks, its members' notifications and their email.
#
#   deploy/aws/notify.sh test                  run its tests here (no AWS)
#   deploy/aws/notify.sh release [--yes]       install its packages for Lambda (arm64), zip and upload
#                                              it to the install's bucket, and apply the stack with it
#                                              and the dashboard's address (stack.sh shows the changes
#                                              and asks first)
#   deploy/aws/notify.sh plan                  the same, but only show what applying would change
#   deploy/aws/notify.sh ask JSON              invoke it with your own credentials, as the dashboard
#                                              does: '{"kind":"ses_status","domain":"example.com"}'
#   deploy/aws/notify.sh dead                  how many events gave up (the dead-letter queue)
#
# After the first release: the dashboard's table must let it in (`stack.sh apply web`), control may
# send to its queue (`stack.sh apply control`), the routers too (`test-env.sh roles`) and are told
# the queue (`test-env.sh router-env FORKRAIL_EVENTS_QUEUE=<QueueUrl output>`), and the dashboard
# may invoke it and encrypt with its key (`dashboard.sh role`).
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SRC="$ROOT/deploy/notify"
OUT="$ROOT/target/notify"
FUNCTION=forkrail-notify

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

# Its packages for Lambda's Linux on arm64 (the AWS SDK comes with the runtime), then the zip;
# prints its path. The same packages give the same zip (fixed times, sorted names).
build() {
  rm -rf "$OUT" && mkdir -p "$OUT/pkg"
  cp "$SRC/index.mjs" "$SRC/lib.mjs" "$SRC/package.json" "$OUT/pkg/"
  (cd "$OUT/pkg" && npm install --omit=dev --os=linux --cpu=arm64 --no-audit --no-fund --silent >&2)
  rm -f "$OUT/pkg/package-lock.json"
  find "$OUT/pkg" -exec touch -t 198001010000 {} +
  (cd "$OUT/pkg" && find . -type f | LC_ALL=C sort | zip -X -q "$OUT/notify.zip" -@)
  echo "$OUT/notify.zip"
}

account() { aws sts get-caller-identity --query Account --output text; }

# Upload the zip under its hash, unless it's there; prints the key.
upload() {
  local zip="$1" bucket sha key
  bucket="forkrail-artifacts-$(account)"
  sha="$(shasum -a 256 "$zip" | cut -c1-16)"
  key="notify/forkrail-notify-$sha.zip"
  if aws s3api head-object --bucket "$bucket" --key "$key" >/dev/null 2>&1; then
    log "s3://$bucket/$key is there already"
  else
    aws s3 cp "$zip" "s3://$bucket/$key" --only-show-errors
    log "uploaded s3://$bucket/$key ($(wc -c <"$zip" | tr -d ' ') bytes)"
  fi
  echo "$key"
}

# The dashboard's address from the install's settings (`forkrail dashboard URL`), or nothing.
dashboard_url() {
  aws s3 cp "s3://forkrail-artifacts-$(account)/state/settings.json" - 2>/dev/null | jq -r '.dashboard // empty' | sed 's:/*$::'
}

cmd="${1:-}"
[ -n "$cmd" ] && shift
case "$cmd" in
  # Build the zip only and print its path (deploy/aws/bundle.sh, for `forkrail install`).
  zip) build ;;
  test)
    (cd "$SRC" && node --test)
    ;;
  release|plan)
    (cd "$SRC" && node --test >&2) || die "the notifier's tests failed: nothing released"
    key="$(upload "$(build)")"
    url="$(dashboard_url)"
    [ -n "$url" ] || log "the install has no dashboard address: notifications link only where an event carries the link"
    action=apply
    [ "$cmd" = plan ] && action=plan
    "$ROOT/deploy/aws/stack.sh" "$action" notify "CodeKey=$key" "DashboardUrl=$url" "$@"
    ;;
  ask)
    [ $# -eq 1 ] || usage
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    aws lambda invoke --function-name "$FUNCTION" --cli-binary-format raw-in-base64-out --payload "$1" "$out" >/dev/null
    jq . "$out"
    ;;
  dead)
    url="$(aws sqs get-queue-url --queue-name forkrail-events-dead --query QueueUrl --output text)"
    aws sqs get-queue-attributes --queue-url "$url" --attribute-names ApproximateNumberOfMessages --query Attributes.ApproximateNumberOfMessages --output text
    ;;
  *) usage ;;
esac
