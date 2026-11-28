#!/usr/bin/env bash
# The install's own secrets in SSM (home region): the routers' Cloudflare token and challenge key, and
# the ClickHouse passwords. Each is a SecureString under the install key, alias/forkrail-install
# (deploy/install/install-secrets.yaml), which only its readers may decrypt, each its own.
#
#   deploy/aws/install-secrets.sh status   each parameter's key (names and keys only, never values)
#   deploy/aws/install-secrets.sh move     put each one that's under another key (AWS's own SSM key,
#                                          from before) under the install key, value unchanged
#
# Apply the key first (deploy/aws/stack.sh apply install-secrets) and the readers' policies
# (test-env.sh push or roles, analytics.sh role, dashboard.sh role), so they can decrypt it once
# it moves. A value is piped from SSM back into SSM through a file only this user can read, never
# printed or put on a command line; that it came through unchanged is checked by its SHA-256.
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

KEY=alias/forkrail-install
PARAMS=(
  /forkrail/cloudflare-api-token
  /forkrail/router/challenge-key
  /forkrail/clickhouse/ingest
  /forkrail/clickhouse/reader
  /forkrail/clickhouse/buildlog
)

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

# The key id a parameter is under, or "missing".
key_of() {
  aws ssm describe-parameters --parameter-filters "Key=Name,Values=$1" --query 'Parameters[0].KeyId' --output text 2>/dev/null | sed 's/^None$/missing/'
}

# The SHA-256 of a parameter's value, the value itself never leaving the pipe.
value_hash() {
  aws ssm get-parameter --name "$1" --with-decryption --output json | jq -j .Parameter.Value | shasum -a 256 | cut -d' ' -f1
}

key_arn() {
  aws kms describe-key --key-id "$KEY" --query KeyMetadata.Arn --output text
}

status() {
  local arn
  arn="$(key_arn 2>/dev/null || echo none)"
  for p in "${PARAMS[@]}"; do
    local k
    k="$(key_of "$p")"
    case "$k" in
      "$KEY" | "$arn") printf '%-34s %s\n' "$p" "install key" ;;
      missing) printf '%-34s %s\n' "$p" "missing" ;;
      *) printf '%-34s %s\n' "$p" "$k (to move)" ;;
    esac
  done
}

move() {
  local arn
  arn="$(key_arn)" || die "no $KEY yet: deploy/aws/stack.sh apply install-secrets"
  local input
  input="$(mktemp)"
  chmod 600 "$input"
  # Expanded now: the variable is gone by the time the shell exits.
  trap "rm -f -- '$input'" EXIT
  for p in "${PARAMS[@]}"; do
    local k
    k="$(key_of "$p")"
    if [ "$k" = missing ]; then
      log "$p: not there (made by the script that needs it), skipped"
      continue
    fi
    if [ "$k" = "$KEY" ] || [ "$k" = "$arn" ]; then
      log "$p: under the install key already"
      continue
    fi
    local before after
    before="$(value_hash "$p")"
    # Its description is kept: an overwrite without one would drop it.
    local description
    description="$(aws ssm describe-parameters --parameter-filters "Key=Name,Values=$p" --query 'Parameters[0].Description' --output text | sed 's/^None$//')"
    aws ssm get-parameter --name "$p" --with-decryption --output json |
      jq --arg key "$KEY" --arg description "$description" \
        '{Name: .Parameter.Name, Value: .Parameter.Value, Type: "SecureString", KeyId: $key, Overwrite: true}
          + (if $description == "" then {} else {Description: $description} end)' >"$input"
    aws ssm put-parameter --cli-input-json "file://$input" >/dev/null
    : >"$input"
    after="$(value_hash "$p")"
    [ "$before" = "$after" ] || die "$p: its value changed on the way: look at it before anything reads it"
    log "$p: moved under the install key (value unchanged)"
  done
}

case "${1:-}" in
  status) status ;;
  move) move ;;
  *) usage ;;
esac
