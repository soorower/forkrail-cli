#!/usr/bin/env bash
# The dashboard (web/, its own repo) as a forkrail project, deployed like any user's site: the CLI
# builds it with Nitro, the server runs on the Node runtime, the static files go to S3 for the
# routers. What it needs beyond any site:
# - an IAM role to read forkrail's state and keep its own table (people, the team, sessions:
#   deploy/install/web.yaml, `stack.sh apply web`), as any app that calls AWS would
#   (`forkrail project --role`);
# - the `analytics` egress target, since the analytics box (ClickHouse, for logs) admits only the
#   routers (`forkrail project --egress`, and the routers' FORKRAIL_EGRESS_TARGETS: analytics.sh
#   egress), and `postgres` for older deployments, which kept people there;
# - the install's GitHub App, which people sign in with (`forkrail git setup`).
#
#   deploy/aws/dashboard.sh role        create or update the forkrail-dashboard role
#   deploy/aws/dashboard.sh setup       the role, and the project's role and egress
#   deploy/aws/dashboard.sh deploy [--prod]
#                                       build web/ and deploy it (in the install's builder; --local
#                                       builds here, with this checkout's runtime)
#
# FORKRAIL_PREBUILT=1 uses target/'s CLI as it is, without compiling (release.sh builds it first).
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
STATE_REGION=us-east-1
export AWS_REGION=$STATE_REGION
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ACCOUNT="$(aws sts get-caller-identity --query Account --output text)"
BUCKET="forkrail-artifacts-$ACCOUNT"
PROJECT=forkrail-web
ROLE=forkrail-dashboard
READER_PARAM=/forkrail/clickhouse/reader
BUILDLOG_PARAM=/forkrail/clickhouse/buildlog
MEMORY=1024
TAG_KEY=project
TAG_VALUE=forkrail
FORKRAIL="$ROOT/target/release/forkrail"

log() { echo "==> $*" >&2; }

# The CLI, and the runtime it packages, built from this checkout (quick when nothing changed).
# With AWS credentials, whoever is signed in (`forkrail login`): this builds here and sets roles.
cli() {
  [ "${FORKRAIL_PREBUILT:-0}" = 1 ] || (cd "$ROOT" && cargo build --quiet --release -p forkrail-cli >&2)
  "$FORKRAIL" --aws "$@"
}

runtime() {
  (cd "$ROOT" && cargo zigbuild --quiet --release --target aarch64-unknown-linux-gnu.2.34 -p forkrail-runtime >&2)
}

# The role reads forkrail's state and writes only its own table, forkrail-web (people, the team,
# members, sessions, terminals' tokens: deploy/install/web.yaml), whose resource policy admits no
# one else. Builds' log lines go into ClickHouse as `buildlog` (insert into build_logs, nothing
# else). Older dashboard deployments (a rollback) keep their people in Postgres on the analytics
# box, reached through the router (egress `postgres`). Its changes to deployments (promote,
# redeploy, settings, env vars) go through the control function, whose own resource policy lets
# this role invoke it (deploy/install/control.yaml); nothing here grants that.
#
# Its own secrets (the Postgres password, GitHub's client and webhook secrets) are SSM parameters
# under /forkrail/web/, encrypted with the key that also protects every project's env var values.
# So its kms:Decrypt is narrowed three ways: that key only, only through SSM, and only where the
# encryption context names a /forkrail/web/ parameter (SSM sets the context to the parameter's
# ARN). A project's env values (/forkrail/env/...) and functions' environments have other
# contexts, and this role can't decrypt them.
role() {
  if ! aws iam get-role --role-name "$ROLE" >/dev/null 2>&1; then
    log "creating role $ROLE"
    aws iam create-role --role-name "$ROLE" \
      --description "forkrail dashboard: reads forkrail's state, the ClickHouse reader password, and infrastructure status" \
      --assume-role-policy-document "$(jq -n --arg account "$ACCOUNT" \
        '{Version: "2012-10-17", Statement: [{Effect: "Allow", Principal: {Service: "lambda.amazonaws.com"},
          Action: "sts:AssumeRole", Condition: {StringEquals: {"aws:SourceAccount": $account}}}]}')" \
      --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null
  elif [ "$(aws iam list-role-tags --role-name "$ROLE" --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)" != "$TAG_VALUE" ]; then
    echo "$ROLE exists without $TAG_KEY=$TAG_VALUE; not touching it" >&2
    exit 1
  fi
  # Applied on every run, so edits here take effect. Read-only but for its page previews (one
  # screenshot and its JSON per deployment, under previews/: web/src/server/page-preview.ts); no
  # logs. The screenshot function lets this role invoke it by its own policy (screenshot.yaml).
  aws iam put-role-policy --role-name "$ROLE" --policy-name "$ROLE" --policy-document "$(jq -n \
    --arg bucket "arn:aws:s3:::$BUCKET" \
    --arg state "arn:aws:s3:::$BUCKET/state/*" \
    --arg certkeys "arn:aws:s3:::$BUCKET/state/certs/*/key.pem" \
    --arg acmekeys "arn:aws:s3:::$BUCKET/state/certs/*/account.json" \
    --arg buildlogs "arn:aws:s3:::$BUCKET/logs/builds/*" \
    --arg previews "arn:aws:s3:::$BUCKET/previews/*" \
    --arg manifests "arn:aws:s3:::$BUCKET/static/manifests/*" \
    --arg reader "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter$READER_PARAM" \
    --arg buildlog "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter$BUILDLOG_PARAM" \
    --arg web "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter/forkrail/web/*" \
    --arg app "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter/forkrail/github/app" \
    --arg keys "arn:aws:kms:$STATE_REGION:$ACCOUNT:key/*" \
    --arg ssm "ssm.$STATE_REGION.amazonaws.com" \
    --arg table "arn:aws:dynamodb:$STATE_REGION:$ACCOUNT:table/forkrail-state" \
    --arg webtable "arn:aws:dynamodb:$STATE_REGION:$ACCOUNT:table/forkrail-web" \
    --arg ddb "dynamodb.$STATE_REGION.amazonaws.com" \
    --arg notify "arn:aws:lambda:$STATE_REGION:$ACCOUNT:function:forkrail-notify" \
    --arg ownfunctions "arn:aws:lambda:*:$ACCOUNT:function:forkrail-*" \
    '{Version: "2012-10-17", Statement: [
      {Sid: "State", Effect: "Allow", Action: "s3:GetObject", Resource: $state},
      # The notifier (deploy/install/notify.yaml): test emails and deliveries, address checks, its
      # SES domain. The SMTP password and webhook secrets are encrypted here with its key and
      # decrypted only by it: this role may encrypt, never decrypt (the key policy says so too).
      {Sid: "Notifier", Effect: "Allow", Action: "lambda:InvokeFunction", Resource: $notify},
      {Sid: "NotifierSecrets", Effect: "Allow", Action: "kms:Encrypt", Resource: $keys, Condition: {
        "ForAnyValue:StringEquals": {"kms:ResourceAliases": "alias/forkrail-notify"},
        StringLike: {"kms:EncryptionContext:forkrail": ["smtp-password", "webhook-secret"]}}},
      # state/ also holds the private key of every certificate and the ACME account key: only the
      # routers read those. The dashboard reads cert.pem (expiry), never the keys.
      {Sid: "NoCertificateKeys", Effect: "Deny", Action: "s3:GetObject", Resource: [$certkeys, $acmekeys]},
      {Sid: "BuildLogs", Effect: "Allow", Action: "s3:GetObject", Resource: $buildlogs},
      {Sid: "Previews", Effect: "Allow", Action: ["s3:GetObject", "s3:PutObject"], Resource: $previews},
      # A deployment page lists its static files from the manifest the routers serve them by:
      # paths, sizes and types, never a file itself.
      {Sid: "StaticManifests", Effect: "Allow", Action: "s3:GetObject", Resource: $manifests},
      # Project files and deployment records once they are in the state table (deploy/install/state.yaml),
      # read only: the policy of the table itself refuses this role every write too. DescribeTable
      # is its size and settings (the usage page), never an item. No apostrophes in here: the
      # filter is one single-quoted shell word.
      {Sid: "StateTable", Effect: "Allow", Action: ["dynamodb:GetItem", "dynamodb:BatchGetItem", "dynamodb:Query", "dynamodb:DescribeTable"],
        Resource: [$table, ($table + "/index/*")]},
      {Sid: "StateKey", Effect: "Allow", Action: ["kms:Decrypt", "kms:DescribeKey"], Resource: $keys, Condition: {
        "ForAnyValue:StringEquals": {"kms:ResourceAliases": "alias/forkrail-state"},
        StringEquals: {"kms:ViaService": $ddb}}},
      # Its own table (deploy/install/web.yaml): items one by one, queries and transactions. The
      # policy of the table itself refuses this role Scan, PartiQL, exports, backups and restores.
      {Sid: "WebTable", Effect: "Allow", Action: ["dynamodb:GetItem", "dynamodb:BatchGetItem", "dynamodb:Query",
        "dynamodb:PutItem", "dynamodb:UpdateItem", "dynamodb:DeleteItem", "dynamodb:BatchWriteItem",
        "dynamodb:ConditionCheckItem", "dynamodb:DescribeTable"],
        Resource: [$webtable, ($webtable + "/index/*")]},
      {Sid: "WebKey", Effect: "Allow", Action: ["kms:Decrypt", "kms:Encrypt", "kms:GenerateDataKey*", "kms:ReEncrypt*", "kms:DescribeKey"],
        Resource: $keys, Condition: {
        "ForAnyValue:StringEquals": {"kms:ResourceAliases": "alias/forkrail-web"},
        StringEquals: {"kms:ViaService": $ddb}}},
      {Sid: "ListState", Effect: "Allow", Action: "s3:ListBucket", Resource: $bucket,
        Condition: {StringLike: {"s3:prefix": "state/*"}}},
      # The usage page adds up what the bucket of each region keeps, by folder and project: key
      # names and sizes in the buckets of this install, never the contents of an object.
      {Sid: "ListKept", Effect: "Allow", Action: "s3:ListBucket", Resource: [$bucket, ($bucket + "-*")]},
      {Sid: "ClickHouseReader", Effect: "Allow", Action: "ssm:GetParameter", Resource: $reader},
      {Sid: "ClickHouseBuildLog", Effect: "Allow", Action: "ssm:GetParameter", Resource: $buildlog},
      {Sid: "OwnParameters", Effect: "Allow", Action: "ssm:GetParameter", Resource: [$web, $app]},
      {Sid: "OwnSecrets", Effect: "Allow", Action: "kms:Decrypt", Resource: $keys, Condition: {
        "ForAnyValue:StringEquals": {"kms:ResourceAliases": "alias/forkrail-env"},
        StringEquals: {"kms:ViaService": $ssm},
        StringLike: {"kms:EncryptionContext:PARAMETER_ARN": $web}}},
      # The two ClickHouse passwords it reads, under the install key (deploy/install/install-secrets.yaml).
      {Sid: "ClickHouseSecrets", Effect: "Allow", Action: "kms:Decrypt", Resource: $keys, Condition: {
        "ForAnyValue:StringEquals": {"kms:ResourceAliases": "alias/forkrail-install"},
        StringEquals: {"kms:ViaService": $ssm, "kms:EncryptionContext:PARAMETER_ARN": [$reader, $buildlog]}}},
      # Not lambda:ListFunctions, nor GetFunctionConfiguration on every function: both answer the
      # environment variables of a function, and other workloads of the account keep secrets there.
      {Sid: "Infrastructure", Effect: "Allow", Resource: "*", Action: [
        "ec2:DescribeInstances", "ec2:DescribeVolumes",
        "globalaccelerator:ListAccelerators", "globalaccelerator:ListListeners", "globalaccelerator:ListEndpointGroups",
        "lambda:GetAccountSettings",
        "cloudwatch:GetMetricStatistics", "cloudwatch:DescribeAlarms", "cloudwatch:ListMetrics",
        "kms:ListAliases", "route53:GetHealthCheck", "ecr:DescribeImages"]},
      # The functions of forkrail itself, whose memory the usage page prices (web/src/server/costs.ts).
      {Sid: "OwnFunctions", Effect: "Allow", Action: "lambda:GetFunctionConfiguration", Resource: $ownfunctions},
      # The usage page shares the free allowances with whatever else runs in the account: the
      # accounts use of their kinds each day, from its bill (web/src/server/account-use.ts). Cost
      # Explorer has no resources to name; it reads the whole bill, a call a day at $0.01.
      {Sid: "AccountBill",Effect: "Allow", Resource: "*", Action: "ce:GetCostAndUsage"}]}')"
  log "$ROLE: policy applied"
}

case "${1:-}" in
  role) role ;;
  setup)
    role
    cli project "$PROJECT" --role "$ROLE" --egress analytics,postgres
    log "next: \`forkrail git setup\` (once per install), then $0 deploy --prod"
    ;;
  deploy)
    shift
    # The runtime packaged here matters only to a build on this machine; the builder has its own.
    case " $* " in *" --local "* | *" --prebuilt "* | *" --bootstrap"*) [ "${FORKRAIL_PREBUILT:-0}" = 1 ] || runtime ;; esac
    cli deploy "$ROOT/web" --project "$PROJECT" --memory "$MEMORY" "$@"
    ;;
  *)
    sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
