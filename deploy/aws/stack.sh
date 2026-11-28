#!/usr/bin/env bash
# forkrail's own infrastructure as CloudFormation stacks. The template deploy/install/NAME.yaml is
# the stack forkrail-NAME in AWS_REGION (us-east-1 unless set). Every change goes through a change
# set that is shown first; nothing here edits a resource directly. New infrastructure starts here
# (from 2026-11-11), and the installer will apply the same templates. The routers, the analytics
# box and the accelerator still come from test-env.sh and analytics.sh.
#
#   deploy/aws/stack.sh plan NAME [KEY=VALUE...]    what applying would change; changes nothing
#   deploy/aws/stack.sh apply NAME [KEY=VALUE...]   show the changes, ask, then make them
#       --yes             don't ask
#       --allow-replace   needed for any change that deletes or replaces an existing resource,
#                         --yes or not
#   deploy/aws/stack.sh status NAME                 its state, protection, parameters and outputs
#   deploy/aws/stack.sh failures NAME               what failed in it lately
#   deploy/aws/stack.sh ls                          forkrail's stacks in this region
#   deploy/aws/stack.sh delete NAME [--yes]         delete it after asking; resources the template
#                                                   marks DeletionPolicy: Retain are kept
#
# A parameter not given keeps the stack's current value, else the template's default. Never pass a
# secret as a parameter: templates read secrets from SSM themselves. Every stack is tagged
# project=forkrail (CloudFormation copies the tag to the resources that take tags) and protected
# from deletion, and a stack without the tag is never touched.
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TEMPLATES="$ROOT/deploy/install"
TAG_KEY=project
TAG_VALUE=forkrail
MAX_INLINE_BYTES=51200 # CloudFormation's limit for a template sent with the request

YES=0
ALLOW_REPLACE=0
STACK=""
CS_ID=""   # the change set this run made, until it runs or is discarded
CS_TYPE="" # CREATE or UPDATE

log() { echo "==> $*" >&2; }
die() { echo "$*" >&2; exit 1; }
usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2; }

stack_of() {
  [[ "$1" =~ ^[a-z][a-z0-9-]{0,40}$ ]] || die "$1: a stack's name is lowercase letters, digits and -"
  echo "forkrail-$1"
}

template_of() {
  local file="$TEMPLATES/$1.yaml"
  [ -f "$file" ] || die "no template $file"
  [ "$(wc -c <"$file")" -le "$MAX_INLINE_BYTES" ] ||
    die "$file is over $MAX_INLINE_BYTES bytes: split it, or have stack.sh send it through S3"
  echo "$file"
}

# The stack's status, or NONE. Any other error (expired credentials, say) stops the script.
status_of() {
  local out
  if out="$(aws cloudformation describe-stacks --stack-name "$1" --query 'Stacks[0].StackStatus' --output text 2>&1)"; then
    echo "$out"
  elif [[ "$out" == *"does not exist"* ]]; then
    echo NONE
  else
    die "$out"
  fi
}

require_tag() {
  local tag
  tag="$(aws cloudformation describe-stacks --stack-name "$1" --query "Stacks[0].Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)"
  [ "$tag" = "$TAG_VALUE" ] || die "$1 isn't tagged $TAG_KEY=$TAG_VALUE; not touching it"
}

confirm() {
  [ "$YES" = 1 ] && return 0
  { true </dev/tty; } 2>/dev/null || die "no terminal to ask on: pass --yes"
  local answer
  read -r -p "$1 [y/N] " answer </dev/tty
  [[ "$answer" =~ ^[Yy]$ ]] || die "nothing changed"
}

# Polls until the stack settles; prints its final status (NONE once deleted).
wait_stack() {
  local status last=""
  while :; do
    status="$(status_of "$1")"
    [ "$status" = "$last" ] || log "$1: ${status/#NONE/deleted}"
    last="$status"
    [[ "$status" == *_IN_PROGRESS ]] || break
    sleep 5
  done
  echo "$status"
}

failures() {
  aws cloudformation describe-events --stack-name "$1" --filters FailedEvents=true --max-items 20 \
    --query 'OperationEvents[].[Timestamp, LogicalResourceId, ResourceStatus || ValidationName, ResourceStatusReason || ValidationStatusReason]' \
    --output text >&2
}

# The change set's parameters as JSON: the ones given, then the stack's current values for the
# others it has; the rest take the template's defaults.
parameters() {
  local file="$1" stack="$2" status="$3"
  shift 3
  local keys current="[]" given="{}" pair unknown
  keys="$(aws cloudformation validate-template --template-body "file://$file" --query 'Parameters[].ParameterKey' --output json)"
  for pair in "$@"; do
    [[ "$pair" == *=* ]] || die "$pair: parameters are KEY=VALUE"
    given="$(jq -c --arg k "${pair%%=*}" --arg v "${pair#*=}" '. + {($k): $v}' <<<"$given")"
  done
  unknown="$(jq -r --argjson keys "$keys" 'keys - ($keys // []) | join(", ")' <<<"$given")"
  [ -z "$unknown" ] || die "the template has no parameter $unknown"
  if [ "$status" != NONE ] && [ "$status" != REVIEW_IN_PROGRESS ]; then
    current="$(aws cloudformation describe-stacks --stack-name "$stack" --query 'Stacks[0].Parameters[].ParameterKey' --output json)"
  fi
  jq -nc --argjson keys "$keys" --argjson given "$given" --argjson current "$current" '
    [($keys // [])[] as $k
      | if $given[$k] != null then {ParameterKey: $k, ParameterValue: $given[$k]}
        elif (($current // []) | any(. == $k)) then {ParameterKey: $k, UsePreviousValue: true}
        else empty end]'
}

# Makes a change set for NAME's template and sets STACK, CS_ID and CS_TYPE. CS_ID stays empty when
# there's nothing to change.
change_set() {
  local name="$1"
  shift
  local file status params state reason
  STACK="$(stack_of "$name")"
  file="$(template_of "$name")"
  status="$(status_of "$STACK")"
  case "$status" in
    NONE) CS_TYPE=CREATE ;;
    REVIEW_IN_PROGRESS) CS_TYPE=CREATE && require_tag "$STACK" ;; # created by a change set, never run
    CREATE_COMPLETE | UPDATE_COMPLETE | UPDATE_ROLLBACK_COMPLETE | IMPORT_COMPLETE | IMPORT_ROLLBACK_COMPLETE)
      CS_TYPE=UPDATE && require_tag "$STACK" ;;
    ROLLBACK_COMPLETE) die "$STACK failed to create and was rolled back ($0 failures $name says why): $0 delete $name, then apply again" ;;
    *) die "$STACK is $status: wait for it to settle, or see $0 failures $name" ;;
  esac
  params="$(parameters "$file" "$STACK" "$status" ${@+"$@"})"
  log "$STACK: change set from ${file#"$ROOT"/} ($AWS_REGION)"
  CS_ID="$(aws cloudformation create-change-set --stack-name "$STACK" \
    --change-set-name "stack-sh-$(date -u +%Y%m%d-%H%M%S)" --change-set-type "$CS_TYPE" \
    --template-body "file://$file" --parameters "$params" --tags "Key=$TAG_KEY,Value=$TAG_VALUE" \
    --capabilities CAPABILITY_IAM CAPABILITY_NAMED_IAM --description "deploy/aws/stack.sh $name" \
    --query Id --output text)"
  trap discard EXIT
  while :; do
    state="$(aws cloudformation describe-change-set --change-set-name "$CS_ID" --query Status --output text)"
    [[ "$state" == CREATE_PENDING || "$state" == CREATE_IN_PROGRESS ]] || break
    sleep 2
  done
  [ "$state" = FAILED ] || return 0
  reason="$(aws cloudformation describe-change-set --change-set-name "$CS_ID" --query StatusReason --output text)"
  if [[ "$reason" == *"didn't contain changes"* || "$reason" == *"No updates are to be performed"* ]]; then
    discard
    return 0
  fi
  echo "$STACK: the change set failed: $reason" >&2
  aws cloudformation describe-events --change-set-name "$CS_ID" \
    --query "OperationEvents[?EventType=='VALIDATION_ERROR'].[ValidationName, LogicalResourceId, ValidationPath, ValidationStatusReason]" \
    --output text >&2 || true
  exit 1 # discard runs on exit
}

# Throws away an unexecuted change set, and the empty stack a CREATE change set leaves behind.
discard() {
  [ -n "$CS_ID" ] || return 0
  aws cloudformation delete-change-set --change-set-name "$CS_ID" >/dev/null 2>&1 || true
  if [ "$CS_TYPE" = CREATE ]; then
    aws cloudformation delete-stack --stack-name "$STACK" >/dev/null 2>&1 || true
  fi
  CS_ID=""
}

show_changes() {
  local rows
  rows="$(aws cloudformation describe-change-set --change-set-name "$CS_ID" \
    --query 'Changes[].ResourceChange.[Action, LogicalResourceId, ResourceType, Replacement || `-`]' --output text)"
  if [ -z "$rows" ]; then
    log "no resource changes (outputs or parameters only)"
  else
    { echo "ACTION RESOURCE TYPE REPLACES"; echo "$rows"; } | column -t | sed 's/^/  /' >&2
  fi
}

# Changes that delete or replace a resource that exists: a replaced resource is a new one, with a
# new id and, for anything stateful, without its data.
destructive() {
  aws cloudformation describe-change-set --change-set-name "$CS_ID" --output text \
    --query "length(Changes[?ResourceChange.Action=='Remove' || ResourceChange.Replacement=='True' || ResourceChange.Replacement=='Conditional'])"
}

plan() {
  change_set "$@"
  [ -n "$CS_ID" ] || { log "$STACK: no changes"; return 0; }
  show_changes
  log "plan only: nothing was changed"
}

apply() {
  local count final
  change_set "$@"
  [ -n "$CS_ID" ] || { log "$STACK: no changes"; return 0; }
  show_changes
  count="$(destructive)"
  if [ "$count" != 0 ] && [ "$ALLOW_REPLACE" = 0 ]; then
    die "$count of these changes delete or replace an existing resource; nothing was changed. Pass --allow-replace if that's meant."
  fi
  confirm "apply these changes to $STACK?"
  aws cloudformation execute-change-set --change-set-name "$CS_ID"
  CS_ID=""
  trap - EXIT
  final="$(wait_stack "$STACK")"
  if [ "$final" != CREATE_COMPLETE ] && [ "$final" != UPDATE_COMPLETE ]; then
    failures "$STACK"
    die "$STACK: $final"
  fi
  if [ "$CS_TYPE" = CREATE ]; then
    aws cloudformation update-termination-protection --enable-termination-protection --stack-name "$STACK" >/dev/null
  fi
  aws cloudformation describe-stacks --stack-name "$STACK" --query 'Stacks[0].Outputs[].[OutputKey, OutputValue]' --output text
}

status() {
  local stack
  stack="$(stack_of "$1")"
  [ "$(status_of "$stack")" != NONE ] || die "no stack $stack in $AWS_REGION"
  aws cloudformation describe-stacks --stack-name "$stack" --output yaml --query 'Stacks[0].{
    status: StackStatus, reason: StackStatusReason, updated: LastUpdatedTime || CreationTime,
    protected: EnableTerminationProtection, tags: Tags[].join(`=`, [Key, Value]),
    parameters: Parameters[].join(`=`, [ParameterKey, ParameterValue]),
    outputs: Outputs[].join(`=`, [OutputKey, OutputValue])}'
}

list() {
  local rows
  rows="$(aws cloudformation describe-stacks --output text \
    --query "Stacks[?Tags[?Key=='$TAG_KEY' && Value=='$TAG_VALUE']].[StackName, StackStatus, LastUpdatedTime || CreationTime]")"
  [ -n "$rows" ] || { log "no forkrail stacks in $AWS_REGION"; return 0; }
  { echo "STACK STATUS UPDATED"; echo "$rows"; } | column -t
}

delete() {
  local stack final
  stack="$(stack_of "$1")"
  [ "$(status_of "$stack")" != NONE ] || die "no stack $stack in $AWS_REGION"
  require_tag "$stack"
  # Some ids are signed URLs (a wait condition's): never print their query strings.
  aws cloudformation list-stack-resources --stack-name "$stack" --output text \
    --query 'StackResourceSummaries[].[LogicalResourceId, ResourceType, PhysicalResourceId]' |
    sed -E 's/\?[^[:space:]]*//' | column -t | sed 's/^/  /' >&2
  confirm "delete $stack and the resources above (except any the template retains)?"
  aws cloudformation update-termination-protection --no-enable-termination-protection --stack-name "$stack" >/dev/null
  aws cloudformation delete-stack --stack-name "$stack"
  final="$(wait_stack "$stack")"
  [ "$final" = NONE ] || { failures "$stack"; die "$stack: $final"; }
}

command="${1:-}"
[ $# -gt 0 ] && shift
args=()
for arg in "$@"; do
  case "$arg" in
    --yes) YES=1 ;;
    --allow-replace) ALLOW_REPLACE=1 ;;
    --*) die "unknown option $arg" ;;
    *) args+=("$arg") ;;
  esac
done
set -- ${args[@]+"${args[@]}"}

case "$command" in
  plan) [ $# -ge 1 ] || usage; plan "$@" ;;
  apply) [ $# -ge 1 ] || usage; apply "$@" ;;
  status) [ $# -eq 1 ] || usage; status "$1" ;;
  failures) [ $# -eq 1 ] || usage; failures "$(stack_of "$1")" ;;
  ls) [ $# -eq 0 ] || usage; list ;;
  delete) [ $# -eq 1 ] || usage; delete "$1" ;;
  *) usage ;;
esac
