#!/usr/bin/env bash
# Test environment: one router instance and Lambda worker functions: forkrail-echo (the
# runtime's built-in echo app), forkrail-node (test-apps/node-http on Node), forkrail-python
# (test-apps/python-fastapi), and any app deployed with `app`. Every resource is named forkrail-*
# and tagged project=forkrail, and `down` deletes only resources that carry that tag.
#
# The router serves the deployments in s3://$BUCKET/state/routes.json, which the forkrail CLI
# (target/release/forkrail, built by up and push) writes; hosts it doesn't know go to the function
# `use` picked.
#
#   deploy/aws/test-env.sh up          build, create everything, deploy
#   deploy/aws/test-env.sh push        build and redeploy the router and the three test functions, then restart the
#                                      router with the new binary already on the instance, so the gap is short.
#                                      deploy/aws/release.sh runs it with these, to skip what it has done already:
#                                      FORKRAIL_PREBUILT=1 (no compiling: target/'s binaries as they are),
#                                      FORKRAIL_TEST_FUNCTIONS=0 (the test functions left as they are),
#                                      FORKRAIL_ROLES=0 (IAM is global: applied by the first region's push),
#                                      FORKRAIL_ROUTER_BIN=PATH (that router binary instead of target/'s)
#   deploy/aws/test-env.sh domain BASE ZONE_ID [staging]
#                                      serve https://<project>.BASE: the router's Elastic IP, a DNS-only wildcard
#                                      *.BASE in Cloudflare zone ZONE_ID (token from SSM, never printed), BASE in
#                                      state/settings.json for the CLI, and the router's TLS settings (Let's Encrypt;
#                                      `staging` for its staging server). Idempotent
#   deploy/aws/test-env.sh logs PROJECT|DEPLOYMENT|default [SINCE]
#                                      the app's output from the router's log, since SINCE (journalctl syntax,
#                                      default "1 hour ago")
#   deploy/aws/test-env.sh report SINCE [UNTIL]
#                                      per project: requests, status codes, latency, workers, cold starts, and
#                                      estimated Lambda, egress and Lambda-router transfer cost; for public traffic,
#                                      latency and concurrency by hour, the busiest paths, failures and the slowest
#                                      requests with their ids
#   deploy/aws/test-env.sh point HOST TARGET
#                                      make HOST, in whichever Cloudflare zone the token can edit, a DNS-only CNAME
#                                      to TARGET with TTL 60, and print what it was: moves a custom domain to the
#                                      router (TARGET <project>.<base>) and back. Only an existing CNAME is changed
#   deploy/aws/test-env.sh monitor HOST EMAIL
#                                      watch HOST from outside: a Route 53 HTTPS health check of /_forkrail/health
#                                      from several regions (with latency), alarms on it and on 5xx (from the
#                                      router's 10-minute metrics), mailed to EMAIL through SNS topic forkrail-alerts
#                                      (confirm the subscription mail once), and the certificate expiry alarm
#                                      (expiry-alarm). Idempotent. Also installs the log export (below), which
#                                      push keeps up to date
#   deploy/aws/test-env.sh roles       the routers' and workers' IAM roles alone, as push applies them (no router change)
#   deploy/aws/test-env.sh attack-alarm
#                                      alarm forkrail-attack alone, on the existing topic: the routers' system
#                                      mitigations refused 10,000 or more requests and connections in 10 minutes
#   deploy/aws/test-env.sh expiry-alarm
#                                      alarm forkrail-cert-expiry alone, on the existing topic: a certificate with
#                                      under 14 days left, or 6 hours without the issuer's hourly word on it
#   deploy/aws/test-env.sh alerts-policy [default]
#                                      the alerts topic's policy alone (monitor sets it too): the account's alarms
#                                      publish, apps, builds and page previews are refused. `default` puts SNS's own
#                                      default policy back, as the topic had before (any principal in the account)
#
# The router's journal goes to s3://$BUCKET/logs/router/ every 10 minutes as JSON lines
# (log-export.sh, a systemd timer), with request, 5xx and p95 counts as CloudWatch metrics in the
# forkrail namespace, and the issuer's soonest certificate expiry (CertificateDaysLeft).
#   deploy/aws/test-env.sh app NAME DIR ENTRY [KEY=VALUE...]
#                                      deploy the Python app in DIR as forkrail-NAME, e.g.
#                                      `app atlas ../apps/atlas api/main.py`, with
#                                      KEY=VALUE as extra environment variables
#   deploy/aws/test-env.sh use NAME    point the router at forkrail-NAME (echo, node, python or an app)
#   deploy/aws/test-env.sh ip          print the router's public IP (its Elastic IP once `domain` has run)
#   deploy/aws/test-env.sh run CMD     run a shell command on the router through SSM
#   deploy/aws/test-env.sh tcp         apply deploy/aws/tcp-tuning.sh on the router (push and up do too) and print
#                                      the live values: no restart, nothing dropped
#   deploy/aws/test-env.sh load DURATION CONNECTIONS PATH|URL [HOST...]
#                                      load-test the router from its own instance with oha,
#                                      e.g. `load 60s 50 '/sleep?ms=200'`, then summarize its log;
#                                      with HOSTs, one oha per Host header at once, CONNECTIONS each;
#                                      a URL (https://pyapp.forkrail.com/...) goes through the public name.
#                                      OHA_ARGS adds oha options, e.g. OHA_ARGS=--http2
#   deploy/aws/test-env.sh memory NAME MB   set forkrail-NAME's memory size
#   deploy/aws/test-env.sh router-env KEY=VALUE...
#                                      set router environment variables and restart it
#   deploy/aws/test-env.sh replay PATHS_FILE MODE [ARGS...]
#                                      run deploy/aws/replay.py on the router instance against the
#                                      router, e.g. `replay paths.txt poisson --rate 2 --duration 300`
#   deploy/aws/test-env.sh fetch SINCE OUT   save the router's journal since SINCE (UTC, as
#                                      `date -u '+%Y-%m-%d %H:%M:%S'` prints it) as JSON lines in OUT
#   deploy/aws/test-env.sh geoip       install DB-IP's City and ASN databases and the daily timer that
#                                      keeps them current (push does too)
#   deploy/aws/test-env.sh vector      install or update Vector on the router (push does too, once the analytics box
#                                      exists): its log files to ClickHouse every 2 seconds. See analytics.sh
#   deploy/aws/test-env.sh stop|start  stop or start the router instance
#   deploy/aws/test-env.sh down        delete everything (outside STATE_REGION: that region's router, its
#                                      Elastic IP, security group and functions only)
#
# Regions: AWS_REGION picks the router a command works on, default us-east-1 (STATE_REGION, which
# keeps the state and issues certificates). `AWS_REGION=us-west-1 test-env.sh up` starts a router
# there that serves every host with the same certificates; `forkrail regions add us-west-1` then
# publishes the deployments there, so it invokes them in its own region.
#   deploy/aws/test-env.sh accelerator Global Accelerator in front of the routers in ROUTER_REGIONS: two static
#                                      IPs, each client to the nearest healthy router (health: HTTP on port 80,
#                                      /_forkrail/health). Idempotent; prints the IPs. DNS isn't touched
#   deploy/aws/test-env.sh dial REGION PERCENT
#                                      the share of its nearby traffic REGION's router takes (0 sends it all to
#                                      the next nearest), before maintenance, say
#   deploy/aws/test-env.sh wildcard ZONE_ID BASE IP...
#                                      make *.BASE's A records exactly IP... (DNS only, TTL 60), adding before
#                                      deleting so it always resolves, and print what they were: the accelerator's
#                                      IPs to move every project behind it, the router's Elastic IP to move back
#   deploy/aws/test-env.sh arecord ZONE_ID NAME [IP...]
#                                      the same for one name, e.g. atlas.forkrail.com to move one project (and
#                                      the custom domains that CNAME to it) ahead of the wildcard; without IPs, its
#                                      A records go and it follows the wildcard again
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION="${AWS_REGION:-us-east-1}"
export AWS_PAGER=""
# AWS_REGION picks the router being worked on (each region has its own instance, Elastic IP and
# Lambda functions). The state stays in STATE_REGION: the bucket (routing table, certificates,
# logs), the Cloudflare token, the alerts, and the router that issues certificates. Routers in
# other regions read the same table, so they know every host.
STATE_REGION=us-east-1
# Regions with a router, for `accelerator`.
ROUTER_REGIONS="${ROUTER_REGIONS:-us-east-1 us-west-1}"
# Global Accelerator's API is in us-west-2, whatever regions it serves.
GA_REGION=us-west-2
GA_NAME=forkrail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET=aarch64-unknown-linux-gnu
RELEASE="$ROOT/target/$TARGET/release"
ROUTER_BIN="${FORKRAIL_ROUTER_BIN:-$RELEASE/forkrail-router}"
PACKAGES="$ROOT/target/lambda"
ZIP="$PACKAGES/runtime.zip"
PYTHON_ZIP="$PACKAGES/python.zip"
ACCOUNT="$(aws sts get-caller-identity --query Account --output text)"
BUCKET="forkrail-artifacts-$ACCOUNT"
FUNCTION=forkrail-echo
NODE_FUNCTION=forkrail-node
# Lambda's managed Node runtime, with our bootstrap as its exec wrapper: the wrapper starts in
# place of AWS's Node bootstrap and never runs it, and Node comes from the runtime's image at
# /var/lang/bin/node. Cold starts measured 240-440 ms this way, against 850-1,100 ms with Node in
# a layer on provided.al2023, and AWS keeps Node patched.
NODE_RUNTIME=nodejs24.x
PYTHON_FUNCTION=forkrail-python
# The same for Python: the wrapper runs /var/lang/bin/python3 from the managed runtime's image.
PYTHON_RUNTIME=python3.13
# Packages are compiled to bytecode with the same Python version (python@3.13 from Homebrew).
PYTHON="${PYTHON:-python3.13}"
# The environment every Python app runs with; packages are in python/ (see package_python).
PYTHON_ENV="AWS_LAMBDA_EXEC_WRAPPER=/var/task/bootstrap,FORKRAIL_APP_DIR=/var/task/app,PYTHONPATH=/var/task/python"
WORKER_ROLE=forkrail-worker
NAME=forkrail-router # router role, instance profile, security group and instance
EIP_NAME=forkrail-router-eip
# The Cloudflare API token (DNS edit on the base domain's zone), as an SSM SecureString.
CLOUDFLARE_TOKEN_PARAM=/forkrail/cloudflare-api-token
# The routers' shared key for the firewall's challenge passes (crates/common/src/install.rs).
CHALLENGE_KEY_PARAM=/forkrail/router/challenge-key
# The routers' shared key for their link, over which pages go between them (cache/peers.rs).
PEER_KEY_PARAM=/forkrail/router/peer-key
# The port routers reach each other on for pages, open to the other routers' addresses only.
PEER_PORT=7445
# The install's own secrets' key (deploy/install/install-secrets.yaml): the token, the challenge and
# peer keys and the ClickHouse passwords are encrypted with it.
INSTALL_KEY=alias/forkrail-install
INSTANCE_TYPE=t4g.small
TAG_KEY=project
TAG_VALUE=forkrail
# Load generator, run on the router instance so home bandwidth isn't in the numbers.
OHA_URL=https://github.com/hatoo/oha/releases/download/v1.16.0/oha-linux-arm64
OHA_SHA256=99a790eb8c3e0feaca974bd6b32f0f8d4426a0c5b289f39e833e5b2c7529cd39
# Vector ships the router's log to the analytics box (deploy/aws/vector/vector.yaml). The tarball
# is in s3://$BUCKET/tools/, put there once from the release page and checked against this.
VECTOR_VERSION=0.60.0
VECTOR_TARBALL=vector-$VECTOR_VERSION-aarch64-unknown-linux-musl.tar.gz
VECTOR_SHA256=e2984f826aa61bf3ea98fba4cccde2437f645794b2f8a3d45c97df9594dcfbd1
CLICKHOUSE_INGEST_PARAM=/forkrail/clickhouse/ingest

log() { echo "==> $*" >&2; }

build_binaries() {
  log "building arm64 binaries"
  (cd "$ROOT" && cargo zigbuild --release --target "$TARGET.2.34" -p forkrail-runtime -p forkrail-router)
}

# The CLI, for this machine. It packages target/$TARGET/release/forkrail-runtime as each project's bootstrap.
build_cli() {
  log "building the forkrail CLI"
  (cd "$ROOT" && cargo build --release -p forkrail-cli)
}

# FORKRAIL_PEERS, as the routers in ROUTER_REGIONS are now: the router finds the others with it
# (and its home, the one in STATE_REGION). Written on every push, so a router added since is known.
peers_env() {
  local spec
  spec="$(peers_spec)"
  [ -n "$spec" ] || return 0
  echo "sed -i '/^FORKRAIL_PEERS=/d' /etc/forkrail/router.env && echo 'FORKRAIL_PEERS=$spec' >> /etc/forkrail/router.env"
}

# Routers launched before projects existed have no FORKRAIL_ROUTES; add it.
ROUTES_ENV="grep -q '^FORKRAIL_ROUTES=' /etc/forkrail/router.env || echo 'FORKRAIL_ROUTES=s3://$BUCKET/state/routes.json' >> /etc/forkrail/router.env
grep -q '^FORKRAIL_STATE_REGION=' /etc/forkrail/router.env || printf 'FORKRAIL_STATE_REGION=%s\\nFORKRAIL_HOME_REGION=%s\\n' $STATE_REGION $STATE_REGION >> /etc/forkrail/router.env"

build() {
  build_binaries
  mkdir -p "$PACKAGES"
  local stage
  stage="$(mktemp -d)"
  cp "$RELEASE/forkrail-runtime" "$stage/bootstrap"
  # One package for echo and Node. Only forkrail-node is configured to run the app.
  mkdir "$stage/app"
  cp "$ROOT/test-apps/node-http/server.mjs" "$stage/app/"
  rm -f "$ZIP"
  (cd "$stage" && zip -q -X -r "$ZIP" bootstrap app)
  rm -rf "$stage"
  package_python "$ROOT/test-apps/python-fastapi" "$PYTHON_ZIP"
}

# package_python DIR ZIP: the Python app in DIR as a Lambda package: the runtime as bootstrap,
# the app in app/, and in python/ its requirements plus uvicorn (the shim serves the app with
# it), as Linux arm64 wheels. Everything is compiled to bytecode here, because /var/task is
# read-only: Python would otherwise compile every module again on every cold start.
package_python() {
  local src="$1" zip="$2" stage venv="$ROOT/target/python-build"
  log "packaging $src"
  [ -x "$venv/bin/python" ] || "$PYTHON" -m venv "$venv"
  stage="$(mktemp -d)"
  cp "$RELEASE/forkrail-runtime" "$stage/bootstrap"
  rsync -a --exclude .git --exclude .vercel --exclude __pycache__ --exclude '.env*' --exclude .DS_Store \
    --exclude node_modules --exclude .venv --exclude venv "$src/" "$stage/app/"
  local requirements=() platforms=(--platform manylinux2014_aarch64) glibc
  [ -f "$src/requirements.txt" ] && requirements=(-r "$src/requirements.txt")
  # Every glibc up to Amazon Linux 2023's 2.34: pip matches only the tags it's given.
  for glibc in $(seq 17 34); do platforms+=(--platform "manylinux_2_${glibc}_aarch64"); done
  "$venv/bin/python" -m pip install --quiet --disable-pip-version-check --target "$stage/python" \
    "${platforms[@]}" --implementation cp --python-version "${PYTHON_RUNTIME#python}" \
    --only-binary=:all: --no-compile "${requirements[@]}" uvicorn uvloop httptools
  # Unchecked: Lambda's copy of each file has a different mtime, which would void timestamped bytecode.
  "$venv/bin/python" -m compileall -q -j 0 --invalidation-mode unchecked-hash -s "$stage" -p /var/task \
    "$stage/app" "$stage/python" >/dev/null
  rm -f "$zip"
  (cd "$stage" && zip -q -X -r "$zip" bootstrap app python)
  rm -rf "$stage"
  log "$(basename "$zip"): $(du -h "$zip" | cut -f1)"
}

assume_role_policy() {
  jq -n --arg service "$1" \
    '{Version: "2012-10-17", Statement: [{Effect: "Allow", Principal: {Service: $service}, Action: "sts:AssumeRole"}]}'
}

ensure_roles() {
  if ! aws iam get-role --role-name "$WORKER_ROLE" >/dev/null 2>&1; then
    log "creating role $WORKER_ROLE"
    aws iam create-role --role-name "$WORKER_ROLE" \
      --description "forkrail Lambda workers. No policies: logs go to the router, not CloudWatch." \
      --assume-role-policy-document "$(assume_role_policy lambda.amazonaws.com)" \
      --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null
  fi
  if ! aws iam get-role --role-name "$NAME" >/dev/null 2>&1; then
    log "creating role $NAME"
    aws iam create-role --role-name "$NAME" \
      --description "forkrail test router: invoke forkrail-* functions, read build artifacts and routes, SSM." \
      --assume-role-policy-document "$(assume_role_policy ec2.amazonaws.com)" \
      --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null
    aws iam attach-role-policy --role-name "$NAME" \
      --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore
  fi
  # Applied on every run, so edits here take effect.
  # forkrail-control matches forkrail-* but is the dashboard's alone (deploy/install/control.yaml):
  # a router faces the internet, so it's denied outright.
  aws iam put-role-policy --role-name "$NAME" --policy-name "$NAME" --policy-document "$(jq -n \
    --arg functions "arn:aws:lambda:*:$ACCOUNT:function:forkrail-*" \
    --arg control "arn:aws:lambda:*:$ACCOUNT:function:forkrail-control" \
    --arg control_versions "arn:aws:lambda:*:$ACCOUNT:function:forkrail-control:*" \
    --arg bucket "arn:aws:s3:::$BUCKET" \
    --arg objects "arn:aws:s3:::$BUCKET/*" \
    --arg statics "arn:aws:s3:::$BUCKET-*/static/*" \
    --arg results "arn:aws:s3:::$BUCKET/results/*" \
    --arg certs "arn:aws:s3:::$BUCKET/state/certs/*" \
    --arg challenges "arn:aws:s3:::$BUCKET/state/acme/*" \
    --arg logs "arn:aws:s3:::$BUCKET/logs/*" \
    --arg spend_ledger "arn:aws:s3:::$BUCKET/state/spend/*" \
    --arg salts "arn:aws:s3:::$BUCKET/state/insights/salts/*" \
    --arg routers "arn:aws:s3:::$BUCKET/state/routers/*" \
    --arg token "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter$CLOUDFLARE_TOKEN_PARAM" \
    --arg ingest "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter$CLICKHOUSE_INGEST_PARAM" \
    --arg challenge "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter$CHALLENGE_KEY_PARAM" \
    --arg peer "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter$PEER_KEY_PARAM" \
    --arg schedules "arn:aws:sqs:$STATE_REGION:$ACCOUNT:forkrail-schedules" \
    --arg events "arn:aws:sqs:$STATE_REGION:$ACCOUNT:forkrail-events" \
    --arg spend "arn:aws:sqs:$STATE_REGION:$ACCOUNT:forkrail-spend" \
    --arg notify "arn:aws:lambda:*:$ACCOUNT:function:forkrail-notify" \
    --arg notify_versions "arn:aws:lambda:*:$ACCOUNT:function:forkrail-notify:*" \
    --arg builders "arn:aws:lambda:*:$ACCOUNT:function:forkrail-builder*" \
    --arg guard "arn:aws:lambda:*:$ACCOUNT:function:forkrail-guard*" \
    --arg screenshot "arn:aws:lambda:*:$ACCOUNT:function:forkrail-screenshot*" \
    --arg cost_tags "arn:aws:lambda:*:$ACCOUNT:function:forkrail-cost-tags*" \
    --arg install_fetch "arn:aws:lambda:*:$ACCOUNT:function:forkrail-install-fetch*" \
    --arg keys "arn:aws:kms:$STATE_REGION:$ACCOUNT:key/*" \
    --arg ssm "ssm.$STATE_REGION.amazonaws.com" \
    --arg install_key "$INSTALL_KEY" \
    '{Version: "2012-10-17", Statement: [
      {Effect: "Allow", Action: "lambda:InvokeFunction", Resource: $functions},
      # Not control, and not the notifier (it sends email as the team): the router sends events to
      # its queue instead, below. Nor the rest of forkrail itself, which it never calls: the
      # builders (a job of its making would run anything and report to control), the spend guard,
      # the screenshot and cost-tag functions.
      {Effect: "Deny", Action: "lambda:*", Resource: [$control, $control_versions, $notify, $notify_versions, $builders, $guard, $screenshot, $cost_tags, $install_fetch]},
      {Effect: "Allow", Action: "sqs:SendMessage", Resource: $events},
      # Spend pauses, for control to record (deploy/install/control.yaml; router env
      # FORKRAIL_SPEND_QUEUE=<its SpendQueueUrl output>).
      {Effect: "Allow", Action: "sqs:SendMessage", Resource: $spend},
      {Effect: "Allow", Action: "s3:GetObject", Resource: [$objects, $statics]},
      {Effect: "Allow", Action: "s3:ListBucket", Resource: $bucket},
      # state/spend/<region>.json: the month to date, for forkrail-guard (deploy/install/guard.yaml).
      # state/routers/<region>/<host:port>: where it is, for the other routers (crates/router/src/routers.rs).
      {Effect: "Allow", Action: "s3:PutObject", Resource: [$results, $certs, $logs, $challenges, $spend_ledger, $routers]},
      {Effect: "Allow", Action: "s3:DeleteObject", Resource: $challenges},
      # The Web Analytics salt of the day (crates/router/src/insights.rs): made by the first router to
      # need it, deleted once its day is over.
      {Effect: "Allow", Action: ["s3:PutObject", "s3:DeleteObject"], Resource: $salts},
      {Effect: "Allow", Action: "cloudwatch:PutMetricData", Resource: "*",
        Condition: {StringEquals: {"cloudwatch:namespace": "forkrail"}}},
      {Effect: "Allow", Action: "ssm:GetParameter", Resource: [$token, $ingest, $challenge, $peer]},
      # Its four secrets, decrypted through SSM with the install key, and no other parameter of the
      # account: AmazonSSMManagedInstanceCore (for Run Command) allows ssm:GetParameter on all.
      {Sid: "OwnSecrets", Effect: "Allow", Action: "kms:Decrypt", Resource: $keys, Condition: {
        "ForAnyValue:StringEquals": {"kms:ResourceAliases": $install_key},
        StringEquals: {"kms:ViaService": $ssm, "kms:EncryptionContext:PARAMETER_ARN": [$token, $ingest, $challenge, $peer]}}},
      {Sid: "NoOtherParameters", Effect: "Deny",
        Action: ["ssm:GetParameter", "ssm:GetParameters", "ssm:GetParametersByPath", "ssm:GetParameterHistory"],
        NotResource: [$token, $ingest, $challenge, $peer]},
      {Effect: "Allow", Action: ["sqs:ReceiveMessage", "sqs:DeleteMessage", "sqs:GetQueueAttributes"], Resource: $schedules}]}')"
  if ! aws iam get-instance-profile --instance-profile-name "$NAME" >/dev/null 2>&1; then
    aws iam create-instance-profile --instance-profile-name "$NAME" --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null
    aws iam add-role-to-instance-profile --instance-profile-name "$NAME" --role-name "$NAME"
  fi
}

ensure_bucket() {
  if aws s3api head-bucket --region "$STATE_REGION" --bucket "$BUCKET" >/dev/null 2>&1; then return; fi
  log "creating bucket $BUCKET"
  if [ "$STATE_REGION" = us-east-1 ]; then
    aws s3api create-bucket --region "$STATE_REGION" --bucket "$BUCKET" >/dev/null
  else
    aws s3api create-bucket --region "$STATE_REGION" --bucket "$BUCKET" --create-bucket-configuration "LocationConstraint=$STATE_REGION" >/dev/null
  fi
  aws s3api put-bucket-tagging --region "$STATE_REGION" --bucket "$BUCKET" --tagging "TagSet=[{Key=$TAG_KEY,Value=$TAG_VALUE}]"
}

# The router binary every router starts from after a reboot (and push stages), with its SHA-256 as
# metadata. The one it replaces stays as releases/router/forkrail-router-<first 16 of its SHA-256>,
# for release.sh's routers-back. Skipped when S3 has this binary already (the second region's push).
upload_router() {
  local sha old
  sha="$(shasum -a 256 "$ROUTER_BIN" | cut -d' ' -f1)"
  old="$(aws s3api head-object --region $STATE_REGION --bucket "$BUCKET" --key forkrail-router \
    --query 'Metadata.sha256' --output text 2>/dev/null || true)"
  if [ "$old" = "$sha" ]; then
    log "the router binary is in S3 already (${sha:0:16})"
    return
  fi
  if [ -z "$old" ] || [ "$old" = None ]; then
    old=""
    # Uploaded before the SHA-256 was kept: hashed from the copy itself. A new install has none.
    if aws s3api head-object --region $STATE_REGION --bucket "$BUCKET" --key forkrail-router >/dev/null 2>&1; then
      old="$(aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/forkrail-router" - | shasum -a 256 | cut -d' ' -f1)"
    fi
  fi
  if [ -n "$old" ]; then
    aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/forkrail-router" "s3://$BUCKET/releases/router/forkrail-router-${old:0:16}" \
      --metadata "sha256=$old" --metadata-directive REPLACE
  fi
  log "uploading the router binary (${sha:0:16})"
  aws s3 --region $STATE_REGION cp --quiet "$ROUTER_BIN" "s3://$BUCKET/forkrail-router" --metadata "sha256=$sha"
}

# deploy_function NAME RUNTIME HANDLER ENVIRONMENT [ZIP]: create or update a worker function
# from ZIP (default: the echo and Node package). ENVIRONMENT is in the CLI's shorthand, e.g.
# Variables={KEY=value}. Packages go through S3, since Python ones pass the 50 MB upload limit.
deploy_function() {
  local name="$1" config=(--runtime "$2" --handler "$3" --environment "$4") zip="${5:-$ZIP}"
  local key="functions/$name.zip"
  aws s3 --region $STATE_REGION cp --quiet "$zip" "s3://$BUCKET/$key"
  if aws lambda get-function --function-name "$name" >/dev/null 2>&1; then
    if ! has_tag "$(function_tag "$name")"; then
      echo "$name exists but isn't tagged $TAG_KEY=$TAG_VALUE; not touching it" >&2
      return 1
    fi
    log "updating $name"
    aws lambda update-function-code --function-name "$name" --s3-bucket "$BUCKET" --s3-key "$key" >/dev/null
    # Until the update lands, new invocations still run the old code.
    aws lambda wait function-updated-v2 --function-name "$name"
    aws lambda update-function-configuration --function-name "$name" "${config[@]}" >/dev/null
  else
    log "creating $name"
    # A new role takes a few seconds before Lambda can assume it.
    local tries=0
    until aws lambda create-function --function-name "$name" "${config[@]}" \
      --architectures arm64 --memory-size 1769 --timeout 900 \
      --role "arn:aws:iam::$ACCOUNT:role/$WORKER_ROLE" \
      --code "S3Bucket=$BUCKET,S3Key=$key" --tags "$TAG_KEY=$TAG_VALUE" >/dev/null; do
      tries=$((tries + 1))
      [ "$tries" -lt 10 ] || return 1
      sleep 3
    done
  fi
  aws lambda wait function-updated-v2 --function-name "$name"
}

deploy_functions() {
  deploy_function "$FUNCTION" provided.al2023 bootstrap "Variables={}"
  # The handler is required, and unused.
  deploy_function "$NODE_FUNCTION" "$NODE_RUNTIME" unused \
    "Variables={AWS_LAMBDA_EXEC_WRAPPER=/var/task/bootstrap,FORKRAIL_APP_DIR=/var/task/app,FORKRAIL_APP_ENTRY=server.mjs}"
  deploy_function "$PYTHON_FUNCTION" "$PYTHON_RUNTIME" unused \
    "Variables={$PYTHON_ENV,FORKRAIL_APP_ENTRY=api/main.py}" "$PYTHON_ZIP"
}

function_tag() {
  aws lambda list-tags --resource "arn:aws:lambda:$AWS_REGION:$ACCOUNT:function:$1" \
    --query "Tags.$TAG_KEY" --output text 2>/dev/null
}

allow() {
  local out
  if ! out="$(aws ec2 authorize-security-group-ingress --group-id "$1" --ip-permissions "$(jq -n \
    --argjson port "$2" --arg cidr "$3" --arg description "$4" \
    '[{IpProtocol: "tcp", FromPort: $port, ToPort: $port, IpRanges: [{CidrIp: $cidr, Description: $description}]}]')" 2>&1)"; then
    grep -q InvalidPermission.Duplicate <<<"$out" || { echo "$out" >&2; return 1; }
  fi
}

ensure_security_group() {
  VPC="$(aws ec2 describe-vpcs --filters Name=is-default,Values=true --query 'Vpcs[0].VpcId' --output text)"
  SG="$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" "Name=vpc-id,Values=$VPC" \
    --query 'SecurityGroups[0].GroupId' --output text)"
  if [ "$SG" = None ]; then
    log "creating security group $NAME"
    SG="$(aws ec2 create-security-group --group-name "$NAME" --vpc-id "$VPC" \
      --description "forkrail test router" \
      --tag-specifications "ResourceType=security-group,Tags=[{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
      --query GroupId --output text)"
    # Lambda outside a VPC has no fixed source range. The pinned certificate and
    # single-use tokens guard this port.
    allow "$SG" 7443 0.0.0.0/0 "tunnel from Lambda workers"
  fi
  # During a restart the second router takes the other tunnel port.
  allow "$SG" 7444 0.0.0.0/0 "tunnel from Lambda workers, second router during restarts"
  # Re-run on every up/start, in case the tester's IP changed.
  allow "$SG" 8080 "$(curl -sf https://checkip.amazonaws.com)/32" "plain HTTP and the default pool, tester only"
  allow "$SG" 443 0.0.0.0/0 "HTTPS"
  allow "$SG" 80 0.0.0.0/0 "HTTP, redirects to HTTPS"
  # Pages between routers (cache/peers.rs): from the other routers' Elastic IPs only. A region's
  # router added later is let in by the next push in each region; one removed stays listed until
  # its rule is revoked by hand.
  local region ip
  for region in $ROUTER_REGIONS; do
    [ "$region" = "$AWS_REGION" ] && continue
    ip="$(router_ip "$region")" || continue
    if [ -n "$ip" ]; then allow "$SG" "$PEER_PORT" "$ip/32" "pages from the router in $region"; fi
  done
}

eip_allocation() {
  aws ec2 describe-addresses --filters "Name=tag:Name,Values=$EIP_NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
    --query 'Addresses[0].AllocationId' --output text
}

# The router's Elastic IP, allocated once and kept across stop/start, so DNS can point at it. It costs
# what the automatic public IPv4 did ($0.005 an hour), which AWS releases once this one is attached.
# Sets EIP_MOVED when it was just associated: the router must restart so workers dial the new address.
ensure_eip() {
  local alloc instance attached
  instance="$(instance_id)"
  alloc="$(eip_allocation)"
  if [ "$alloc" = None ]; then
    log "allocating Elastic IP $EIP_NAME"
    alloc="$(aws ec2 allocate-address --domain vpc \
      --tag-specifications "ResourceType=elastic-ip,Tags=[{Key=Name,Value=$EIP_NAME},{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
      --query AllocationId --output text)"
  fi
  attached="$(aws ec2 describe-addresses --allocation-ids "$alloc" --query 'Addresses[0].InstanceId' --output text)"
  EIP_MOVED=""
  if [ "$attached" != "$instance" ]; then
    log "associating $EIP_NAME with $instance"
    aws ec2 associate-address --allocation-id "$alloc" --instance-id "$instance" >/dev/null
    EIP_MOVED=1
  fi
}

eip_address() {
  aws ec2 describe-addresses --allocation-ids "$(eip_allocation)" --query 'Addresses[0].PublicIp' --output text
}

# The router's systemd unit, as router-init.sh writes it at launch. `push` and `domain` write it
# again, so older instances get its changes. A template: a restart starts forkrail-router@blue or
# @green beside the other, which stops once the new one is ready (see ROUTER_RESTART).
router_unit() {
  cat <<UNIT
[Unit]
Description=forkrail router (%i)
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
# Ready once every listener is up, HTTPS included; only then does a restart stop the old router.
Type=notify
NotifyAccess=main
TimeoutStartSec=180
# Stopping closes the listeners at once, then lets requests in flight finish (up to 30 s).
TimeoutStopSec=45
User=forkrail
# Two routers at once each take one of the tunnel ports; the log goes to hourly files.
Environment=FORKRAIL_TUNNEL_LISTEN=0.0.0.0:7443,0.0.0.0:7444
Environment=FORKRAIL_LOG_DIR=/var/log/forkrail
# Buffers of a megabyte or more (batches of app logs for ClickHouse) go back to the system when
# freed: without this glibc keeps them, and a backlog took the router from 140 to 560 MB.
Environment=MALLOC_MMAP_THRESHOLD_=1048576
LogsDirectory=forkrail
# Large tables of bulk redirects, kept on disk and mapped rather than held in memory (statics.rs),
# the verified crawlers' published ranges (bots.rs), and pages (cache/disk.rs).
StateDirectory=forkrail/redirects forkrail/bots forkrail/pages forkrail/keepwarm
EnvironmentFile=/etc/forkrail/router.env
# The binary push staged goes in place; without one (after a reboot, say), the latest build comes from S3.
ExecStartPre=+/bin/sh -c 'if [ -f /opt/forkrail/forkrail-router.new ]; then mv -f /opt/forkrail/forkrail-router.new /opt/forkrail/forkrail-router; else /usr/bin/aws s3 --region $STATE_REGION cp --quiet s3://$BUCKET/forkrail-router /opt/forkrail/forkrail-router; fi; chmod 755 /opt/forkrail/forkrail-router'
ExecStart=/opt/forkrail/start.sh
# Ports 80 and 443, as the forkrail user.
AmbientCapabilities=CAP_NET_BIND_SERVICE
# The last process the kernel kills when memory runs out, after tools run on the instance.
OOMScoreAdjust=-900
Restart=always
RestartSec=2
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT
}

# Connections queued on a router that stops are handed to the one beside it, not reset.
INSTALL_UNIT="cat > /etc/systemd/system/forkrail-router@.service <<'UNITFILE'
$(router_unit)
UNITFILE
echo 'net.ipv4.tcp_migrate_req = 1' > /etc/sysctl.d/90-forkrail.conf
sysctl -q -p /etc/sysctl.d/90-forkrail.conf
$(cat "$ROOT/deploy/aws/tcp-tuning.sh")
systemctl daemon-reload"

# Instance shell lines that restart the router with no gap: start the other of @blue and @green,
# which binds the public ports beside the running one and the tunnel port it doesn't hold; once it
# tells systemd it's ready, stop the old one, which closes its listeners at once and drains. If the
# new one fails to start, the old one keeps serving. An instance still on the single unit from
# before goes through one last restart with a gap.
ROUTER_RESTART='active=""
for color in blue green; do systemctl is-active --quiet forkrail-router@$color && active=$color; done
if [ -z "$active" ] && [ -f /etc/systemd/system/forkrail-router.service ]; then
  echo "router: moving from forkrail-router.service to forkrail-router@blue (one restart with a gap)"
  systemctl disable --quiet forkrail-router 2>/dev/null || true
  systemctl stop forkrail-router || true
  rm -f /etc/systemd/system/forkrail-router.service
  systemctl daemon-reload
fi
next=blue; [ "$active" = blue ] && next=green
started=$(date +%s%N)
if ! systemctl start forkrail-router@$next; then
  echo "router: forkrail-router@$next did not get ready; ${active:-nothing} keeps serving" >&2
  journalctl -u forkrail-router@$next --no-pager -n 30 >&2
  systemctl stop forkrail-router@$next 2>/dev/null || true
  exit 1
fi
ready_ms=$(( ($(date +%s%N) - started) / 1000000 ))
systemctl enable --quiet forkrail-router@$next
if [ -n "$active" ]; then
  systemctl disable --quiet forkrail-router@$active
  systemctl stop forkrail-router@$active
fi
echo "router: forkrail-router@$next serving, ready after ${ready_ms} ms${active:+; @$active stopped and drained}"'

# Shell lines for the router instance that install the log export: the scripts, and a timer that
# runs them every 10 minutes (see log-export.sh).
install_export() {
  cat <<EXPORT
$(to_router "$ROOT/deploy/aws/log-export.sh")
$(to_router "$ROOT/deploy/aws/log-export.py")
chmod 755 /opt/forkrail/log-export.sh
cat > /etc/systemd/system/forkrail-log-export.service <<'UNITFILE'
[Unit]
Description=forkrail: router log to S3, request metrics to CloudWatch

[Service]
Type=oneshot
OOMScoreAdjust=500
# As the router's own user, which owns the log folder: run as root, a link the router left
# there would have root write wherever it pointed.
User=forkrail
StateDirectory=forkrail-log-export
Environment=HOME=/var/lib/forkrail-log-export
# The bucket and the 5xx alarm are in the state region, whichever region this router is in.
Environment=AWS_REGION=$STATE_REGION
ExecStart=/opt/forkrail/log-export.sh $BUCKET
UNITFILE
cat > /etc/systemd/system/forkrail-log-export.timer <<'UNITFILE'
[Unit]
Description=forkrail log export every 10 minutes

[Timer]
OnCalendar=*:0/10
Persistent=true
AccuracySec=10s

[Install]
WantedBy=timers.target
UNITFILE
systemctl daemon-reload
systemctl enable --now forkrail-log-export.timer >/dev/null 2>&1
EXPORT
}

# Shell lines for the router instance that install or update Vector, which ships the router's log
# files to the analytics box (deploy/aws/analytics.sh): to its private address over HTTP in the
# box's region, else over HTTPS with the box's certificate pinned. Prints nothing (and the caller
# skips it) until the box exists.
install_vector() {
  local endpoint url tls config
  endpoint="$(aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/state/analytics/endpoint.json" - 2>/dev/null)" || return 0
  if [ "$AWS_REGION" = "$(jq -r .region <<<"$endpoint")" ]; then
    url="http://$(jq -r .private_ip <<<"$endpoint"):$(jq -r .http_port <<<"$endpoint")"
    tls=""
  else
    url="https://$(jq -r .public_ip <<<"$endpoint"):$(jq -r .https_port <<<"$endpoint")"
    tls='    tls:\n      ca_file: /etc/vector/clickhouse-ca.pem\n      verify_hostname: false'
  fi
  config="$(mktemp -d)/vector-$AWS_REGION.yaml"
  awk -v tls="$tls" '$0 == "#TLS#" { if (tls != "") print tls; next } { print }' "$ROOT/deploy/aws/vector/vector.yaml" > "$config"
  if ! aws s3 --region $STATE_REGION ls "s3://$BUCKET/tools/$VECTOR_TARBALL" >/dev/null 2>&1; then
    echo "s3://$BUCKET/tools/$VECTOR_TARBALL is missing: download it from Vector's release page, check it against VECTOR_SHA256, and upload it" >&2
    return 1
  fi
  cat <<VECTOR
if ! /opt/vector/bin/vector --version 2>/dev/null | grep -q '^vector $VECTOR_VERSION '; then
  aws s3 --region $STATE_REGION cp --quiet s3://$BUCKET/tools/$VECTOR_TARBALL /tmp/vector.tar.gz
  echo '$VECTOR_SHA256  /tmp/vector.tar.gz' | sha256sum -c --quiet
  rm -rf /opt/vector.new && mkdir -p /opt/vector.new
  tar xzf /tmp/vector.tar.gz -C /opt/vector.new --strip-components=2
  rm -rf /opt/vector && mv /opt/vector.new /opt/vector && rm -f /tmp/vector.tar.gz
fi
mkdir -p /etc/vector
$(to_router "$config")
mv /opt/forkrail/$(basename "$config") /etc/vector/vector.yaml
chmod 644 /etc/vector/vector.yaml
${tls:+aws s3 --region $STATE_REGION cp --quiet s3://$BUCKET/state/analytics/ca.pem /etc/vector/clickhouse-ca.pem && chmod 644 /etc/vector/clickhouse-ca.pem}
imds=\$(curl -sf -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
instance=\$(curl -sf -H "X-aws-ec2-metadata-token: \$imds" http://169.254.169.254/latest/meta-data/instance-id)
sed -i -e "s|@REGION@|$AWS_REGION|" -e "s|@INSTANCE@|\$instance|" -e "s|@ENDPOINT@|$url|" /etc/vector/vector.yaml
(umask 077; aws ssm get-parameter --region $STATE_REGION --name $CLICKHOUSE_INGEST_PARAM --with-decryption --query Parameter.Value --output text > /etc/vector/ch_password)
rm -f /etc/vector/vector.env
cat > /etc/systemd/system/forkrail-vector.service <<'UNITFILE'
[Unit]
Description=forkrail: the router's log to ClickHouse (Vector)
After=network-online.target
Wants=network-online.target

[Service]
DynamicUser=yes
StateDirectory=vector
# The password, readable by this service alone at /run/credentials/forkrail-vector.service/.
LoadCredential=ch_password:/etc/vector/ch_password
Environment=VECTOR_LOG=warn
ExecStartPre=/opt/vector/bin/vector validate --no-environment --config-yaml /etc/vector/vector.yaml
ExecStart=/opt/vector/bin/vector --config-yaml /etc/vector/vector.yaml
ExecReload=/bin/kill -HUP \$MAINPID
# Killed well before the router when memory runs out, and never more than this.
OOMScoreAdjust=500
MemoryMax=400M
Nice=5
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNITFILE
systemctl daemon-reload
systemctl enable --quiet forkrail-vector
systemctl restart forkrail-vector
sleep 3
systemctl is-active forkrail-vector
VECTOR
}

# Shell lines for the router instance that point its log sink at the analytics box: app log lines
# and outgoing calls then go from the router straight to ClickHouse, not through its log file and
# Vector (crates/router/src/sink.rs). The same address and password as Vector's. DIRECT_LOGS=0
# takes the setting away again, and lines go back to the file.
install_sink() {
  local endpoint url ca=""
  endpoint="$(aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/state/analytics/endpoint.json" - 2>/dev/null)" || return 0
  if [ "${DIRECT_LOGS:-1}" != 1 ]; then
    echo "sed -i -e '/^FORKRAIL_CLICKHOUSE_URL=/d' -e '/^FORKRAIL_CLICKHOUSE_CA=/d' /etc/forkrail/router.env"
    return 0
  fi
  if [ "$AWS_REGION" = "$(jq -r .region <<<"$endpoint")" ]; then
    url="http://$(jq -r .private_ip <<<"$endpoint"):$(jq -r .http_port <<<"$endpoint")"
  else
    url="https://$(jq -r .public_ip <<<"$endpoint"):$(jq -r .https_port <<<"$endpoint")"
    ca=/etc/forkrail/clickhouse-ca.pem
  fi
  cat <<SINK
(umask 077; aws ssm get-parameter --region $STATE_REGION --name $CLICKHOUSE_INGEST_PARAM --with-decryption --query Parameter.Value --output text > /etc/forkrail/ch_password.new)
chown forkrail:forkrail /etc/forkrail/ch_password.new && mv -f /etc/forkrail/ch_password.new /etc/forkrail/ch_password
${ca:+aws s3 --region $STATE_REGION cp --quiet s3://$BUCKET/state/analytics/ca.pem $ca && chmod 644 $ca}
imds=\$(curl -sf -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
instance=\$(curl -sf -H "X-aws-ec2-metadata-token: \$imds" http://169.254.169.254/latest/meta-data/instance-id)
sed -i -e '/^FORKRAIL_CLICKHOUSE_URL=/d' -e '/^FORKRAIL_CLICKHOUSE_CA=/d' -e '/^FORKRAIL_ROUTER_ID=/d' /etc/forkrail/router.env
echo "FORKRAIL_ROUTER_ID=\$instance" >> /etc/forkrail/router.env
echo 'FORKRAIL_CLICKHOUSE_URL=$url' >> /etc/forkrail/router.env
${ca:+echo 'FORKRAIL_CLICKHOUSE_CA=$ca' >> /etc/forkrail/router.env}
SINK
}

# GeoIP: DB-IP's free City and ASN databases ("IP Geolocation by DB-IP", CC BY 4.0), for each
# client's location in the router's log and the app's x-vercel-ip-* headers. A daily timer on the
# instance fetches a new month's files once DB-IP publishes them (on the 1st), checks them, and
# renames each over the old one, which the router, mapping the old file, can keep reading; it
# reopens the new ones within the hour.
install_geoip() {
  cat <<'GEOIP'
mkdir -p /var/lib/forkrail/geoip && chmod 755 /var/lib/forkrail /var/lib/forkrail/geoip
cat > /usr/local/bin/forkrail-geoip-update <<'SCRIPT'
#!/bin/bash
# This month's DB-IP City and ASN Lite databases into /var/lib/forkrail/geoip, or last month's
# early in a month, before DB-IP publishes. Does nothing when the newest is already there.
set -euo pipefail
dir=/var/lib/forkrail/geoip
this=$(date -u +%Y-%m)
last=$(date -u -d "$(date -u +%Y-%m-01) -1 day" +%Y-%m)
have=$(cat "$dir/.month" 2>/dev/null || true)
for month in "$this" "$last"; do
  [ "$have" = "$month" ] && exit 0
  declare -A staged=()
  for db in city asn; do
    tmp=$(mktemp "$dir/.$db.XXXXXX")
    if curl -sf --max-time 900 "https://download.db-ip.com/free/dbip-$db-lite-$month.mmdb.gz" | gunzip > "$tmp" \
        && [ "$(stat -c %s "$tmp")" -gt 1000000 ] && tail -c 100000 "$tmp" | grep -qa 'MaxMind.com'; then
      chmod 644 "$tmp"
      staged[$db]=$tmp
    else
      rm -f "$tmp"
      for f in "${staged[@]}"; do rm -f "$f"; done
      continue 2
    fi
  done
  for db in city asn; do mv -f "${staged[$db]}" "$dir/$db.mmdb"; done
  echo "$month" > "$dir/.month"
  echo "GeoIP databases for $month installed"
  exit 0
done
echo "GeoIP: no databases for $this or $last could be fetched" >&2
exit 1
SCRIPT
chmod 755 /usr/local/bin/forkrail-geoip-update
cat > /etc/systemd/system/forkrail-geoip.service <<'UNITFILE'
[Unit]
Description=forkrail: this month's GeoIP databases (DB-IP Lite)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/forkrail-geoip-update
OOMScoreAdjust=1000
Nice=10
UNITFILE
cat > /etc/systemd/system/forkrail-geoip.timer <<'UNITFILE'
[Unit]
Description=forkrail: check for new GeoIP databases daily

[Timer]
OnCalendar=*-*-* 04:00:00
RandomizedDelaySec=1h
Persistent=true

[Install]
WantedBy=timers.target
UNITFILE
systemctl daemon-reload
systemctl enable --quiet --now forkrail-geoip.timer
/usr/local/bin/forkrail-geoip-update
ls -la /var/lib/forkrail/geoip | grep mmdb
GEOIP
}

ALERTS_TOPIC=forkrail-alerts
# 5xx on the public listener in one 10-minute export that raise the alarm.
ALARM_5XX=20
ALARM_ATTACK=10000

# alerts_topic_policy ARN: the alerts topic's policy, in place of SNS's default one (which lets any
# principal in the account publish, subscribe and change it). The account's CloudWatch alarms
# publish; administrators manage it and its subscriptions by their own IAM policies; the roles that
# run code forkrail didn't write (apps, builds, page previews) are refused everything.
alerts_topic_policy() {
  local account partition
  account="$(cut -d: -f5 <<<"$1")"; partition="$(cut -d: -f2 <<<"$1")"
  jq -nc --arg topic "$1" --arg account "$account" --arg p "$partition" '{
    Version: "2012-10-17",
    Statement: [
      {Sid: "AlarmsPublish", Effect: "Allow", Principal: {Service: "cloudwatch.amazonaws.com"}, Action: "sns:Publish",
       Resource: $topic, Condition: {StringEquals: {"aws:SourceAccount": $account},
         ArnLike: {"aws:SourceArn": ("arn:" + $p + ":cloudwatch:*:" + $account + ":alarm:*")}}},
      {Sid: "NeverCodeForkrailDidntWrite", Effect: "Deny", Principal: "*", Resource: $topic,
       # SNS refuses the `sns:*` wildcard in a topic policy: every topic action is named.
       Action: ["sns:Publish", "sns:Subscribe", "sns:GetTopicAttributes", "sns:SetTopicAttributes", "sns:AddPermission",
         "sns:RemovePermission", "sns:DeleteTopic", "sns:ListSubscriptionsByTopic", "sns:GetDataProtectionPolicy",
         "sns:PutDataProtectionPolicy", "sns:TagResource", "sns:UntagResource", "sns:ListTagsForResource"],
       Condition: {ArnLike: {"aws:PrincipalArn": [
         ("arn:" + $p + ":iam::" + $account + ":role/forkrail-worker"),
         ("arn:" + $p + ":iam::" + $account + ":role/forkrail-builder"),
         ("arn:" + $p + ":iam::" + $account + ":role/forkrail-screenshot")]}}}
    ]}'
}

# sns_default_policy ARN: the policy SNS gives a topic it makes, to undo alerts_topic_policy.
sns_default_policy() {
  jq -nc --arg topic "$1" --arg account "$(cut -d: -f5 <<<"$1")" '{
    Version: "2008-10-17", Id: "__default_policy_ID",
    Statement: [{Sid: "__default_statement_ID", Effect: "Allow", Principal: {AWS: "*"},
      Action: ["SNS:GetTopicAttributes", "SNS:SetTopicAttributes", "SNS:AddPermission", "SNS:RemovePermission",
        "SNS:DeleteTopic", "SNS:Subscribe", "SNS:ListSubscriptionsByTopic", "SNS:Publish"],
      Resource: $topic, Condition: {StringEquals: {"AWS:SourceOwner": $account}}}]}'
}

# ensure_alerts_topic EMAIL: the SNS topic, with its policy and EMAIL subscribed; prints the topic ARN.
ensure_alerts_topic() {
  local arn subscribed
  arn="$(aws sns --region "$STATE_REGION" create-topic --name "$ALERTS_TOPIC" --tags "Key=$TAG_KEY,Value=$TAG_VALUE" --query TopicArn --output text)"
  has_tag "$(aws sns --region "$STATE_REGION" list-tags-for-resource --resource-arn "$arn" --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)" ||
    { echo "SNS topic $ALERTS_TOPIC isn't tagged $TAG_KEY=$TAG_VALUE" >&2; return 2; }
  aws sns --region "$STATE_REGION" set-topic-attributes --topic-arn "$arn" --attribute-name Policy --attribute-value "$(alerts_topic_policy "$arn")"
  subscribed="$(aws sns --region "$STATE_REGION" list-subscriptions-by-topic --topic-arn "$arn" \
    --query "Subscriptions[?Protocol=='email' && Endpoint=='$1'] | length(@)" --output text)"
  if [ "$subscribed" = 0 ]; then
    aws sns --region "$STATE_REGION" subscribe --topic-arn "$arn" --protocol email --notification-endpoint "$1" >/dev/null
    log "subscribed $1 to $ALERTS_TOPIC: confirm it from the mail AWS sends"
  fi
  echo "$arn"
}

# ensure_health_check HOST: a Route 53 HTTPS health check of HOST's /_forkrail/health, which the
# router answers itself; prints its id. From all of Route 53's regions, every 30 s, unhealthy after
# 3 failures, with connect and first-byte times recorded.
ensure_health_check() {
  local host="$1" id
  id="$(aws route53 list-health-checks --query "HealthChecks[?HealthCheckConfig.FullyQualifiedDomainName=='$host' \
    && HealthCheckConfig.ResourcePath=='/_forkrail/health'].Id | [0]" --output text)"
  if [ "$id" = None ]; then
    id="$(aws route53 create-health-check --caller-reference "forkrail-$host-$(date +%s)" --health-check-config "$(jq -nc \
      --arg host "$host" '{Type: "HTTPS", FullyQualifiedDomainName: $host, Port: 443, ResourcePath: "/_forkrail/health",
        RequestInterval: 30, FailureThreshold: 3, MeasureLatency: true, EnableSNI: true}')" \
      --query HealthCheck.Id --output text)"
    aws route53 change-tags-for-resource --resource-type healthcheck --resource-id "$id" \
      --add-tags "Key=$TAG_KEY,Value=$TAG_VALUE" "Key=Name,Value=forkrail-$host"
    log "health check $id: https://$host/_forkrail/health"
  fi
  has_tag "$(aws route53 list-tags-for-resource --resource-type healthcheck --resource-id "$id" \
    --query "ResourceTagSet.Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)" ||
    { echo "health check $id for $host isn't tagged $TAG_KEY=$TAG_VALUE" >&2; return 2; }
  echo "$id"
}

# env_edits KEY=VALUE...: shell lines that set each in the router's environment file.
env_edits() {
  local pair key
  for pair in "$@"; do
    [[ "$pair" =~ ^[A-Z_]+=[A-Za-z0-9_./:=,-]+$ ]] || { echo "bad KEY=VALUE: $pair" >&2; return 2; }
    key="${pair%%=*}"
    echo "sed -i '/^$key=/d' /etc/forkrail/router.env && echo '$pair' >> /etc/forkrail/router.env"
  done
}

# cloudflare METHOD PATH [JSON]: a Cloudflare API call. The token goes to curl on stdin, never on a
# command line, and is never printed.
cloudflare() {
  printf 'header = "Authorization: Bearer %s"\n' "$CF_TOKEN" |
    curl -sS -K - -X "$1" -H 'Content-Type: application/json' ${3:+--data "$3"} "https://api.cloudflare.com/client/v4$2"
}

# cloudflare_wildcard ZONE BASE IP: *.BASE A IP, DNS only (no Cloudflare proxy), TTL 60 so a move to
# Global Accelerator later takes effect quickly. The apex and www stay free for the site.
cloudflare_wildcard() {
  local zone="$1" name="*.$2" ip="$3" existing body out
  CF_TOKEN="$(aws ssm get-parameter --region "$STATE_REGION" --name "$CLOUDFLARE_TOKEN_PARAM" --with-decryption --query Parameter.Value --output text)"
  out="$(cloudflare GET "/zones/$zone/dns_records?type=A&name=$(jq -rn --arg n "$name" '$n|@uri')")"
  jq -e .success <<<"$out" >/dev/null || { echo "Cloudflare: $(jq -c .errors <<<"$out")" >&2; unset CF_TOKEN; return 1; }
  # Once it exists, it may point at the accelerator: moving it is `wildcard`'s job.
  if [ "$(jq '.result | length' <<<"$out")" != 0 ]; then
    unset CF_TOKEN
    log "DNS: $name A $(jq -r '[.result[].content] | join(", ")' <<<"$out"), left as it is (see wildcard)"
    return
  fi
  body="$(jq -nc --arg name "$name" --arg ip "$ip" \
    '{type: "A", name: $name, content: $ip, ttl: 60, proxied: false, comment: "forkrail router (test-env.sh domain)"}')"
  out="$(cloudflare POST "/zones/$zone/dns_records" "$body")"
  unset CF_TOKEN
  jq -e .success <<<"$out" >/dev/null || { echo "Cloudflare: $(jq -c .errors <<<"$out")" >&2; return 1; }
  log "DNS: $name A $ip, DNS only, TTL 60"
}

# cloudflare_a_set ZONE NAME IP...: NAME's A records exactly IP..., DNS only, TTL 60. New ones are
# added before old ones go, so the name always resolves. No IPs removes them all. Prints what it was.
cloudflare_a_set() {
  local zone="$1" name="$2" out ip id content keep body
  shift 2
  CF_TOKEN="$(aws ssm get-parameter --region "$STATE_REGION" --name "$CLOUDFLARE_TOKEN_PARAM" --with-decryption --query Parameter.Value --output text)"
  out="$(cloudflare GET "/zones/$zone/dns_records?type=A&name=$(jq -rn --arg n "$name" '$n|@uri')")"
  jq -e .success <<<"$out" >/dev/null || { echo "Cloudflare: $(jq -c .errors <<<"$out")" >&2; unset CF_TOKEN; return 1; }
  log "$name was: A $(jq -r '[.result[].content] | join(", ")' <<<"$out")"
  for ip in "$@"; do
    jq -e --arg ip "$ip" 'any(.result[]; .content == $ip)' <<<"$out" >/dev/null && continue
    body="$(jq -nc --arg name "$name" --arg ip "$ip" \
      '{type: "A", name: $name, content: $ip, ttl: 60, proxied: false, comment: "forkrail (test-env.sh)"}')"
    jq -e .success <<<"$(cloudflare POST "/zones/$zone/dns_records" "$body")" >/dev/null ||
      { unset CF_TOKEN; echo "Cloudflare: adding $ip failed" >&2; return 1; }
  done
  while read -r id content; do
    [ -n "$id" ] || continue
    keep=0
    for ip in "$@"; do [ "$ip" = "$content" ] && keep=1; done
    [ "$keep" = 1 ] && continue
    jq -e .success <<<"$(cloudflare DELETE "/zones/$zone/dns_records/$id")" >/dev/null ||
      { unset CF_TOKEN; echo "Cloudflare: removing $content failed" >&2; return 1; }
  done < <(jq -r '.result[] | "\(.id) \(.content)"' <<<"$out")
  unset CF_TOKEN
  log "$name is now: A ${*:-(none)}, DNS only, TTL 60. Resolvers that cached the old answer keep it up to its TTL"
}

# run_to_file COMMAND OUT: COMMAND's stdout on the router, in OUT here. Through the artifact bucket,
# since SSM keeps only the first 24,000 characters of a command's output.
run_to_file() {
  local key
  key="results/$(date -u +%Y%m%dT%H%M%S)-$RANDOM.out.gz"
  # A file of its own on the instance, so runs from several shells at once don't mix their output.
  local tmp="/tmp/forkrail-out-${key##*/}"
  run "set -e
($1) | gzip > $tmp
aws s3 --region $STATE_REGION cp --quiet $tmp s3://$BUCKET/$key
rm -f $tmp" >/dev/null
  aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/$key" - | gunzip > "$2"
  aws s3 --region $STATE_REGION rm --quiet "s3://$BUCKET/$key"
}

instance_id() {
  aws ec2 describe-instances \
    --filters "Name=tag:Name,Values=$NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
    Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId' --output text
}

public_ip() {
  aws ec2 describe-instances --instance-ids "$(instance_id)" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' --output text
}

ensure_instance() {
  INSTANCE="$(instance_id)"
  if [ -n "$INSTANCE" ]; then return; fi
  log "launching a $INSTANCE_TYPE router"
  local ami zone subnet user_data tries=0
  ami="$(aws ssm get-parameter --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64 \
    --query Parameter.Value --output text)"
  # Not every zone offers every instance type.
  zone="$(aws ec2 describe-instance-type-offerings --location-type availability-zone \
    --filters "Name=instance-type,Values=$INSTANCE_TYPE" --query 'sort(InstanceTypeOfferings[].Location)[0]' --output text)"
  subnet="$(aws ec2 describe-subnets \
    --filters "Name=vpc-id,Values=$VPC" "Name=availability-zone,Values=$zone" Name=default-for-az,Values=true \
    --query 'Subnets[0].SubnetId' --output text)"
  user_data="$(sed -e "s|@BUCKET@|$BUCKET|g" -e "s|@FUNCTION@|$FUNCTION|g" -e "s|@REGION@|$AWS_REGION|g" \
    -e "s|@STATE_REGION@|$STATE_REGION|g" \
    -e "/^@TCP_TUNING@\$/r $ROOT/deploy/aws/tcp-tuning.sh" -e "/^@TCP_TUNING@\$/d" \
    "$ROOT/deploy/aws/router-init.sh")"
  # A new instance profile takes a few seconds before EC2 accepts it.
  until INSTANCE="$(aws ec2 run-instances --image-id "$ami" --instance-type "$INSTANCE_TYPE" \
    --subnet-id "$subnet" --security-group-ids "$SG" --iam-instance-profile "Name=$NAME" \
    --metadata-options HttpTokens=required,HttpEndpoint=enabled \
    --user-data "$user_data" \
    --tag-specifications \
    "ResourceType=instance,Tags=[{Key=Name,Value=$NAME},{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
    "ResourceType=volume,Tags=[{Key=Name,Value=$NAME},{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
    --query 'Instances[0].InstanceId' --output text)"; do
    tries=$((tries + 1))
    [ "$tries" -lt 10 ] || return 1
    sleep 3
  done
  aws ec2 wait instance-running --instance-ids "$INSTANCE"
}

wait_for_router() {
  local ip
  ip="$(public_ip)"
  log "waiting for the router on $ip:8080"
  until nc -z -w 2 "$ip" 8080 2>/dev/null; do sleep 3; done
  log "router up: http://$ip:8080"
}

run() {
  local instance id
  instance="$(instance_id)"
  # SSM's agent runs commands with its own oom_score_adj of -900, which its children inherit, so
  # under memory pressure the kernel would kill the router before them: on 2026-11-06 a 1.6 GB
  # journal export took the router down for 18 minutes. Make them the first to go instead.
  id="$(aws ssm send-command --instance-ids "$instance" --document-name AWS-RunShellScript \
    --parameters "$(jq -n --arg command "echo 1000 > /proc/self/oom_score_adj
$1" '{commands: [$command]}')" \
    --query Command.CommandId --output text)"
  # Poll rather than use `aws ssm wait`, which gives up after 100 s; load tests run longer.
  # SSM's own limit is an hour per command.
  local status=Pending
  # Right after sending, SSM may not know the command yet: a few failed reads are waited out, but
  # not an expired session, which would otherwise poll for ever.
  local unread=0
  while [[ "$status" =~ ^(Pending|InProgress|Delayed)$ ]]; do
    sleep 2
    if status="$(aws ssm get-command-invocation --command-id "$id" --instance-id "$instance" \
      --query Status --output text 2>&1)"; then
      unread=0
    else
      unread=$((unread + 1))
      [ "$unread" -lt 15 ] || { echo "can't read command $id's status: $status" >&2; exit 1; }
      status=Pending
    fi
  done
  aws ssm get-command-invocation --command-id "$id" --instance-id "$instance" \
    --query '[StandardOutputContent, StandardErrorContent]' --output text
}

has_tag() { [ "$1" = "$TAG_VALUE" ]; }

# apart NAME MESSAGE LINES: instance shell lines that run LINES as a script of their own with
# bash -e, and print MESSAGE if it fails, without failing the command around them.
apart() {
  cat <<APART
apart=\$(mktemp /tmp/forkrail-$1.XXXXXX)
cat > "\$apart" <<'FORKRAIL_APART_END'
$3
FORKRAIL_APART_END
bash -e "\$apart" || echo '$2' >&2
rm -f "\$apart"
APART
}

# tagged_function NAME: forkrail-NAME, if it carries the project tag.
tagged_function() {
  local fn="forkrail-$1"
  has_tag "$(function_tag "$fn")" || { echo "no function $fn tagged $TAG_KEY=$TAG_VALUE" >&2; return 2; }
  echo "$fn"
}

# Copy a local file to the router instance through the artifact bucket, as /opt/forkrail/NAME.
to_router() {
  aws s3 --region $STATE_REGION cp --quiet "$1" "s3://$BUCKET/tools/$(basename "$1")"
  echo "aws s3 --region $STATE_REGION cp --quiet s3://$BUCKET/tools/$(basename "$1") /opt/forkrail/$(basename "$1")"
}

# A router outside STATE_REGION: HTTPS for the base domain with the certificates the state
# region's router issues and stores (no Cloudflare zone here, so it never issues), no default pool,
# the state read from STATE_REGION, and the log export.
regional_router() {
  local base edits
  base="$(aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/state/settings.json" - | jq -r '.base_domain // empty')"
  [ -n "$base" ] || { echo "no base domain in state/settings.json: run domain in $STATE_REGION first" >&2; return 2; }
  edits="$(env_edits "FORKRAIL_BASE_DOMAIN=$base" FORKRAIL_TLS_LISTEN=0.0.0.0:443 FORKRAIL_REDIRECT_LISTEN=0.0.0.0:80 \
    "FORKRAIL_TLS_CERTS=s3://$BUCKET/state/certs" FORKRAIL_DEFAULT_POOL=0 \
    "FORKRAIL_STATE_REGION=$STATE_REGION" "FORKRAIL_HOME_REGION=$STATE_REGION")"
  log "the router in $AWS_REGION: https://<project>.$base with the certificates from $STATE_REGION"
  run "set -e
$INSTALL_UNIT
$edits
sed -i '/^FORKRAIL_CLOUDFLARE_ZONE=/d' /etc/forkrail/router.env
$(install_export)
cp /opt/forkrail/forkrail-router /opt/forkrail/forkrail-router.new
$ROUTER_RESTART
grep -E '^(AWS_REGION|FORKRAIL_)' /etc/forkrail/router.env"
}

# The install key its secrets are made under (deploy/install/install-secrets.yaml): applied once if
# this install doesn't have it yet.
ensure_install_key() {
  if aws kms describe-key --region "$STATE_REGION" --key-id "$INSTALL_KEY" >/dev/null 2>&1; then return; fi
  log "making the install key ($INSTALL_KEY)"
  AWS_REGION="$STATE_REGION" "$ROOT/deploy/aws/stack.sh" apply install-secrets --yes
}

# The firewall's challenge key, shared by every router: made once (32 random bytes as hex, piped
# straight into SSM, never printed), never replaced here (a new key ends every pass in force).
ensure_challenge_key() {
  if aws ssm --region "$STATE_REGION" get-parameter --name "$CHALLENGE_KEY_PARAM" --query Parameter.Name --output text >/dev/null 2>&1; then
    return
  fi
  ensure_install_key
  log "making the firewall's challenge key ($CHALLENGE_KEY_PARAM)"
  openssl rand -hex 32 | tr -d '\n' | aws ssm --region "$STATE_REGION" put-parameter --name "$CHALLENGE_KEY_PARAM" --type SecureString \
    --key-id "$INSTALL_KEY" --value file:///dev/stdin --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null
}

# The routers' key for their link (pages between them, crates/router/src/cache/peers.rs), made
# once like the challenge key. A new key cuts the links until every router has restarted with it.
ensure_peer_key() {
  if aws ssm --region "$STATE_REGION" get-parameter --name "$PEER_KEY_PARAM" --query Parameter.Name --output text >/dev/null 2>&1; then
    return
  fi
  ensure_install_key
  log "making the routers' peer key ($PEER_KEY_PARAM)"
  openssl rand -hex 32 | tr -d '\n' | aws ssm --region "$STATE_REGION" put-parameter --name "$PEER_KEY_PARAM" --type SecureString \
    --key-id "$INSTALL_KEY" --value file:///dev/stdin --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null
}

# The Elastic IP of the router in region $1, or nothing if it has none.
router_ip() {
  local ip
  ip="$(aws ec2 describe-addresses --region "$1" --filters "Name=tag:Name,Values=$EIP_NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
    --query 'Addresses[0].PublicIp' --output text 2>/dev/null)"
  [ "$ip" != None ] && echo "$ip"
}

# FORKRAIL_PEERS for the routers in ROUTER_REGIONS that have an address: region=ip:port, comma-separated.
peers_spec() {
  local region ip spec=""
  for region in $ROUTER_REGIONS; do
    ip="$(router_ip "$region")" || continue
    if [ -n "$ip" ]; then spec="${spec:+$spec,}$region=$ip:$PEER_PORT"; fi
  done
  echo "$spec"
}

# Requests and connections the routers' system mitigations refused (`MitigatedRequests`, from
# log-export.sh every 10 minutes): ALARM_ATTACK or more in 10 minutes is an attack worth a look.
attack_alarm() {
  aws cloudwatch --region "$STATE_REGION" put-metric-alarm --alarm-name forkrail-attack \
    --alarm-description "The routers' system mitigations refused $ALARM_ATTACK or more requests and connections in 10 minutes: see the mitigation lines in the router's log (blocked addresses) and forkrail firewall mitigations --install" \
    --namespace forkrail --metric-name MitigatedRequests --statistic Sum --period 600 --evaluation-periods 1 \
    --threshold "$ALARM_ATTACK" --comparison-operator GreaterThanOrEqualToThreshold --treat-missing-data notBreaching \
    --alarm-actions "$1" --ok-actions "$1" --tags "Key=$TAG_KEY,Value=$TAG_VALUE"
}

# The soonest certificate to expire, as the issuing router logs it every hour (`certificates`) and
# log-export.sh sends it (`CertificateDaysLeft`): an alarm under 14 days, or after 6 hours without a
# word from the issuer.
expiry_alarm() {
  aws cloudwatch --region "$STATE_REGION" put-metric-alarm --alarm-name forkrail-cert-expiry \
    --alarm-description "A certificate has under 14 days left, or the issuing router hasn't said for 6 hours: see forkrail domains ls and the router's log" \
    --namespace forkrail --metric-name CertificateDaysLeft --statistic Minimum --period 21600 --evaluation-periods 1 \
    --threshold 14 --comparison-operator LessThanThreshold --treat-missing-data breaching \
    --alarm-actions "$1" --ok-actions "$1" --tags "Key=$TAG_KEY,Value=$TAG_VALUE"
}

ga() { aws globalaccelerator --region "$GA_REGION" "$@"; }

accelerator_arn() {
  ga list-accelerators --query "Accelerators[?Name=='$GA_NAME'].AcceleratorArn | [0]" --output text
}

# Global Accelerator in front of the routers in ROUTER_REGIONS that exist: TCP 80 and 443, each region's
# router instance as its endpoint group's one endpoint, the client's address kept, health checked over
# HTTP on port 80 every 10 s (unhealthy after 3 failures, when clients go to the next nearest region).
accelerator() {
  local arn listener region instance group config hc=(--health-check-protocol HTTP --health-check-port 80
    --health-check-path /_forkrail/health --health-check-interval-seconds 10 --threshold-count 3)
  arn="$(accelerator_arn)"
  if [ "$arn" = None ]; then
    log "creating accelerator $GA_NAME"
    arn="$(ga create-accelerator --name "$GA_NAME" --ip-address-type IPV4 --enabled \
      --tags "Key=$TAG_KEY,Value=$TAG_VALUE" --query Accelerator.AcceleratorArn --output text)"
  fi
  has_tag "$(ga list-tags-for-resource --resource-arn "$arn" --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)" ||
    { echo "accelerator $GA_NAME isn't tagged $TAG_KEY=$TAG_VALUE; not touching it" >&2; return 2; }
  listener="$(ga list-listeners --accelerator-arn "$arn" --query 'Listeners[0].ListenerArn' --output text)"
  if [ "$listener" = None ]; then
    listener="$(ga create-listener --accelerator-arn "$arn" --protocol TCP --client-affinity NONE \
      --port-ranges FromPort=80,ToPort=80 FromPort=443,ToPort=443 --query Listener.ListenerArn --output text)"
  fi
  for region in $ROUTER_REGIONS; do
    instance="$(AWS_REGION=$region instance_id)"
    [ -n "$instance" ] || { log "no router in $region; skipped"; continue; }
    config="EndpointId=$instance,Weight=128,ClientIPPreservationEnabled=true"
    group="$(ga list-endpoint-groups --listener-arn "$listener" \
      --query "EndpointGroups[?EndpointGroupRegion=='$region'].EndpointGroupArn | [0]" --output text)"
    if [ "$group" = None ]; then
      log "endpoint group $region: $instance"
      ga create-endpoint-group --listener-arn "$listener" --endpoint-group-region "$region" \
        --endpoint-configurations "$config" --traffic-dial-percentage 100 "${hc[@]}" >/dev/null
    else
      ga update-endpoint-group --endpoint-group-arn "$group" --endpoint-configurations "$config" "${hc[@]}" >/dev/null
    fi
  done
  until [ "$(ga describe-accelerator --accelerator-arn "$arn" --query Accelerator.Status --output text)" = DEPLOYED ]; do sleep 10; done
  settings_addresses "$arn"
  accelerator_status
}

# The accelerator's addresses in state/settings.json (`addresses`), which custom domains' A
# records name (the CLI's and the dashboard's DNS instructions).
settings_addresses() {
  local ips settings
  ips="$(ga describe-accelerator --accelerator-arn "$1" --query 'Accelerator.IpSets[0].IpAddresses' --output json)"
  settings="$(aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/state/settings.json" - 2>/dev/null || echo '{}')"
  [ "$(jq -c '.addresses // []' <<<"$settings")" = "$(jq -c . <<<"$ips")" ] && return
  log "state/settings.json: addresses $(jq -r 'join(", ")' <<<"$ips")"
  jq --argjson ips "$ips" '.addresses = $ips' <<<"$settings" |
    aws s3 --region $STATE_REGION cp --quiet --content-type application/json - "s3://$BUCKET/state/settings.json"
}

accelerator_status() {
  local arn listener
  arn="$(accelerator_arn)"
  [ "$arn" != None ] || { echo "no accelerator $GA_NAME" >&2; return 2; }
  ga describe-accelerator --accelerator-arn "$arn" \
    --query 'Accelerator.[Status, join(`, `, IpSets[0].IpAddresses), DnsName]' --output text
  listener="$(ga list-listeners --accelerator-arn "$arn" --query 'Listeners[0].ListenerArn' --output text)"
  ga list-endpoint-groups --listener-arn "$listener" \
    --query 'EndpointGroups[].[EndpointGroupRegion, TrafficDialPercentage, EndpointDescriptions[0].EndpointId, EndpointDescriptions[0].HealthState]' \
    --output text
}

down() {
  local instance sg alarm check topic
  if [ "$AWS_REGION" != "$STATE_REGION" ]; then
    down_region
    return
  fi
  for alarm in $(aws cloudwatch --region "$STATE_REGION" describe-alarms --alarm-name-prefix forkrail- --query 'MetricAlarms[].AlarmArn' --output text); do
    if has_tag "$(aws cloudwatch --region "$STATE_REGION" list-tags-for-resource --resource-arn "$alarm" \
      --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)"; then
      log "deleting alarm ${alarm##*:}"
      aws cloudwatch --region "$STATE_REGION" delete-alarms --alarm-names "${alarm##*:}"
    fi
  done
  for check in $(aws route53 list-health-checks --query "HealthChecks[?HealthCheckConfig.ResourcePath=='/_forkrail/health'].Id" \
    --output text); do
    if has_tag "$(aws route53 list-tags-for-resource --resource-type healthcheck --resource-id "$check" \
      --query "ResourceTagSet.Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)"; then
      log "deleting health check $check"
      aws route53 delete-health-check --health-check-id "$check"
    fi
  done
  topic="arn:aws:sns:$STATE_REGION:$ACCOUNT:$ALERTS_TOPIC"
  if has_tag "$(aws sns --region "$STATE_REGION" list-tags-for-resource --resource-arn "$topic" \
    --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text 2>/dev/null)"; then
    log "deleting SNS topic $ALERTS_TOPIC"
    aws sns --region "$STATE_REGION" delete-topic --topic-arn "$topic"
  fi
  instance="$(instance_id)"
  if [ -n "$instance" ]; then
    log "terminating $instance"
    aws ec2 terminate-instances --instance-ids $instance >/dev/null
    aws ec2 wait instance-terminated --instance-ids $instance
  fi
  local alloc
  alloc="$(eip_allocation)"
  if [ "$alloc" != None ]; then
    log "releasing Elastic IP $EIP_NAME"
    aws ec2 release-address --allocation-id "$alloc"
  fi
  sg="$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
    --query 'SecurityGroups[0].GroupId' --output text)"
  if [ "$sg" != None ]; then
    log "deleting security group $sg"
    aws ec2 delete-security-group --group-id "$sg"
  fi
  local fn
  for fn in $(aws lambda list-functions --query "Functions[?starts_with(FunctionName, 'forkrail-')].FunctionName" \
    --output text); do
    if has_tag "$(function_tag "$fn")"; then
      log "deleting $fn"
      aws lambda delete-function --function-name "$fn"
    fi
  done
  if has_tag "$(aws s3api get-bucket-tagging --region "$STATE_REGION" --bucket "$BUCKET" \
    --query "TagSet[?Key=='$TAG_KEY'].Value | [0]" --output text 2>/dev/null)"; then
    log "deleting bucket $BUCKET"
    aws s3 --region $STATE_REGION rb "s3://$BUCKET" --force >/dev/null
  fi
  if has_tag "$(aws iam list-instance-profile-tags --instance-profile-name "$NAME" \
    --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text 2>/dev/null)"; then
    log "deleting instance profile $NAME"
    aws iam remove-role-from-instance-profile --instance-profile-name "$NAME" --role-name "$NAME" 2>/dev/null || true
    aws iam delete-instance-profile --instance-profile-name "$NAME"
  fi
  if has_tag "$(aws iam list-role-tags --role-name "$NAME" \
    --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text 2>/dev/null)"; then
    log "deleting role $NAME"
    aws iam detach-role-policy --role-name "$NAME" \
      --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore 2>/dev/null || true
    aws iam delete-role-policy --role-name "$NAME" --policy-name "$NAME" 2>/dev/null || true
    aws iam delete-role --role-name "$NAME"
  fi
  if has_tag "$(aws iam list-role-tags --role-name "$WORKER_ROLE" \
    --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text 2>/dev/null)"; then
    log "deleting role $WORKER_ROLE"
    aws iam delete-role --role-name "$WORKER_ROLE"
  fi
}

# Outside the state region: that region's router, Elastic IP, security group and functions. The
# routing table still names versions there until `forkrail regions rm` runs.
down_region() {
  local instance alloc sg fn
  instance="$(instance_id)"
  if [ -n "$instance" ]; then
    log "terminating $instance in $AWS_REGION"
    aws ec2 terminate-instances --instance-ids $instance >/dev/null
    aws ec2 wait instance-terminated --instance-ids $instance
  fi
  alloc="$(eip_allocation)"
  [ "$alloc" = None ] || { log "releasing Elastic IP $EIP_NAME"; aws ec2 release-address --allocation-id "$alloc"; }
  sg="$(aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
    --query 'SecurityGroups[0].GroupId' --output text)"
  [ "$sg" = None ] || { log "deleting security group $sg"; aws ec2 delete-security-group --group-id "$sg"; }
  for fn in $(aws lambda list-functions --query "Functions[?starts_with(FunctionName, 'forkrail-')].FunctionName" --output text); do
    if has_tag "$(function_tag "$fn")"; then
      log "deleting $fn in $AWS_REGION"
      aws lambda delete-function --function-name "$fn"
    fi
  done
}

case "${1:-}" in
  up)
    if [ "$AWS_REGION" != "$STATE_REGION" ]; then
      # Another region's router: no test functions or bucket here (the CLI makes the code bucket).
      build_binaries
      build_cli
      ensure_roles
      upload_router
      ensure_security_group
      ensure_instance
      wait_for_router
      ensure_eip
      regional_router
      exit
    fi
    build
    build_cli
    ensure_roles
    ensure_bucket
    upload_router
    deploy_functions
    ensure_security_group
    ensure_instance
    wait_for_router
    ensure_eip
    if [ -n "$EIP_MOVED" ]; then
      log "restarting the router on its Elastic IP"
      run "set -e
$INSTALL_UNIT
$ROUTER_RESTART"
    fi
    ;;
  push)
    if [ "${FORKRAIL_PREBUILT:-0}" = 1 ]; then
      [ -x "$ROUTER_BIN" ] || { echo "FORKRAIL_PREBUILT=1, but there's no $ROUTER_BIN" >&2; exit 1; }
      if [ "$AWS_REGION" = "$STATE_REGION" ] && [ "${FORKRAIL_TEST_FUNCTIONS:-1}" != 0 ]; then
        mkdir -p "$PACKAGES"
        stage="$(mktemp -d)"
        cp "$RELEASE/forkrail-runtime" "$stage/bootstrap"
        mkdir "$stage/app" && cp "$ROOT/test-apps/node-http/server.mjs" "$stage/app/"
        rm -f "$ZIP" && (cd "$stage" && zip -q -X -r "$ZIP" bootstrap app) && rm -rf "$stage"
        package_python "$ROOT/test-apps/python-fastapi" "$PYTHON_ZIP"
      fi
    else
      if [ "$AWS_REGION" = "$STATE_REGION" ] && [ "${FORKRAIL_TEST_FUNCTIONS:-1}" != 0 ]; then build; else build_binaries; fi
      build_cli
    fi
    [ "${FORKRAIL_ROLES:-1}" = 0 ] || ensure_roles
    ensure_challenge_key
    ensure_peer_key
    upload_router
    [ "$AWS_REGION" != "$STATE_REGION" ] || [ "${FORKRAIL_TEST_FUNCTIONS:-1}" = 0 ] || deploy_functions
    # The binary is downloaded while the old router still serves, so the restart only drains and starts.
    ensure_security_group
    vector_lines="$(install_vector)"
    # One command on the instance: GeoIP first (a failed download keeps the databases it has),
    # then the new binary staged and started beside the running router, then Vector. GeoIP and
    # Vector each run as a script of their own with bash -e, so a failed step stops that script
    # (not the push, as when they were commands of their own): `( set -e ... ) || ...` would not
    # stop, since bash ignores set -e on the left of ||.
    log "GeoIP, then the router binary staged and started beside the running one${vector_lines:+, then Vector}"
    out="$(run "set -e
$(apart geoip 'GeoIP: the update failed; the router keeps the databases it has' "$(install_geoip)")
$ROUTES_ENV
$(peers_env)
$INSTALL_UNIT
$(install_export)
$(install_sink)
aws s3 --region $STATE_REGION cp --quiet s3://$BUCKET/forkrail-router /opt/forkrail/forkrail-router.new
chmod 755 /opt/forkrail/forkrail-router.new
$ROUTER_RESTART
${vector_lines:+$(apart vector 'Vector: the update failed' "$vector_lines")}")"
    printf '%s\n' "$out"
    grep -q 'serving, ready after' <<<"$out" || { echo "the router in $AWS_REGION didn't restart (above)" >&2; exit 1; }
    ;;
  geoip)
    log "installing the GeoIP databases and their timer on the router in $AWS_REGION"
    run "set -e
$(install_geoip)"
    ;;
  vector)
    vector_lines="$(install_vector)"
    [ -n "$vector_lines" ] || { echo "no analytics box yet (state/analytics/endpoint.json): run deploy/aws/analytics.sh up" >&2; exit 2; }
    ensure_roles
    log "installing Vector on the router in $AWS_REGION"
    run "set -e
$vector_lines"
    ;;
  domain)
    [[ $# -ge 3 && $# -le 4 ]] || { echo "usage: $0 domain BASE ZONE_ID [staging]" >&2; exit 2; }
    base="$2" zone="$3" staging=0
    [[ "$base" =~ ^[a-z0-9-]+(\.[a-z0-9-]+)+$ ]] || { echo "bad base domain: $base" >&2; exit 2; }
    [[ "$zone" =~ ^[a-f0-9]{32}$ ]] || { echo "bad zone id: $zone" >&2; exit 2; }
    case "${4:-}" in
      "") ;;
      staging) staging=1 ;;
      *) echo "usage: $0 domain BASE ZONE_ID [staging]" >&2; exit 2 ;;
    esac
    [ "$AWS_REGION" = "$STATE_REGION" ] ||
      { echo "domain sets up the router in $STATE_REGION, which issues the certificates; in $AWS_REGION, up does" >&2; exit 2; }
    ensure_roles
    ensure_security_group
    ensure_eip
    ip="$(eip_address)"
    log "the CLI's base domain: $base (state/settings.json)"
    settings="$(aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/state/settings.json" - 2>/dev/null || echo '{}')"
    jq --arg base "$base" '.base_domain = $base' <<<"$settings" |
      aws s3 --region $STATE_REGION cp --quiet --content-type application/json - "s3://$BUCKET/state/settings.json"
    cloudflare_wildcard "$zone" "$base" "$ip"
    log "the router's TLS settings, then a restart"
    edits="$(env_edits "FORKRAIL_BASE_DOMAIN=$base" FORKRAIL_TLS_LISTEN=0.0.0.0:443 FORKRAIL_REDIRECT_LISTEN=0.0.0.0:80 \
      "FORKRAIL_TLS_CERTS=s3://$BUCKET/state/certs" "FORKRAIL_CLOUDFLARE_ZONE=$zone" "FORKRAIL_ACME_STAGING=$staging")"
    run "set -e
$INSTALL_UNIT
$edits
cp /opt/forkrail/forkrail-router /opt/forkrail/forkrail-router.new
$ROUTER_RESTART
grep -E '^FORKRAIL_(BASE|TLS|REDIRECT|ACME|CLOUDFLARE)' /etc/forkrail/router.env"
    log "https://<project>.$base on $ip; the first certificate takes about a minute"
    ;;
  point)
    [[ $# -eq 3 ]] || { echo "usage: $0 point HOST TARGET" >&2; exit 2; }
    host="$2" target="$3"
    for name in "$host" "$target"; do
      [[ "$name" =~ ^[a-z0-9-]+(\.[a-z0-9-]+)+$ ]] || { echo "bad name: $name" >&2; exit 2; }
    done
    CF_TOKEN="$(aws ssm get-parameter --region "$STATE_REGION" --name "$CLOUDFLARE_TOKEN_PARAM" --with-decryption --query Parameter.Value --output text)"
    zone="" candidate="$host"
    while [[ "$candidate" == *.* ]]; do
      zone="$(cloudflare GET "/zones?name=$candidate" | jq -r '.result[0].id // empty')"
      [ -n "$zone" ] && break
      candidate="${candidate#*.}"
    done
    [ -n "$zone" ] || { unset CF_TOKEN; echo "no Cloudflare zone for $host that the token can edit" >&2; exit 2; }
    out="$(cloudflare GET "/zones/$zone/dns_records?name=$host")"
    jq -e '.success and (.result | length == 1) and .result[0].type == "CNAME"' <<<"$out" >/dev/null ||
      { unset CF_TOKEN; echo "$host: expected one CNAME record, found $(jq -c '[.result[]? | {type, content}]' <<<"$out")" >&2; exit 2; }
    record="$(jq -r '.result[0].id' <<<"$out")"
    log "$host was: CNAME $(jq -r '.result[0] | "\(.content), TTL \(.ttl), proxied \(.proxied)"' <<<"$out")"
    body="$(jq -nc --arg name "$host" --arg target "$target" --arg was "$(jq -r '.result[0].content' <<<"$out")" \
      '{type: "CNAME", name: $name, content: $target, ttl: 60, proxied: false, comment: ("forkrail test-env.sh point; was " + $was)}')"
    out="$(cloudflare PUT "/zones/$zone/dns_records/$record" "$body")"
    unset CF_TOKEN
    jq -e .success <<<"$out" >/dev/null || { echo "Cloudflare: $(jq -c .errors <<<"$out")" >&2; exit 1; }
    log "$host is now: CNAME $target, TTL 60, DNS only. Resolvers that cached the old answer keep it up to its old TTL"
    ;;
  monitor)
    [[ $# -eq 3 ]] || { echo "usage: $0 monitor HOST EMAIL" >&2; exit 2; }
    host="$2" email="$3"
    [[ "$host" =~ ^[a-z0-9-]+(\.[a-z0-9-]+)+$ ]] || { echo "bad host: $host" >&2; exit 2; }
    [[ "$email" =~ ^[^@\ ]+@[^@\ ]+\.[^@\ ]+$ ]] || { echo "bad email: $email" >&2; exit 2; }
    ensure_roles
    log "installing the log export on the router"
    run "set -e
$(install_export)
systemctl is-active forkrail-log-export.timer"
    topic="$(ensure_alerts_topic "$email")"
    check="$(ensure_health_check "$host")"
    aws cloudwatch --region "$STATE_REGION" put-metric-alarm --alarm-name "forkrail-health-$host" \
      --alarm-description "https://$host/_forkrail/health is failing from Route 53's health checkers: the router, its certificate or DNS" \
      --namespace AWS/Route53 --metric-name HealthCheckStatus --dimensions "Name=HealthCheckId,Value=$check" \
      --statistic Minimum --period 60 --evaluation-periods 2 --threshold 1 --comparison-operator LessThanThreshold \
      --treat-missing-data missing --alarm-actions "$topic" --ok-actions "$topic" \
      --tags "Key=$TAG_KEY,Value=$TAG_VALUE"
    aws cloudwatch --region "$STATE_REGION" put-metric-alarm --alarm-name forkrail-5xx \
      --alarm-description "$ALARM_5XX or more 5xx responses on the router's public listener in 10 minutes; see test-env.sh report" \
      --namespace forkrail --metric-name Errors5xx --statistic Sum --period 600 --evaluation-periods 1 \
      --threshold "$ALARM_5XX" --comparison-operator GreaterThanOrEqualToThreshold --treat-missing-data notBreaching \
      --alarm-actions "$topic" --ok-actions "$topic" --tags "Key=$TAG_KEY,Value=$TAG_VALUE"
    expiry_alarm "$topic"
    attack_alarm "$topic"
    log "alarms forkrail-health-$host, forkrail-5xx, forkrail-cert-expiry and forkrail-attack mail $email through $ALERTS_TOPIC"
    ;;
  roles) ensure_roles; log "$NAME: roles applied" ;;
  attack-alarm)
    # The attack alarm alone, on the existing alerts topic.
    topic="arn:aws:sns:$STATE_REGION:$ACCOUNT:$ALERTS_TOPIC"
    aws sns --region "$STATE_REGION" get-topic-attributes --topic-arn "$topic" >/dev/null ||
      { echo "no topic $ALERTS_TOPIC: run monitor first" >&2; exit 2; }
    attack_alarm "$topic"
    log "alarm forkrail-attack mails through $ALERTS_TOPIC"
    ;;
  alerts-policy)
    # The alerts topic's policy alone, on the existing topic; `default` puts SNS's own back.
    topic="arn:aws:sns:$STATE_REGION:$ACCOUNT:$ALERTS_TOPIC"
    has_tag "$(aws sns --region "$STATE_REGION" list-tags-for-resource --resource-arn "$topic" --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)" ||
      { echo "SNS topic $ALERTS_TOPIC isn't there or isn't tagged $TAG_KEY=$TAG_VALUE: run monitor first" >&2; exit 2; }
    case "${2:-}" in
      "") policy="$(alerts_topic_policy "$topic")" ;;
      default) policy="$(sns_default_policy "$topic")" ;;
      *) echo "usage: $0 alerts-policy [default]" >&2; exit 2 ;;
    esac
    aws sns --region "$STATE_REGION" set-topic-attributes --topic-arn "$topic" --attribute-name Policy --attribute-value "$policy"
    if [ -n "${2:-}" ]; then log "$ALERTS_TOPIC has SNS's default policy again"; else log "$ALERTS_TOPIC has forkrail's policy"; fi
    ;;
  expiry-alarm)
    # The certificate expiry alarm alone, on the existing alerts topic.
    topic="arn:aws:sns:$STATE_REGION:$ACCOUNT:$ALERTS_TOPIC"
    aws sns --region "$STATE_REGION" get-topic-attributes --topic-arn "$topic" >/dev/null ||
      { echo "no topic $ALERTS_TOPIC: run monitor first" >&2; exit 2; }
    expiry_alarm "$topic"
    log "alarm forkrail-cert-expiry mails through $ALERTS_TOPIC"
    ;;
  logs)
    [[ $# -ge 2 && $# -le 3 ]] || { echo "usage: $0 logs PROJECT|DEPLOYMENT|default [SINCE]" >&2; exit 2; }
    since="${3:-1 hour ago}"
    [[ "$since" =~ ^[A-Za-z0-9\ :+-]+$ ]] || { echo "bad SINCE: $since" >&2; exit 2; }
    if [ "$2" = default ]; then
      ids=default
    else
      routes="$(aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/state/routes.json" - 2>/dev/null || echo '{}')"
      ids="$(jq -r --arg p "$2" '.deployments // {} | to_entries[] | select(.value.project == $p or .key == $p) | .key' \
        <<<"$routes" | tr '\n' ' ')"
      [ -n "$ids" ] || { echo "no project or deployment $2 in the routing table" >&2; exit 2; }
    fi
    out="$(mktemp)"
    run_to_file "$(to_router "$ROOT/deploy/aws/app-logs.py")
$(to_router "$ROOT/deploy/aws/forkrail_log.py")
python3 /opt/forkrail/app-logs.py '$since' $ids" "$out"
    cat "$out"
    rm -f "$out"
    ;;
  report)
    [[ $# -ge 2 && $# -le 3 ]] || { echo "usage: $0 report SINCE [UNTIL]" >&2; exit 2; }
    for t in "${@:2}"; do [[ "$t" =~ ^[A-Za-z0-9\ :+-]+$ ]] || { echo "bad time: $t" >&2; exit 2; }; done
    routes="$(aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/state/routes.json" - 2>/dev/null || echo '{}')"
    projects="$(jq -c '.deployments // {} | map_values({project, memory_mb})' <<<"$routes" | base64 | tr -d '\n')"
    out="$(mktemp)"
    run_to_file "$(to_router "$ROOT/deploy/aws/report.py")
$(to_router "$ROOT/deploy/aws/forkrail_log.py")
python3 /opt/forkrail/report.py '$projects' '$2' ${3:+'$3'}" "$out"
    cat "$out"
    rm -f "$out"
    ;;
  ip) public_ip ;;
  tcp) run "set -e
$(cat "$ROOT/deploy/aws/tcp-tuning.sh")" ;;
  app)
    [[ $# -ge 4 ]] || { echo "usage: $0 app NAME DIR ENTRY [KEY=VALUE...]" >&2; exit 2; }
    fn="forkrail-$2" dir="$3" entry="$4"
    shift 4
    [[ "${entry%%:*}" == *.py ]] || { echo "only Python apps so far: ENTRY is a .py file" >&2; exit 2; }
    [ "$AWS_REGION" = "$STATE_REGION" ] || { echo "app deploys in $STATE_REGION only; use the CLI elsewhere" >&2; exit 2; }
    environment="$PYTHON_ENV,FORKRAIL_APP_ENTRY=$entry"
    for variable in "$@"; do environment+=",$variable"; done
    build_binaries
    ensure_bucket
    package_python "$(cd "$dir" && pwd)" "$PACKAGES/$fn.zip"
    deploy_function "$fn" "$PYTHON_RUNTIME" unused "Variables={$environment}" "$PACKAGES/$fn.zip"
    ;;
  use)
    [[ $# -eq 2 ]] || { echo "usage: $0 use NAME" >&2; exit 2; }
    fn="$(tagged_function "$2")"
    run "set -e
sed -i 's/^FORKRAIL_FUNCTION=.*/FORKRAIL_FUNCTION=$fn/' /etc/forkrail/router.env
cp /opt/forkrail/forkrail-router /opt/forkrail/forkrail-router.new
$ROUTER_RESTART
grep FORKRAIL_FUNCTION /etc/forkrail/router.env"
    ;;
  load)
    [[ $# -ge 4 ]] || { echo "usage: $0 load DURATION CONNECTIONS PATH [HOST...]" >&2; exit 2; }
    summary="$(base64 < "$ROOT/deploy/aws/load-summary.py" | tr -d '\n')"
    url="http://127.0.0.1:8080$4"
    [[ "$4" == http* ]] && url="$4"
    [[ "${OHA_ARGS:-}" =~ ^[A-Za-z0-9\ =._-]*$ ]] || { echo "bad OHA_ARGS" >&2; exit 2; }
    oha="oha --no-tui ${OHA_ARGS:-} -z '$2' -c '$3' '$url'"
    if [[ $# -gt 4 ]]; then
      oha=""
      for host in "${@:5}"; do
        [[ "$host" =~ ^[a-z0-9.-]+$ ]] || { echo "bad host: $host" >&2; exit 2; }
        oha+="oha --no-tui ${OHA_ARGS:-} -z '$2' -c '$3' -H 'Host: $host' '$url' > /tmp/oha-$host.txt &
"
      done
      oha+="wait
for f in /tmp/oha-*.txt; do echo \"== \${f#/tmp/oha-}\"; sed -n '1,12p;/Status code/,\$p' \"\$f\"; rm -f \"\$f\"; done"
    fi
    run "set -e
if ! command -v oha >/dev/null; then
  curl -sSL -o /tmp/oha $OHA_URL
  echo '$OHA_SHA256  /tmp/oha' | sha256sum -c - >/dev/null
  install -m 755 /tmp/oha /usr/local/bin/oha
fi
echo $summary | base64 -d > /opt/forkrail/load-summary.py
$(to_router "$ROOT/deploy/aws/forkrail_log.py")
since=\"\$(date -u '+%Y-%m-%d %H:%M:%S')\"
$oha
python3 /opt/forkrail/load-summary.py \"\$since\""
    ;;
  memory)
    [[ $# -eq 3 ]] || { echo "usage: $0 memory NAME MB" >&2; exit 2; }
    fn="$(tagged_function "$2")"
    log "setting $fn to $3 MB"
    aws lambda update-function-configuration --function-name "$fn" --memory-size "$3" >/dev/null
    aws lambda wait function-updated-v2 --function-name "$fn"
    ;;
  router-env)
    shift
    [[ $# -ge 1 ]] || { echo "usage: $0 router-env KEY=VALUE..." >&2; exit 2; }
    edits="$(env_edits "$@")"
    # The binary in place, staged again, so the restart doesn't wait for the S3 download.
    run "set -e
$edits
cp /opt/forkrail/forkrail-router /opt/forkrail/forkrail-router.new
$ROUTER_RESTART
cat /etc/forkrail/router.env"
    ;;
  replay)
    [[ $# -ge 3 ]] || { echo "usage: $0 replay PATHS_FILE MODE [ARGS...]" >&2; exit 2; }
    paths="$2"
    shift 2
    run "set -e
$(to_router "$ROOT/deploy/aws/replay.py")
$(to_router "$paths")
echo \"start: \$(date -u '+%Y-%m-%d %H:%M:%S')\"
python3 /opt/forkrail/replay.py /opt/forkrail/$(basename "$paths") $*
echo \"end: \$(date -u '+%Y-%m-%d %H:%M:%S')\""
    ;;
  fetch)
    [[ $# -eq 3 ]] || { echo "usage: $0 fetch SINCE OUT" >&2; exit 2; }
    key="results/$(date -u +%Y%m%dT%H%M%S)-$RANDOM.jsonl.gz"
    run "set -e
$(to_router "$ROOT/deploy/aws/journal-json.py")
$(to_router "$ROOT/deploy/aws/forkrail_log.py")
python3 /opt/forkrail/journal-json.py '$2' | gzip > /tmp/journal.jsonl.gz
aws s3 --region $STATE_REGION cp --quiet /tmp/journal.jsonl.gz s3://$BUCKET/$key" >/dev/null
    aws s3 --region $STATE_REGION cp --quiet "s3://$BUCKET/$key" - | gunzip > "$3"
    log "$(wc -l < "$3" | tr -d ' ') events in $3"
    ;;
  run)
    shift
    run "$*"
    ;;
  stop)
    aws ec2 stop-instances --instance-ids "$(instance_id)" >/dev/null
    log "stopping $(instance_id)"
    ;;
  start)
    ensure_security_group
    INSTANCE="$(instance_id)"
    aws ec2 start-instances --instance-ids "$INSTANCE" >/dev/null
    aws ec2 wait instance-running --instance-ids "$INSTANCE"
    # The Elastic IP stays attached across stop/start; this reattaches it if it isn't.
    if [ "$(eip_allocation)" != None ]; then
      ensure_eip
      [ -z "$EIP_MOVED" ] || run "set -e
$INSTALL_UNIT
$ROUTER_RESTART"
    fi
    wait_for_router
    ;;
  accelerator) accelerator ;;
  accelerator-status) accelerator_status ;;
  dial)
    [[ $# -eq 3 && "$3" =~ ^[0-9]+$ && "$3" -le 100 ]] || { echo "usage: $0 dial REGION PERCENT" >&2; exit 2; }
    arn="$(accelerator_arn)"
    listener="$(ga list-listeners --accelerator-arn "$arn" --query 'Listeners[0].ListenerArn' --output text)"
    group="$(ga list-endpoint-groups --listener-arn "$listener" \
      --query "EndpointGroups[?EndpointGroupRegion=='$2'].EndpointGroupArn | [0]" --output text)"
    [ "$group" != None ] || { echo "no endpoint group in $2" >&2; exit 2; }
    ga update-endpoint-group --endpoint-group-arn "$group" --traffic-dial-percentage "$3" >/dev/null
    log "$2 takes $3% of its nearby traffic; the rest goes to the next nearest region"
    accelerator_status
    ;;
  wildcard)
    [[ $# -ge 4 ]] || { echo "usage: $0 wildcard ZONE_ID BASE IP..." >&2; exit 2; }
    [[ "$2" =~ ^[a-f0-9]{32}$ ]] || { echo "bad zone id: $2" >&2; exit 2; }
    [[ "$3" =~ ^[a-z0-9-]+(\.[a-z0-9-]+)+$ ]] || { echo "bad base domain: $3" >&2; exit 2; }
    for ip in "${@:4}"; do
      [[ "$ip" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || { echo "bad IPv4 address: $ip" >&2; exit 2; }
    done
    cloudflare_a_set "$2" "*.$3" "${@:4}"
    ;;
  arecord)
    [[ $# -ge 3 ]] || { echo "usage: $0 arecord ZONE_ID NAME [IP...]" >&2; exit 2; }
    [[ "$2" =~ ^[a-f0-9]{32}$ ]] || { echo "bad zone id: $2" >&2; exit 2; }
    [[ "$3" =~ ^[a-z0-9-]+(\.[a-z0-9-]+)+$ ]] || { echo "bad name: $3" >&2; exit 2; }
    for ip in "${@:4}"; do
      [[ "$ip" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || { echo "bad IPv4 address: $ip" >&2; exit 2; }
    done
    cloudflare_a_set "$2" "$3" "${@:4}"
    ;;
  down) down ;;
  *)
    sed -n '2,/^set -euo/p' "$0" | sed '$d'
    exit 1
    ;;
esac
