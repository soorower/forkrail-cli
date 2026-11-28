#!/usr/bin/env bash
# Concurrency kept for the functions that stop a runaway (spend protection, C15): the guard,
# control (records a pause, sets concurrency 0) and the notifier (tells the team). Reserved
# concurrency is a floor and a ceiling at once: the function always has that much, and never
# more. So each gets twice its peak of the last 14 days, and no app is capped: what's reserved
# comes out of the region's shared pool.
#
# AWS keeps at least 100 of a region's concurrency unreserved. An account with a small quota (a
# new account has 10) can reserve nothing, and is told so: ask AWS for a higher quota.
#
# The dashboard isn't reserved for by default: it runs as an app (forkrail-app-forkrail-web), its
# peak can be tens of workers, and a reserve under that would throttle it. When a runaway takes
# the region's concurrency, the routers pause it within a second and the dashboard gets its share
# back as the runaway's workers retire. `--dashboard N` reserves N for it anyway (a pause keeps
# and a resume puts it back).
#
#   deploy/aws/reserve.sh plan [--dashboard N]    the region's quota, each function's peak, what it would reserve
#   deploy/aws/reserve.sh apply [--dashboard N]   the same, then apply it (stack.sh asks for each)
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
KEEP_UNRESERVED=100

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }

cmd="${1:-}"
[[ "$cmd" =~ ^(plan|apply)$ ]] || { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }
shift
dashboard=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dashboard) dashboard="${2:?--dashboard N}"; shift 2 ;;
    *) die "unknown: $1" ;;
  esac
done
[[ "$dashboard" =~ ^[0-9]+$ ]] || die "--dashboard: a whole number"

# Each function's peak concurrency over the last 14 days (a few metrics: under a cent).
peak() {
  { aws cloudwatch get-metric-statistics --namespace AWS/Lambda --metric-name ConcurrentExecutions \
    --dimensions "Name=FunctionName,Value=$1" --start-time "$(date -u -d '-14 days' +%Y-%m-%dT%H:%M:%SZ)" \
    --end-time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --period 86400 --statistics Maximum \
    --query 'max(Datapoints[].Maximum)' --output text 2>/dev/null || true; } | awk '{ v = $1 } END { printf "%d", (v == "None" || v == "") ? 0 : v }'
}

# What a function has reserved now (0: none).
reserved_now() {
  { aws lambda get-function-concurrency --function-name "$1" --query ReservedConcurrentExecutions --output text 2>/dev/null || true; } |
    awk '{ v = $1 } END { printf "%d", (v == "None" || v == "") ? 0 : v }'
}

read -r limit unreserved < <(aws lambda get-account-settings --query 'AccountLimit.[ConcurrentExecutions,UnreservedConcurrentExecutions]' --output text)
# What forkrail's functions hold now goes back into the pool before sizing.
names=(forkrail-guard forkrail-control forkrail-notify forkrail-app-forkrail-web)
held=0
for f in "${names[@]}"; do held=$((held + $(reserved_now "$f"))); done
room=$((unreserved + held - KEEP_UNRESERVED))
log "$AWS_REGION: quota $limit, unreserved $unreserved (forkrail's functions hold $held), room for $((room > 0 ? room : 0))"

# In this order: the guard first (it acts when the routers can't), then control, then the notifier.
declare -A want
want[forkrail-guard]=1
for f in forkrail-control forkrail-notify; do
  p="$(peak "$f")"
  w=$((2 * p)); [ "$w" -ge 2 ] || w=2
  want[$f]=$w
  log "$f: peak $p over 14 days"
done
want[forkrail-app-forkrail-web]=$dashboard
[ "$dashboard" -eq 0 ] && log "forkrail-app-forkrail-web: peak $(peak forkrail-app-forkrail-web) over 14 days; not reserved (--dashboard N to)"

declare -A give
left=$room
for f in "${names[@]}"; do
  w=${want[$f]}
  g=$(( left >= w ? w : (left > 0 ? left : 0) ))
  give[$f]=$g
  left=$((left - g))
done
for f in "${names[@]}"; do
  printf '  %-28s %3d%s\n' "$f" "${give[$f]}" "$([ "${give[$f]}" -lt "${want[$f]}" ] && echo "  (wanted ${want[$f]}: the region's quota is too small)")"
done
[ "$room" -gt 0 ] || log "this region's quota leaves nothing to reserve (AWS keeps $KEEP_UNRESERVED unreserved): ask AWS for a higher Lambda concurrency quota"
[ "$cmd" = apply ] || exit 0

# Through each stack, so the template and the function agree; the dashboard's directly (an app).
"$ROOT/deploy/aws/stack.sh" apply guard "ReservedConcurrency=${give[forkrail-guard]}"
"$ROOT/deploy/aws/stack.sh" apply control "ReservedConcurrency=${give[forkrail-control]}"
"$ROOT/deploy/aws/stack.sh" apply notify "ReservedConcurrency=${give[forkrail-notify]}"
if [ "${give[forkrail-app-forkrail-web]}" -gt 0 ]; then
  aws lambda put-function-concurrency --function-name forkrail-app-forkrail-web --reserved-concurrent-executions "${give[forkrail-app-forkrail-web]}" >/dev/null
  log "forkrail-app-forkrail-web: ${give[forkrail-app-forkrail-web]} reserved"
fi
