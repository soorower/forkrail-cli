#!/usr/bin/env bash
# A separate router for load tests, so they never run on the routers real users reach.
#
# forkrail-bench is the same instance type as a router, with the routers' role and security group,
# and reads the same routing table, so any deployment answers on it. It differs in what keeps it
# apart: no Elastic IP, no base domain (so plain HTTP on 8080 only: no TLS listener, no
# certificates, no ACME), no log export or CloudWatch metrics, a cap on its workers, and Vector
# sends its log to the database forkrail_bench (the same tables), not forkrail. It isn't behind
# Global Accelerator or any DNS name. Load runs on the instance itself (oha against 127.0.0.1).
#
#   deploy/aws/bench.sh up                 launch it with the routers' current binary, Vector and oha
#   deploy/aws/bench.sh push               build the working tree's router and restart the bench on it
#   deploy/aws/bench.sh release            put the routers' released binary back
#   deploy/aws/bench.sh env KEY=VALUE...   set router settings (empty value removes one) and restart
#   deploy/aws/bench.sh run 'COMMAND'      run a shell command on it (as root)
#   deploy/aws/bench.sh load NAME SECONDS CONNECTIONS HOST PATH [RATE]
#                                          oha for SECONDS, flat out or at RATE a second, then what
#                                          the router and Vector used (see bench-load.sh)
#   deploy/aws/bench.sh ip
#   deploy/aws/bench.sh down               terminate it
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
export AWS_REGION=us-east-1
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
NAME=forkrail-bench
ROUTER=forkrail-router # the routers' instance profile and security group
INSTANCE_TYPE=t4g.small
DISK_GB=30 # the hour's log file is about 540 bytes an event, and nothing here compresses it
MAX_WORKERS=120 # of the account's 400 concurrent Lambdas, which production shares
TAG_KEY=project
TAG_VALUE=forkrail
TARGET=aarch64-unknown-linux-gnu
ACCOUNT="$(aws sts get-caller-identity --query Account --output text)"
BUCKET="forkrail-artifacts-$ACCOUNT"
BINARY_KEY=bench/forkrail-router
DATABASE=forkrail_bench
FUNCTION=forkrail-echo
OHA_URL=https://github.com/hatoo/oha/releases/download/v1.16.0/oha-linux-arm64
OHA_SHA256=99a790eb8c3e0feaca974bd6b32f0f8d4426a0c5b289f39e833e5b2c7529cd39
VECTOR_VERSION=0.60.0
VECTOR_TARBALL=vector-$VECTOR_VERSION-aarch64-unknown-linux-musl.tar.gz
VECTOR_SHA256=e2984f826aa61bf3ea98fba4cccde2437f645794b2f8a3d45c97df9594dcfbd1
CLICKHOUSE_INGEST_PARAM=/forkrail/clickhouse/ingest

log() { echo "==> $*" >&2; }

instance_id() {
  aws ec2 describe-instances \
    --filters "Name=tag:Name,Values=$NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
    Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId' --output text
}

# The same as test-env.sh's: SSM's children would otherwise outlive the router under memory pressure.
run() {
  local instance id status=Pending
  instance="$(instance_id)"
  [ -n "$instance" ] || { echo "no $NAME instance: run $0 up" >&2; exit 2; }
  id="$(aws ssm send-command --instance-ids "$instance" --document-name AWS-RunShellScript \
    --parameters "$(jq -n --arg command "echo 1000 > /proc/self/oom_score_adj
$1" '{commands: [$command], executionTimeout: ["3600"]}')" \
    --query Command.CommandId --output text)"
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

# Shell lines that put a local file on the instance at the given path.
to_bench() {
  aws s3 cp --quiet "$1" "s3://$BUCKET/bench/$(basename "$1")"
  echo "aws s3 cp --quiet s3://$BUCKET/bench/$(basename "$1") $2"
}

RESTART='systemctl restart forkrail-router@blue
for i in $(seq 1 60); do curl -sf -o /dev/null http://127.0.0.1:8080/_forkrail/health && break; sleep 0.5; done
systemctl is-active forkrail-router@blue'

launch() {
  local vpc sg ami zone subnet user_data instance
  vpc="$(aws ec2 describe-vpcs --filters Name=is-default,Values=true --query 'Vpcs[0].VpcId' --output text)"
  sg="$(aws ec2 describe-security-groups --filters "Name=vpc-id,Values=$vpc" "Name=group-name,Values=$ROUTER" \
    "Name=tag:$TAG_KEY,Values=$TAG_VALUE" --query 'SecurityGroups[0].GroupId' --output text)"
  [ "$sg" != None ] || { echo "the routers' security group ($ROUTER, tagged $TAG_KEY=$TAG_VALUE) isn't here" >&2; exit 1; }
  ami="$(aws ssm get-parameter --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64 \
    --query Parameter.Value --output text)"
  zone="$(aws ec2 describe-instance-type-offerings --location-type availability-zone \
    --filters "Name=instance-type,Values=$INSTANCE_TYPE" --query 'sort(InstanceTypeOfferings[].Location)[0]' --output text)"
  subnet="$(aws ec2 describe-subnets \
    --filters "Name=vpc-id,Values=$vpc" "Name=availability-zone,Values=$zone" Name=default-for-az,Values=true \
    --query 'Subnets[0].SubnetId' --output text)"
  # The routers' own first boot, with the bench's binary.
  user_data="$(sed -e "s|s3://@BUCKET@/forkrail-router |s3://@BUCKET@/$BINARY_KEY |g" \
    -e "s|@BUCKET@|$BUCKET|g" -e "s|@FUNCTION@|$FUNCTION|g" -e "s|@REGION@|$AWS_REGION|g" \
    -e "s|@STATE_REGION@|$AWS_REGION|g" \
    -e "/^@TCP_TUNING@\$/r $ROOT/deploy/aws/tcp-tuning.sh" -e "/^@TCP_TUNING@\$/d" \
    "$ROOT/deploy/aws/router-init.sh")"
  grep -q "$BINARY_KEY" <<<"$user_data" || { echo "router-init.sh no longer names the binary as expected" >&2; exit 1; }
  log "launching $NAME ($INSTANCE_TYPE, $DISK_GB GB)"
  instance="$(aws ec2 run-instances --image-id "$ami" --instance-type "$INSTANCE_TYPE" \
    --subnet-id "$subnet" --security-group-ids "$sg" --iam-instance-profile "Name=$ROUTER" \
    --metadata-options HttpTokens=required,HttpEndpoint=enabled \
    --block-device-mappings "DeviceName=/dev/xvda,Ebs={VolumeSize=$DISK_GB,VolumeType=gp3,DeleteOnTermination=true}" \
    --user-data "$user_data" \
    --tag-specifications \
    "ResourceType=instance,Tags=[{Key=Name,Value=$NAME},{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
    "ResourceType=volume,Tags=[{Key=Name,Value=$NAME},{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
    --query 'Instances[0].InstanceId' --output text)"
  aws ec2 wait instance-running --instance-ids "$instance"
  log "$instance is running; waiting for SSM"
  until [ "$(aws ssm describe-instance-information --filters "Key=InstanceIds,Values=$instance" \
    --query 'InstanceInformationList[0].PingStatus' --output text 2>/dev/null)" = Online ]; do sleep 5; done
}

# Vector as on a router, but into $DATABASE.
vector_lines() {
  local endpoint url config
  endpoint="$(aws s3 cp --quiet "s3://$BUCKET/state/analytics/endpoint.json" -)"
  url="http://$(jq -r .private_ip <<<"$endpoint"):$(jq -r .http_port <<<"$endpoint")"
  config="$(mktemp -d)/vector-bench.yaml"
  sed -e '/^#TLS#$/d' -e "s|^    database: forkrail\$|    database: $DATABASE|" "$ROOT/deploy/aws/vector/vector.yaml" > "$config"
  [ "$(grep -c "database: $DATABASE" "$config")" = "$(grep -c '^    database: ' "$config")" ] || { echo "vector.yaml: a sink still names the real database" >&2; exit 1; }
  cat <<VECTOR
if ! /opt/vector/bin/vector --version 2>/dev/null | grep -q '^vector $VECTOR_VERSION '; then
  aws s3 cp --quiet s3://$BUCKET/tools/$VECTOR_TARBALL /tmp/vector.tar.gz
  echo '$VECTOR_SHA256  /tmp/vector.tar.gz' | sha256sum -c --quiet
  rm -rf /opt/vector.new && mkdir -p /opt/vector.new
  tar xzf /tmp/vector.tar.gz -C /opt/vector.new --strip-components=2
  rm -rf /opt/vector && mv /opt/vector.new /opt/vector && rm -f /tmp/vector.tar.gz
fi
mkdir -p /etc/vector
$(to_bench "$config" /etc/vector/vector.yaml)
chmod 644 /etc/vector/vector.yaml
imds=\$(curl -sf -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
instance=\$(curl -sf -H "X-aws-ec2-metadata-token: \$imds" http://169.254.169.254/latest/meta-data/instance-id)
sed -i -e "s|@REGION@|$AWS_REGION|" -e "s|@INSTANCE@|\$instance|" -e "s|@ENDPOINT@|$url|" /etc/vector/vector.yaml
(umask 077; aws ssm get-parameter --region $AWS_REGION --name $CLICKHOUSE_INGEST_PARAM --with-decryption --query Parameter.Value --output text > /etc/vector/ch_password)
cat > /etc/systemd/system/forkrail-vector.service <<'UNITFILE'
[Unit]
Description=forkrail: the router's log to ClickHouse (Vector)
After=network-online.target
Wants=network-online.target

[Service]
DynamicUser=yes
StateDirectory=vector
LoadCredential=ch_password:/etc/vector/ch_password
Environment=VECTOR_LOG=warn
ExecStartPre=/opt/vector/bin/vector validate --no-environment --config-yaml /etc/vector/vector.yaml
ExecStart=/opt/vector/bin/vector --config-yaml /etc/vector/vector.yaml
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

set_env() {
  local edits="" pair key
  for pair in "$@"; do
    key="${pair%%=*}"
    [[ "$pair" == *=* && "$key" =~ ^[A-Z][A-Z0-9_]*$ && "${pair#*=}" =~ ^[A-Za-z0-9_.:,/=@-]*$ ]] || { echo "bad setting: $pair" >&2; exit 2; }
    edits+="sed -i '/^$key=/d' /etc/forkrail/router.env
"
    [ -z "${pair#*=}" ] || edits+="echo '$pair' >> /etc/forkrail/router.env
"
  done
  echo "$edits"
}

case "${1:-}" in
  up)
    [ -z "$(instance_id)" ] || { echo "$NAME is already there: $(instance_id)" >&2; exit 2; }
    log "the routers' released binary as the bench's"
    aws s3 cp --quiet "s3://$BUCKET/forkrail-router" "s3://$BUCKET/$BINARY_KEY"
    launch
    log "settings, Vector into $DATABASE, oha"
    run "set -e
cloud-init status --wait >/dev/null 2>&1 || true
$(set_env FORKRAIL_CPU_TARGET_PCT=150 FORKRAIL_HEADROOM_PCT=0 FORKRAIL_IDLE_MS=auto FORKRAIL_MAX_WORKERS=$MAX_WORKERS)
$RESTART
$(vector_lines)
curl -sSL -o /tmp/oha $OHA_URL
echo '$OHA_SHA256  /tmp/oha' | sha256sum -c - >/dev/null
install -m 755 /tmp/oha /usr/local/bin/oha
grep -c . /etc/forkrail/router.env"
    ;;
  push)
    (cd "$ROOT" && cargo zigbuild --release --target "$TARGET.2.34" -p forkrail-router)
    aws s3 cp --quiet "$ROOT/target/$TARGET/release/forkrail-router" "s3://$BUCKET/$BINARY_KEY"
    run "set -e
aws s3 cp --quiet s3://$BUCKET/$BINARY_KEY /opt/forkrail/forkrail-router.new
chmod 755 /opt/forkrail/forkrail-router.new
$RESTART"
    ;;
  release)
    aws s3 cp --quiet "s3://$BUCKET/forkrail-router" "s3://$BUCKET/$BINARY_KEY"
    run "set -e
aws s3 cp --quiet s3://$BUCKET/$BINARY_KEY /opt/forkrail/forkrail-router.new
chmod 755 /opt/forkrail/forkrail-router.new
$RESTART"
    ;;
  env)
    shift
    [ $# -ge 1 ] || { echo "usage: $0 env KEY=VALUE..." >&2; exit 2; }
    run "set -e
$(set_env "$@")
$RESTART
sed -E 's/(TOKEN|SECRET|PASSWORD)([A-Z_]*)=.*/\1\2=***/' /etc/forkrail/router.env"
    ;;
  run)
    [ $# -eq 2 ] || { echo "usage: $0 run 'COMMAND'" >&2; exit 2; }
    run "$2"
    ;;
  load)
    [[ $# -ge 6 && $# -le 7 ]] || { echo "usage: $0 load NAME SECONDS CONNECTIONS HOST PATH [RATE]" >&2; exit 2; }
    [[ "$2" =~ ^[a-z0-9-]+$ && "$3" =~ ^[0-9]+$ && "$4" =~ ^[0-9]+$ && "$5" =~ ^[a-z0-9.-]+$ && "$6" =~ ^/[A-Za-z0-9/_.?=\&:,%-]*$ && "${7:-0}" =~ ^[0-9]+$ ]] || { echo "bad arguments" >&2; exit 2; }
    run "$(to_bench "$ROOT/deploy/aws/bench-load.sh" /opt/forkrail/bench-load.sh)
bash /opt/forkrail/bench-load.sh '$2' '$3' '$4' '$5' '$6' '${7:-0}'"
    ;;
  ip)
    aws ec2 describe-instances --instance-ids "$(instance_id)" \
      --query 'Reservations[0].Instances[0].[InstanceId,PublicIpAddress,PrivateIpAddress]' --output text
    ;;
  down)
    instance="$(instance_id)"
    [ -n "$instance" ] || { echo "no $NAME instance" >&2; exit 0; }
    tag="$(aws ec2 describe-instances --instance-ids "$instance" \
      --query "Reservations[0].Instances[0].Tags[?Key=='$TAG_KEY']|[0].Value" --output text)"
    [ "$tag" = "$TAG_VALUE" ] || { echo "$instance isn't tagged $TAG_KEY=$TAG_VALUE; leaving it" >&2; exit 1; }
    log "terminating $instance"
    aws ec2 terminate-instances --instance-ids "$instance" --query 'TerminatingInstances[0].CurrentState.Name' --output text
    ;;
  *)
    sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
