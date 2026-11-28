#!/usr/bin/env bash
# A release of forkrail's own parts from this checkout's commit, in the order that keeps old and new
# working together: the routers (each extra region, then the home region, one at a time, blue/green),
# then control and the builder together, then the dashboard (web/'s main: a push is a production
# deploy). Every step is checked before the next one starts: each host answers through each router
# and accelerator address as it did before the release, a router runs the binary just built, control
# answers, and test builds go through the new builder.
#
#   deploy/aws/release.sh plan                   compile, then say which parts changed and what each
#                                                step would do; changes nothing
#   deploy/aws/release.sh run [PART...] [--yes]  release; asks once first, unless --yes
#       PART: routers, backend (control and the builder, always together), web; without one, every
#             part that changed. A router change outside the binary (test-env.sh's unit, roles,
#             Vector) isn't seen: name `routers` for it.
#       --web-preview   stop the dashboard at its preview, to look at, without pushing web/
#       --no-smoke      no test builds (nodeapp's and pyapp's previews) after the backend
#       --allow-dirty   release uncommitted or unpushed forkrail changes (rollback then has no commit)
#       --allow-alarms  release while a forkrail alarm is firing
#   deploy/aws/release.sh check [--save]         each host's status through each router and accelerator
#                                                address, against the last saved; --save keeps this one
#   deploy/aws/release.sh routers-back SHA16     the router binary releases/router/forkrail-router-SHA16
#                                                (the one a release replaced) back on every router
#
# Hosts are every custom domain and every project's production host in state/routes.json. Each run
# keeps its logs, timings and summary in target/releases/<UTC time>/, and a record of what it
# released and replaced in s3://<bucket>/releases/<UTC time>.json, which the next run reads.
#
# What runs at once: one compile for the router, control and the builder (one set of shared
# dependencies), then the runtime alone (the one in every function, without the others' features);
# while the routers restart, this machine's CLI compiles and the builder image and control's zip are
# made and uploaded; control's and the builder's stacks apply together; test builds and the
# dashboard's preview build together. Needs cargo-zigbuild, crane, jq, and an AWS session with 25
# minutes left (`aws login --profile forkrail`).
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
AWS_DIR="$ROOT/deploy/aws"
STATE_REGION=us-east-1
GA_REGION=us-west-2
GA_NAME=forkrail
TARGET=aarch64-unknown-linux-gnu
BIN="$ROOT/target/$TARGET/release"
CLI="$ROOT/target/release/forkrail"
SAVED_HOSTS="$ROOT/target/releases/hosts.txt"
DASHBOARD=forkrail-web
SMOKE="nodeapp:test-apps/node-http pyapp:test-apps/python-fastapi"
SESSION_MIN=25

log() { echo "==> $*" >&2; }
warn() { echo "!!  $*" >&2; }
die() { echo "$*" >&2; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

ACCOUNT=""
BUCKET=""
aws_init() {
  ACCOUNT="$(aws sts get-caller-identity --query Account --output text 2>/dev/null)" ||
    die "no AWS session for profile $AWS_PROFILE: aws login --profile $AWS_PROFILE"
  BUCKET="forkrail-artifacts-$ACCOUNT"
}

# Minutes the session has left, or nothing when the profile doesn't say (long-lived keys). Only the
# expiry is read; the keys go no further than jq. An `aws login` session (`login_session` in the
# profile) says nothing either: the CLI hands out keys that last 15 minutes and refreshes them
# itself for as long as the login holds, so their expiry is always under 15 minutes.
session_minutes() {
  local expiry
  [ -z "$(aws configure get login_session --profile "$AWS_PROFILE" 2>/dev/null)" ] || return 0
  expiry="$(aws configure export-credentials --profile "$AWS_PROFILE" --format process 2>/dev/null | jq -r '.Expiration // empty')" || true
  [ -n "$expiry" ] || return 0
  local plain="${expiry%%+*}"
  plain="${plain%Z}"
  echo $(( ($(date -j -u -f '%Y-%m-%dT%H:%M:%S' "$plain" +%s 2>/dev/null || date -u -d "$expiry" +%s) - $(date -u +%s)) / 60 ))
}

# --- What runs now ---------------------------------------------------------------------------------

BASE=""
ROUTES=""
load_state() {
  ROUTES="$(mktemp)"
  aws s3 --region "$STATE_REGION" cp --quiet "s3://$BUCKET/state/routes.json" "$ROUTES"
  BASE="$(aws s3 --region "$STATE_REGION" cp --quiet "s3://$BUCKET/state/settings.json" - | jq -r '.base_domain // empty')"
  [ -n "$BASE" ] || die "no base domain in state/settings.json"
}

# Regions with a router: the others first, the home region (which issues certificates) last.
router_regions() {
  aws s3 --region "$STATE_REGION" cp --quiet "s3://$BUCKET/state/settings.json" - |
    jq -r --arg home "$STATE_REGION" '[(.regions // [])[] | select(. != $home)] + [$home] | .[]'
}

eip_of() {
  aws ec2 describe-addresses --region "$1" \
    --filters Name=tag:Name,Values=forkrail-router-eip Name=tag:project,Values=forkrail \
    --query 'Addresses[0].PublicIp' --output text
}

accelerator_ips() {
  aws globalaccelerator --region "$GA_REGION" list-accelerators \
    --query "Accelerators[?Name=='$GA_NAME'].IpSets[].IpAddresses[]" --output text 2>/dev/null | tr '\t' '\n' || true
}

# Every custom domain and every project's production host.
hosts_of() {
  jq -r --arg base "$BASE" '. as $r | ($r.domains // {} | keys[]),
    ($r.hosts // {} | to_entries[] | select((($r.deployments[.value].project // "") + "." + $base) == .key) | .key)' "$ROUTES" | sort -u
}

# probe ADDRESS...: "ADDRESS HOST STATUS" for every host through every address, and each address's
# health path on port 80, 32 at a time.
probe() {
  local addr host
  {
    for addr in "$@"; do
      while read -r host; do echo "$host $addr"; done < <(hosts_of)
      echo "health $addr"
    done
  } | xargs -P 32 -L 1 sh -c '
    if [ "$0" = health ]; then
      code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 "http://$1/_forkrail/health")
    else
      code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 20 --resolve "$0:443:$1" "https://$0/")
    fi
    printf "%s %s %s\n" "$1" "$0" "$code"' | sort
}

# compare BEFORE NOW: lines whose status changed ("ADDRESS HOST was -> now"); hosts new or gone
# since BEFORE aren't changes.
compare() {
  awk 'NR == FNR { was[$1 " " $2] = $3; next } ($1 " " $2) in was && was[$1 " " $2] != $3 { print $1, $2, was[$1 " " $2], "->", $3 }' "$1" "$2"
}

# check_hosts BEFORE OUT ADDRESS...: every host through ADDRESS... answers as in BEFORE. A host that
# differs is asked again after 5 s before it counts.
check_hosts() {
  local before="$1" out="$2" diffs again
  shift 2
  probe "$@" > "$out"
  diffs="$(compare "$before" "$out")"
  if [ -n "$diffs" ]; then
    sleep 5
    again="$(mktemp)"
    while read -r addr host _; do
      if [ "$host" = health ]; then
        printf '%s health %s\n' "$addr" "$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "http://$addr/_forkrail/health")"
      else
        printf '%s %s %s\n' "$addr" "$host" "$(curl -s -o /dev/null -w '%{http_code}' --max-time 20 --resolve "$host:443:$addr" "https://$host/")"
      fi
    done <<<"$diffs" > "$again"
    diffs="$(compare "$before" "$again")"
    # OUT keeps the second answers, so a saved list never holds a blip.
    awk 'NR == FNR { now[$1 " " $2] = $3; next } { k = $1 " " $2; print $1, $2, ((k in now) ? now[k] : $3) }' "$again" "$out" > "$out.new"
    mv "$out.new" "$out"
    rm -f "$again"
  fi
  local hosts addrs
  hosts="$(hosts_of | wc -l | tr -d ' ')"
  addrs="$#"
  if [ -n "$diffs" ]; then
    warn "hosts answering differently from before the release:"
    sed 's/^/    /' <<<"$diffs" >&2
    return 1
  fi
  log "$hosts hosts through $addrs addresses answer as before ($(awk '{print $3}' "$out" | sort | uniq -c | awk '{printf "%s%s×%s", (NR>1?", ":""), $1, $2}'))"
}

# router_report REGION SINCE: the running router's color and binary SHA-256 ("color sha"), then its
# warnings and errors since SINCE (a time journalctl reads), scanners' bad SNI aside.
router_report() {
  AWS_REGION="$1" "$AWS_DIR/test-env.sh" run "for c in blue green; do
  if systemctl is-active --quiet forkrail-router@\$c; then
    echo \"running \$c \$(sha256sum /proc/\$(systemctl show -p MainPID --value forkrail-router@\$c)/exe | cut -d' ' -f1)\"
  fi
done
journalctl -u 'forkrail-router@*' --since '$2' --no-pager -o cat | grep -E 'WARN|ERROR' | grep -v 'Illegal SNI' | tail -20 | sed 's/^/journal: /'" 2>&1
}

# (No awk `exit` in a pipe: the writer's SIGPIPE would fail the pipeline under pipefail.)
running_sha() { router_report "$1" "1 minute ago" | awk '$1 == "running" && !sha { sha = $3 } END { print sha }'; }

stack_param() {
  aws cloudformation describe-stacks --region "$STATE_REGION" --stack-name "forkrail-$1" \
    --query "Stacks[0].Parameters[?ParameterKey=='$2'].ParameterValue | [0]" --output text
}

# A label of the image the builder runs now (crane, with ECR's password in a folder of its own).
image_label() {
  local uri="$1" label="$2" registry dir out
  registry="${uri%%/*}"
  dir="$(mktemp -d)"
  out="$(DOCKER_CONFIG="$dir" sh -c 'aws ecr get-login-password --region "$0" | crane auth login "$1" -u AWS --password-stdin >/dev/null 2>&1 && crane config "$2"' \
    "$STATE_REGION" "$registry" "$uri" 2>/dev/null | jq -r --arg l "$label" '.config.Labels[$l] // empty')" || true
  rm -rf "$dir"
  echo "$out"
}

# The CPU architecture an image is for, from its config (crane, as `image_label`).
image_arch() {
  local uri="$1" registry dir out
  registry="${uri%%/*}"
  dir="$(mktemp -d)"
  out="$(DOCKER_CONFIG="$dir" sh -c 'aws ecr get-login-password --region "$0" | crane auth login "$1" -u AWS --password-stdin >/dev/null 2>&1 && crane config "$2"' \
    "$STATE_REGION" "$registry" "$uri" 2>/dev/null | jq -r '.architecture // empty')" || true
  rm -rf "$dir"
  echo "$out"
}

dashboard_production() { "$CLI" --aws ls "$DASHBOARD" 2>/dev/null | awk '/\(production\)$/ && !id { id = $1 } END { print id }'; }

# --- Steps -----------------------------------------------------------------------------------------

RUN_DIR=""
STEP=""
STEP_START=0
RUN_START=$(date +%s)
RUN_SINCE="$(date -u '+%Y-%m-%d %H:%M:%S') UTC"
DONE_PARTS=""

step() {
  [ -z "$STEP" ] || step_done
  STEP="$1"
  STEP_START=$(date +%s)
  log "[$(date -u +%H:%M:%S)] $STEP"
}
step_done() {
  [ -n "$STEP" ] || return 0
  printf '%-40s %4ss\n' "$STEP" "$(( $(date +%s) - STEP_START ))" >> "$RUN_DIR/steps.txt"
  STEP=""
}

# bg NAME COMMAND...: COMMAND in the background, its output in RUN_DIR/NAME.log; join NAME waits
# for it and stops the release if it failed.
bg() {
  local name="$1"
  shift
  ( "$@" ) > "$RUN_DIR/$name.log" 2>&1 &
  printf -v "PID_${name//-/_}" %s $!
  echo "$(date +%s)" > "$RUN_DIR/$name.start"
}
join() { finished "$1" || die "$1 failed (log: $RUN_DIR/$1.log)"; }
# finished NAME: waits for it; its status, with the log's end shown when it failed.
finished() {
  local name="$1" var="PID_${1//-/_}" took status=0
  [ -n "${!var:-}" ] || die "$name wasn't started"
  wait "${!var}" || status=$?
  printf -v "$var" %s ""
  took=$(( $(date +%s) - $(cat "$RUN_DIR/$name.start") ))
  printf '%-40s %4ss (alongside)%s\n' "$name" "$took" "$([ "$status" = 0 ] || echo ', FAILED')" >> "$RUN_DIR/steps.txt"
  [ "$status" = 0 ] || tail -25 "$RUN_DIR/$name.log" >&2
  return "$status"
}

compile() {
  step "compile: router, control, builder (arm64)"
  (cd "$ROOT" && cargo zigbuild --release --target "$TARGET.2.34" -p forkrail-router -p forkrail-control -p forkrail-builder)
  step "compile: runtime (arm64)"
  (cd "$ROOT" && cargo zigbuild --release --target "$TARGET.2.34" -p forkrail-runtime)
  # Everything released runs on arm64 (Lambda, the routers' t4g): a binary for this machine's own
  # architecture (a release from an x86 server) would start nowhere.
  local binary
  for binary in forkrail-router forkrail-control forkrail-builder forkrail-runtime; do
    file -b "$BIN/$binary" | grep -q 'ARM aarch64' || die "$BIN/$binary isn't an arm64 binary: $(file -b "$BIN/$binary")"
  done
  step_done
}

NEW_ROUTER=""
NEW_CONTROL_KEY=""
NEW_BUILDER=""
NEW_RUNTIME=""
OLD_ROUTER=""   # "region=sha ..." as running before
OLD_CONTROL_KEY=""
OLD_IMAGE=""
OLD_WEB=""
CHANGED=""

# What changed, from the binaries just compiled against what runs.
detect() {
  NEW_ROUTER="$(shasum -a 256 "$BIN/forkrail-router" | cut -d' ' -f1)"
  NEW_BUILDER="$(shasum -a 256 "$BIN/forkrail-builder" | cut -d' ' -f1)"
  NEW_RUNTIME="$(shasum -a 256 "$BIN/forkrail-runtime" | cut -d' ' -f1)"
  NEW_CONTROL_KEY="$(FORKRAIL_PREBUILT=1 "$AWS_DIR/control.sh" key)"
  local region sha routers_changed=0 backend_changed=0 old_builder old_runtime
  for region in $(router_regions); do
    sha="$(running_sha "$region")"
    OLD_ROUTER="$OLD_ROUTER $region=${sha:-unknown}"
    if [ "$sha" = "$NEW_ROUTER" ]; then log "router $region: runs this binary already (${sha:0:16})"
    else log "router $region: ${sha:0:16} -> ${NEW_ROUTER:0:16}"; routers_changed=1; fi
  done
  OLD_ROUTER="${OLD_ROUTER# }"
  OLD_CONTROL_KEY="$(stack_param control CodeKey)"
  if [ "$OLD_CONTROL_KEY" = "$NEW_CONTROL_KEY" ]; then log "control: runs this build already ($NEW_CONTROL_KEY)"
  else log "control: $OLD_CONTROL_KEY -> $NEW_CONTROL_KEY"; backend_changed=1; fi
  OLD_IMAGE="$(stack_param builder ImageUri)"
  old_builder="$(image_label "$OLD_IMAGE" dev.forkrail.builder-sha256)"
  old_runtime="$(image_label "$OLD_IMAGE" dev.forkrail.runtime-sha256)"
  if [ "$old_builder" = "$NEW_BUILDER" ] && [ "$old_runtime" = "$NEW_RUNTIME" ]; then
    log "builder: its image has these binaries already"
  else
    log "builder: ${old_builder:-an image without binary labels} -> builder ${NEW_BUILDER:0:16}, runtime ${NEW_RUNTIME:0:16}"
    backend_changed=1
  fi
  [ -x "$CLI" ] && OLD_WEB="$(dashboard_production)"
  local web_changed=0 head remote
  if [ -d "$ROOT/web/.git" ]; then
    git -C "$ROOT/web" fetch -q origin main
    head="$(git -C "$ROOT/web" rev-parse HEAD)"
    remote="$(git -C "$ROOT/web" rev-parse origin/main)"
    if [ "$head" = "$remote" ]; then log "web: main is pushed already (${head:0:7}; production ${OLD_WEB:-unknown})"
    else log "web: ${remote:0:7} -> ${head:0:7} ($(git -C "$ROOT/web" rev-list --count "$remote..$head") commits)"; web_changed=1; fi
  fi
  CHANGED=""
  [ "$routers_changed" = 0 ] || CHANGED="$CHANGED routers"
  [ "$backend_changed" = 0 ] || CHANGED="$CHANGED backend"
  [ "$web_changed" = 0 ] || CHANGED="$CHANGED web"
  CHANGED="${CHANGED# }"
}

ADDRESSES=""
BEFORE=""
preflight() {
  step "preflight"
  local missing tool
  for tool in jq crane cargo-zigbuild curl; do command -v "$tool" >/dev/null || missing="${missing:-} $tool"; done
  [ -z "${missing:-}" ] || die "missing:$missing"
  local left
  left="$(session_minutes)"
  if [ -n "$left" ] && [ "$left" -lt "$SESSION_MIN" ]; then
    die "the AWS session has $left minutes left; a release takes up to $SESSION_MIN: aws login --profile $AWS_PROFILE first"
  fi
  git -C "$ROOT" fetch -q origin
  if [ "$ALLOW_DIRTY" = 0 ]; then
    [ -z "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ] ||
      die "forkrail has uncommitted changes: commit them (or --allow-dirty)"
    git -C "$ROOT" merge-base --is-ancestor HEAD origin/main ||
      die "forkrail's $(git -C "$ROOT" rev-parse --short HEAD) isn't on origin/main: push it first (or --allow-dirty)"
  fi
  local alarms
  alarms="$(aws cloudwatch describe-alarms --region "$STATE_REGION" --state-value ALARM --alarm-name-prefix forkrail \
    --query 'MetricAlarms[].AlarmName' --output text)"
  if [ -n "$alarms" ] && [ "$alarms" != None ]; then
    [ "$ALLOW_ALARMS" = 1 ] || die "alarms firing: $alarms (look first, or --allow-alarms)"
    warn "alarms firing: $alarms"
  fi
  load_state
  local region
  for region in $(router_regions); do ADDRESSES="$ADDRESSES $(eip_of "$region")"; done
  ADDRESSES="${ADDRESSES# } $(accelerator_ips | tr '\n' ' ')"
  BEFORE="$RUN_DIR/hosts-before.txt"
  probe $ADDRESSES > "$BEFORE"
  log "before: $(hosts_of | wc -l | tr -d ' ') hosts through ${ADDRESSES% } ($(awk '{print $3}' "$BEFORE" | sort | uniq -c | awk '{printf "%s%s×%s", (NR>1?", ":""), $1, $2}'))"
  step_done
}

web_ready() {
  [ -d "$ROOT/web/.git" ] || die "no web/ checkout"
  [ -z "$(git -C "$ROOT/web" status --porcelain)" ] ||
    die "web/ has uncommitted or untracked files: the preview would build them (commit, stash, or build from a clean copy)"
  [ "$(git -C "$ROOT/web" rev-parse --abbrev-ref HEAD)" = main ] || die "web/ isn't on main"
  git -C "$ROOT/web" merge-base --is-ancestor origin/main HEAD || die "web/'s main is behind or apart from origin/main: pull first"
}

routers() {
  local region first=1 addr since sha report
  # Counted from the first push, so a stop in the second region still prints the first one's way back.
  DONE_PARTS="$DONE_PARTS routers"
  for region in $(router_regions); do
    step "router $region"
    since="$(date -u '+%Y-%m-%d %H:%M:%S')"
    FORKRAIL_PREBUILT=1 FORKRAIL_TEST_FUNCTIONS=0 FORKRAIL_ROLES=$first AWS_REGION="$region" "$AWS_DIR/test-env.sh" push
    first=0
    step "check router $region"
    report="$(router_report "$region" "$since UTC")"
    sha="$(awk '$1 == "running" && !sha { sha = $3 } END { print sha }' <<<"$report")"
    [ "$sha" = "$NEW_ROUTER" ] || die "the router in $region runs ${sha:-nothing} (${sha:0:16}), not the new ${NEW_ROUTER:0:16}"
    grep '^journal: ' <<<"$report" | sed 's/^journal: /    /' >&2 || true
    addr="$(eip_of "$region")"
    check_hosts "$BEFORE" "$RUN_DIR/hosts-$region.txt" "$addr" || die "stopped after the router in $region (rollback below)"
  done
  step_done
}

start_backend_artifacts() {
  bg builder-image env FORKRAIL_PREBUILT=1 "$AWS_DIR/builder.sh" image
  bg control-zip env FORKRAIL_PREBUILT=1 "$AWS_DIR/control.sh" upload
}

backend() {
  step "backend: wait for the image and zip"
  join builder-image
  join control-zip
  local image key
  image="$(tail -1 "$RUN_DIR/builder-image.log")"
  key="$(tail -1 "$RUN_DIR/control-zip.log")"
  [[ "$image" == *"@sha256:"* ]] || die "builder.sh printed no image: $RUN_DIR/builder-image.log"
  [[ "$key" == control/*.zip ]] || die "control.sh printed no key: $RUN_DIR/control-zip.log"
  NEW_IMAGE="$image"
  step "backend: the new builder image is for the running one's architecture"
  local arch old_arch
  arch="$(image_arch "$image")"
  old_arch="$(image_arch "$OLD_IMAGE")"
  [ "$arch" = arm64 ] && [ "$arch" = "$old_arch" ] ||
    die "the new builder image is for ${arch:-an unknown architecture}, the running one for ${old_arch:-an unknown one}: nothing applied"
  log "builder image: $arch, as the running one"
  step "backend: apply control and builder together"
  # Both are waited for whatever happens to the other, and both count as released for the
  # rollback lines: one applied without the other is the state to undo.
  DONE_PARTS="$DONE_PARTS backend"
  bg apply-control "$AWS_DIR/stack.sh" apply control "CodeKey=$key" --yes
  bg apply-builder "$AWS_DIR/stack.sh" apply builder "ImageUri=$image" --yes
  local failed=""
  finished apply-control || failed="control"
  finished apply-builder || failed="${failed:+$failed and }builder"
  [ -z "$failed" ] || die "applying $failed failed: see the rollback lines for the one that went through"
  step "backend: control answers"
  local pong
  pong="$("$AWS_DIR/control.sh" invoke '{"action": "ping"}' 2>&1)"
  jq -e '.ok == true' <<<"$pong" >/dev/null 2>&1 || die "control's ping: $pong"
  step_done
}

# smoke PROJECT DIR: a preview of DIR through the new builder, which answers 200.
smoke() {
  local project="$1" dir="$2" url code
  url="$(cd "$ROOT/$dir" && "$CLI" --aws deploy --project "$project")"
  url="${url%%$'\n'*}"
  [[ "$url" == https://* ]] || { echo "no URL from the deploy of $dir" >&2; return 1; }
  code="$("$CLI" --aws curl "$url" -- -s -o /dev/null -w '%{http_code}' 2>/dev/null | tail -1)"
  echo "$project $url $code"
  [ "$code" = 200 ]
}

# The dashboard's preview from web/ as committed; prints its URL.
web_preview() {
  local url code
  url="$(FORKRAIL_PREBUILT=1 "$AWS_DIR/dashboard.sh" deploy)"
  url="${url%%$'\n'*}"
  [[ "$url" == https://* ]] || { echo "no URL from the dashboard's deploy" >&2; return 1; }
  code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 "$url/sign-in")"
  echo "$url $code"
  [ "$code" = 200 ]
}

web_push() {
  step "web: push main (production deploy)"
  local head before row id status deadline
  head="$(git -C "$ROOT/web" rev-parse --short HEAD)"
  before="$(dashboard_production)"
  git -C "$ROOT/web" push -q origin main
  # Production changes once the push deploy is ready, whether or not this run sees it.
  DONE_PARTS="$DONE_PARTS web"
  step "web: wait for the push deploy"
  deadline=$(( $(date +%s) + 600 ))
  while :; do
    row="$("$CLI" --aws ls "$DASHBOARD" 2>/dev/null | awk 'NR == 2')"
    id="$(awk '{print $1}' <<<"$row")"
    status="$(awk '{print $3}' <<<"$row")"
    if [ "$id" != "$before" ] && [ -n "$id" ]; then
      case "$status" in error | canceled) die "the dashboard's push deploy $id is $status: $CLI --aws inspect $id" ;; esac
      [ "$status" = ready ] && [[ "$row" == *"(production)" ]] && break
    fi
    [ "$(date +%s)" -lt "$deadline" ] || die "no production deploy of web $head after 10 minutes"
    sleep 5
  done
  NEW_WEB="$id"
  step "web: production answers"
  local code dashboard
  dashboard="$(aws s3 --region "$STATE_REGION" cp --quiet "s3://$BUCKET/state/settings.json" - | jq -r '.dashboard')"
  code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 "$dashboard/sign-in")"
  [ "$code" = 200 ] || die "$dashboard/sign-in answers $code"
  log "production dashboard: $NEW_WEB (web $head), /sign-in 200"
  step_done
}

NEW_IMAGE=""
NEW_WEB=""
RESULT="failed"

rollback_lines() {
  local region sha
  case " $DONE_PARTS " in *" routers "*)
    for pair in $OLD_ROUTER; do
      sha="${pair#*=}"
      echo "- router ${pair%%=*}: deploy/aws/release.sh routers-back ${sha:0:16}"
    done ;;
  esac
  case " $DONE_PARTS " in *" backend "*)
    echo "- control: deploy/aws/stack.sh apply control CodeKey=$OLD_CONTROL_KEY"
    echo "- builder: deploy/aws/stack.sh apply builder ImageUri=$OLD_IMAGE" ;;
  esac
  case " $DONE_PARTS " in *" web "*) echo "- dashboard: target/release/forkrail --aws rollback $DASHBOARD (to $OLD_WEB)" ;; esac
}

summary() {
  local status=$? failed_step="$STEP"
  step_done 2>/dev/null || true
  # Background work left behind by a failure.
  jobs -p | xargs kill 2>/dev/null || true
  [ -n "$RUN_DIR" ] || exit "$status"
  if [ "$status" != 0 ]; then RESULT="failed at ${failed_step:-a step before the first}"
  elif [ -z "$PARTS" ]; then RESULT="nothing to release"
  else RESULT="released"; fi
  local took=$(( $(date +%s) - RUN_START )) commit web_commit
  commit="$(git -C "$ROOT" rev-parse --short HEAD)"
  web_commit="$(git -C "$ROOT/web" rev-parse --short HEAD 2>/dev/null || true)"
  {
    echo "# Release $(basename "$RUN_DIR"): $RESULT"
    echo
    echo "forkrail $commit, web $web_commit; parts: ${PARTS:-none}; done: ${DONE_PARTS# }; $((took / 60))m$((took % 60))s"
    echo
    [ -z "$NEW_ROUTER" ] || echo "- routers: ${NEW_ROUTER:0:16} (before: $OLD_ROUTER)"
    [ -z "$NEW_IMAGE" ] || echo "- builder: $NEW_IMAGE"
    [ -z "$NEW_CONTROL_KEY" ] || echo "- control: $NEW_CONTROL_KEY"
    [ -z "$NEW_WEB" ] || echo "- dashboard: $NEW_WEB"
    if [ -n "$DONE_PARTS" ]; then echo; echo "Back:"; rollback_lines; fi
    echo
    echo "Steps:"
    sed 's/^/    /' "$RUN_DIR/steps.txt" 2>/dev/null || true
  } > "$RUN_DIR/summary.md"
  cat "$RUN_DIR/summary.md" >&2
  if [ -n "$DONE_PARTS" ] && [ -n "$BUCKET" ]; then
    jq -n --arg result "$RESULT" --arg commit "$(git -C "$ROOT" rev-parse HEAD)" --arg web "$web_commit" \
      --arg parts "${DONE_PARTS# }" --arg router "$NEW_ROUTER" --arg old_router "$OLD_ROUTER" \
      --arg control "$NEW_CONTROL_KEY" --arg old_control "$OLD_CONTROL_KEY" --arg image "$NEW_IMAGE" \
      --arg old_image "$OLD_IMAGE" --arg dashboard "$NEW_WEB" --arg old_dashboard "$OLD_WEB" --argjson seconds "$took" \
      '{result: $result, commit: $commit, web: $web, parts: ($parts | split(" ")), seconds: $seconds,
        now: {router: $router, control: $control, image: $image, dashboard: $dashboard},
        before: {routers: $old_router, control: $old_control, image: $old_image, dashboard: $old_dashboard}}' |
      aws s3 --region "$STATE_REGION" cp --quiet - "s3://$BUCKET/releases/$(basename "$RUN_DIR").json" 2>/dev/null || true
  fi
  exit "$status"
}

# --- Commands --------------------------------------------------------------------------------------

YES=0
ALLOW_DIRTY=0
ALLOW_ALARMS=0
WEB_PREVIEW=0
SMOKE_ON=1
SAVE=0
PARTS=""

cmd="${1:-}"
[ $# -gt 0 ] && shift
args=()
for arg in "$@"; do
  case "$arg" in
    --yes) YES=1 ;;
    --allow-dirty) ALLOW_DIRTY=1 ;;
    --allow-alarms) ALLOW_ALARMS=1 ;;
    --web-preview) WEB_PREVIEW=1 ;;
    --no-smoke) SMOKE_ON=0 ;;
    --save) SAVE=1 ;;
    --*) die "unknown option $arg" ;;
    *) args+=("$arg") ;;
  esac
done
set -- ${args[@]+"${args[@]}"}

case "$cmd" in
  check)
    aws_init
    load_state
    addresses=""
    for region in $(router_regions); do addresses="$addresses $(eip_of "$region")"; done
    addresses="$addresses $(accelerator_ips | tr '\n' ' ')"
    now="$(mktemp)"
    probe $addresses > "$now"
    log "$(hosts_of | wc -l | tr -d ' ') hosts through$addresses: $(awk '{print $3}' "$now" | sort | uniq -c | awk '{printf "%s%s×%s", (NR>1?", ":""), $1, $2}')"
    if [ -f "$SAVED_HOSTS" ]; then
      diffs="$(compare "$SAVED_HOSTS" "$now")"
      if [ -n "$diffs" ]; then warn "changed since $(date -r "$SAVED_HOSTS" -u '+%Y-%m-%d %H:%M UTC'):"; sed 's/^/    /' <<<"$diffs" >&2
      else log "the same as $(date -r "$SAVED_HOSTS" -u '+%Y-%m-%d %H:%M UTC')"; fi
    fi
    if [ "$SAVE" = 1 ]; then mkdir -p "$(dirname "$SAVED_HOSTS")"; cp "$now" "$SAVED_HOSTS"; log "saved as $SAVED_HOSTS"; fi
    rm -f "$now"
    ;;

  plan)
    aws_init
    RUN_DIR="$ROOT/target/releases/plan-$(date -u +%Y%m%dT%H%M%SZ)"
    mkdir -p "$RUN_DIR"
    load_state
    compile
    detect
    echo >&2
    log "changed: ${CHANGED:-nothing}"
    log "deploy/aws/release.sh run would do, in order:"
    case " $CHANGED " in *" routers "*) echo "    routers: $(router_regions | tr '\n' ' ')one at a time, blue/green, each checked" >&2 ;; esac
    case " $CHANGED " in *" backend "*) echo "    backend: control and the builder applied together, then test builds of nodeapp and pyapp" >&2 ;; esac
    case " $CHANGED " in *" web "*) echo "    web: the dashboard's preview, then web/'s main pushed (production)" >&2 ;; esac
    log "nothing was changed"
    ;;

  run)
    aws_init
    for part in "$@"; do
      case "$part" in routers | backend | web) ;; *) die "no part $part: routers, backend or web" ;; esac
    done
    RUN_DIR="$ROOT/target/releases/$(date -u +%Y%m%dT%H%M%SZ)"
    mkdir -p "$RUN_DIR"
    trap summary EXIT
    preflight
    compile
    step "what changed"
    detect
    PARTS="${*:-$CHANGED}"
    [ -n "$PARTS" ] || { log "nothing changed: nothing to release"; DONE_PARTS=""; exit 0; }
    case " $PARTS " in *" web "*) web_ready ;; esac
    step_done
    if [ "$YES" = 0 ]; then
      { true </dev/tty; } 2>/dev/null || die "no terminal to ask on: pass --yes"
      read -r -p "Release $PARTS from forkrail $(git -C "$ROOT" rev-parse --short HEAD) to production? [y/N] " answer </dev/tty
      [[ "$answer" =~ ^[Yy]$ ]] || die "nothing changed"
    fi
    # While the routers restart: this machine's CLI (the smoke builds and the dashboard use it), and
    # the backend's image and zip.
    bg cli sh -c "cd '$ROOT' && cargo build --release -p forkrail-cli"
    case " $PARTS " in *" backend "*) start_backend_artifacts ;; esac
    case " $PARTS " in *" routers "*) routers ;; esac
    case " $PARTS " in *" backend "*) backend ;; esac
    step "CLI for this machine"
    join cli
    # Test builds through the builder and the dashboard's preview, at once.
    step "test builds and the dashboard's preview"
    if [ "$SMOKE_ON" = 1 ] && [[ " $PARTS " == *" backend "* ]]; then
      for pair in $SMOKE; do bg "smoke-${pair%%:*}" smoke "${pair%%:*}" "${pair#*:}"; done
    fi
    case " $PARTS " in *" web "*) bg web-preview web_preview ;; esac
    if [ "$SMOKE_ON" = 1 ] && [[ " $PARTS " == *" backend "* ]]; then
      for pair in $SMOKE; do join "smoke-${pair%%:*}"; log "test build: $(tail -1 "$RUN_DIR/smoke-${pair%%:*}.log")"; done
    fi
    case " $PARTS " in *" web "*)
      join web-preview
      log "dashboard preview: $(tail -1 "$RUN_DIR/web-preview.log")"
      if [ "$WEB_PREVIEW" = 1 ]; then
        log "stopped at the dashboard's preview (--web-preview): look at it, then deploy/aws/release.sh run web"
      else
        web_push
      fi ;;
    esac
    step "final check"
    check_hosts "$BEFORE" "$RUN_DIR/hosts-after.txt" $ADDRESSES || die "hosts differ after the release (rollback below)"
    for region in $(router_regions); do
      warnings="$(router_report "$region" "$RUN_SINCE" | grep '^journal: ' || true)"
      if [ -n "$warnings" ]; then warn "router $region logged:"; sed 's/^journal: /    /' <<<"$warnings" >&2
      else log "router $region: no warnings since the release began"; fi
    done
    alarms="$(aws cloudwatch describe-alarms --region "$STATE_REGION" --state-value ALARM --alarm-name-prefix forkrail \
      --query 'MetricAlarms[].AlarmName' --output text)"
    [ -z "$alarms" ] || [ "$alarms" = None ] || warn "alarms firing: $alarms"
    cp "$RUN_DIR/hosts-after.txt" "$SAVED_HOSTS"
    step_done
    ;;

  routers-back)
    [ $# -eq 1 ] && [[ "$1" =~ ^[0-9a-f]{16}$ ]] || die "usage: release.sh routers-back SHA16 (the first 16 of its SHA-256)"
    aws_init
    RUN_DIR="$ROOT/target/releases/$(date -u +%Y%m%dT%H%M%SZ)-routers-back"
    mkdir -p "$RUN_DIR"
    binary="$RUN_DIR/forkrail-router"
    aws s3 --region "$STATE_REGION" cp --quiet "s3://$BUCKET/releases/router/forkrail-router-$1" "$binary" ||
      die "no s3://$BUCKET/releases/router/forkrail-router-$1"
    chmod 755 "$binary"
    NEW_ROUTER="$(shasum -a 256 "$binary" | cut -d' ' -f1)"
    [ "${NEW_ROUTER:0:16}" = "$1" ] || die "the copy's SHA-256 is $NEW_ROUTER, not $1…"
    load_state
    BEFORE="$RUN_DIR/hosts-before.txt"
    ADDRESSES=""
    for region in $(router_regions); do ADDRESSES="$ADDRESSES $(eip_of "$region")"; done
    probe $ADDRESSES > "$BEFORE"
    first=1
    for region in $(router_regions); do
      log "router $region back to ${NEW_ROUTER:0:16}"
      FORKRAIL_PREBUILT=1 FORKRAIL_TEST_FUNCTIONS=0 FORKRAIL_ROLES=$first FORKRAIL_ROUTER_BIN="$binary" \
        AWS_REGION="$region" "$AWS_DIR/test-env.sh" push
      first=0
      sha="$(running_sha "$region")"
      [ "$sha" = "$NEW_ROUTER" ] || die "the router in $region runs ${sha:0:16}, not ${NEW_ROUTER:0:16}"
      check_hosts "$BEFORE" "$RUN_DIR/hosts-$region.txt" "$(eip_of "$region")" || warn "hosts differ through $region (above)"
    done
    log "every router runs ${NEW_ROUTER:0:16}"
    ;;

  *) usage ;;
esac
