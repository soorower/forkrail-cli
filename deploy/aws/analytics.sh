#!/usr/bin/env bash
# The analytics box: ClickHouse (logs and metrics) and Postgres (the dashboard's own data: people,
# teams, members, sessions) on a t4g.medium in the state region, in the same zone as that
# region's router, so the router's Vector reaches it over the VPC for nothing. Routers in other
# regions send over HTTPS to its Elastic IP, with its own certificate pinned. The routers' hourly
# log files are the buffer: if the box is down, Vector waits, and nothing is lost for 48 hours.
# The S3 log export stays the archive, from which the box can be filled again (`backfill`).
#
#   deploy/aws/analytics.sh up        create or update everything: role, passwords in SSM, security
#                                     group, instance, Elastic IP, ClickHouse and its settings, the
#                                     schema. Idempotent
#   deploy/aws/analytics.sh role      apply the box's IAM role policy alone (no restart)
#   deploy/aws/analytics.sh schema    apply deploy/aws/clickhouse/schema.sql
#   deploy/aws/analytics.sh users     install deploy/aws/clickhouse/users.xml (creating any password
#                                     missing from SSM); ClickHouse reloads it without a restart
#   deploy/aws/analytics.sh sql 'QUERY'
#                                     run a query on the box as the admin user, e.g.
#                                     sql 'SELECT project, count() FROM forkrail.requests GROUP BY project'
#   deploy/aws/analytics.sh status    disk, memory, the size of each table, and Postgres
#   deploy/aws/analytics.sh backfill DAY [DAY...]
#                                     load days (YYYY/MM/DD) of the S3 log export, e.g. 2026/11/05, up to when
#                                     Vector took over on each router (so once it runs on all of them)
#   deploy/aws/analytics.sh meta      load the routing table's deployments and domains (backfill does too)
#   deploy/aws/analytics.sh postgres  install or update Postgres (deploy/aws/postgres/): the database
#                                     `forkrail` and its one user, the dashboard's, whose password is
#                                     made once in SSM and never reaches the box; TLS only; a backup
#                                     to S3 every 6 hours, kept 30 days. Idempotent, and ClickHouse
#                                     isn't touched
#   deploy/aws/analytics.sh psql 'SQL'
#                                     run SQL on the box as Postgres's admin, in the forkrail database
#   deploy/aws/analytics.sh pg-backup            back Postgres up now
#   deploy/aws/analytics.sh pg-backups           list the backups in S3
#   deploy/aws/analytics.sh pg-restore-check [KEY]
#                                     restore a backup (the newest) into a scratch database, print
#                                     each table's rows beside the live ones, drop it. Changes nothing
#   deploy/aws/analytics.sh pg-restore KEY --yes
#                                     put the live database back as that backup has it
#   deploy/aws/analytics.sh allow-me  let this machine's IP reach HTTPS (the CLI) and Postgres (the
#                                     dashboard run locally), after it changes
#   deploy/aws/analytics.sh egress    let apps reach HTTPS and Postgres through their router (the
#                                     dashboard on forkrail): the state region's router over the VPC
#                                     too, and print each router's FORKRAIL_EGRESS_TARGETS
#   deploy/aws/analytics.sh run CMD   run a shell command on the box through SSM
#   deploy/aws/analytics.sh ip        print the box's Elastic IP
#   deploy/aws/analytics.sh down      delete the box, its Elastic IP, security group and role (the
#                                     passwords in SSM and state/analytics/ stay)
#
# Every resource is named forkrail-analytics* and tagged project=forkrail; changes and deletes
# check the tag first.
set -euo pipefail

export AWS_PROFILE="${AWS_PROFILE:-forkrail}"
STATE_REGION=us-east-1
export AWS_REGION=$STATE_REGION
export AWS_PAGER=""

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ACCOUNT="$(aws sts get-caller-identity --query Account --output text)"
BUCKET="forkrail-artifacts-$ACCOUNT"
NAME=forkrail-analytics
EIP_NAME=forkrail-analytics-eip
ROUTER_NAME=forkrail-router
ROUTER_EIP_NAME=forkrail-router-eip
INSTANCE_TYPE=t4g.medium
DISK_GB=50
# The newest LTS on 2026-11-09. Upgrades: change it and run `up`.
CLICKHOUSE_VERSION=26.8.9.10
TAG_KEY=project
TAG_VALUE=forkrail
INGEST_PARAM=/forkrail/clickhouse/ingest
READER_PARAM=/forkrail/clickhouse/reader
BUILDLOG_PARAM=/forkrail/clickhouse/buildlog
# The dashboard's Postgres password, under forkrail's own key: of forkrail's roles only the
# dashboard's may decrypt it (deploy/aws/dashboard.sh), and this box never needs it.
POSTGRES_PARAM=/forkrail/web/postgres
ENV_KEY=alias/forkrail-env
# The install's own secrets' key (deploy/install/install-secrets.yaml): the ClickHouse passwords.
INSTALL_KEY=alias/forkrail-install
POSTGRES_PORT=5432
POSTGRES_DATABASE=forkrail
POSTGRES_USER=forkrail_web
BACKUP_PREFIX=backups/postgres
BACKUP_DAYS=30
# The box's certificate and address, for Vector on the routers and for the CLI.
STATE_PREFIX=state/analytics
# Regions whose routers ship over HTTPS (the state region's goes over the VPC).
OTHER_ROUTER_REGIONS="${OTHER_ROUTER_REGIONS:-us-west-1}"

log() { echo "==> $*" >&2; }

assume_role_policy() {
  jq -n '{Version: "2012-10-17", Statement: [{Effect: "Allow", Principal: {Service: "ec2.amazonaws.com"}, Action: "sts:AssumeRole"}]}'
}

ensure_role() {
  if ! aws iam get-role --role-name "$NAME" >/dev/null 2>&1; then
    log "creating role $NAME"
    aws iam create-role --role-name "$NAME" \
      --description "forkrail analytics box: read the log export and its passwords, SSM." \
      --assume-role-policy-document "$(assume_role_policy)" \
      --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null
    aws iam attach-role-policy --role-name "$NAME" \
      --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore
  fi
  aws iam put-role-policy --role-name "$NAME" --policy-name "$NAME" --policy-document "$(jq -n \
    --arg bucket "arn:aws:s3:::$BUCKET" \
    --arg logs "arn:aws:s3:::$BUCKET/logs/*" \
    --arg state "arn:aws:s3:::$BUCKET/state/*" \
    --arg certkeys "arn:aws:s3:::$BUCKET/state/certs/*/key.pem" \
    --arg acmekeys "arn:aws:s3:::$BUCKET/state/certs/*/account.json" \
    --arg tools "arn:aws:s3:::$BUCKET/tools/*" \
    --arg mine "arn:aws:s3:::$BUCKET/$STATE_PREFIX/*" \
    --arg backups "arn:aws:s3:::$BUCKET/$BACKUP_PREFIX/*" \
    --arg ingest "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter$INGEST_PARAM" \
    --arg reader "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter$READER_PARAM" \
    --arg buildlog "arn:aws:ssm:$STATE_REGION:$ACCOUNT:parameter$BUILDLOG_PARAM" \
    --arg keys "arn:aws:kms:$STATE_REGION:$ACCOUNT:key/*" \
    --arg ssm "ssm.$STATE_REGION.amazonaws.com" \
    --arg install_key "$INSTALL_KEY" \
    '{Version: "2012-10-17", Statement: [
      {Effect: "Allow", Action: "s3:GetObject", Resource: [$logs, $state, $tools]},
      # state/ also holds the private key of every certificate and the ACME account key: only
      # the routers read those.
      {Sid: "NoCertificateKeys", Effect: "Deny", Action: "s3:GetObject", Resource: [$certkeys, $acmekeys]},
      {Effect: "Allow", Action: "s3:ListBucket", Resource: $bucket},
      {Effect: "Allow", Action: "s3:PutObject", Resource: $mine},
      {Effect: "Allow", Action: ["s3:PutObject", "s3:GetObject"], Resource: $backups},
      {Effect: "Allow", Action: "ssm:GetParameter", Resource: [$ingest, $reader, $buildlog]},
      # Its three passwords, decrypted through SSM with the install key, and no other parameter of
      # the account: AmazonSSMManagedInstanceCore (for Run Command) allows ssm:GetParameter on all.
      {Sid: "OwnSecrets", Effect: "Allow", Action: "kms:Decrypt", Resource: $keys, Condition: {
        "ForAnyValue:StringEquals": {"kms:ResourceAliases": $install_key},
        StringEquals: {"kms:ViaService": $ssm, "kms:EncryptionContext:PARAMETER_ARN": [$ingest, $reader, $buildlog]}}},
      {Sid: "NoOtherParameters", Effect: "Deny",
        Action: ["ssm:GetParameter", "ssm:GetParameters", "ssm:GetParametersByPath", "ssm:GetParameterHistory"],
        NotResource: [$ingest, $reader, $buildlog]}]}')"
  if ! aws iam get-instance-profile --instance-profile-name "$NAME" >/dev/null 2>&1; then
    aws iam create-instance-profile --instance-profile-name "$NAME" --tags "Key=$TAG_KEY,Value=$TAG_VALUE" >/dev/null
    aws iam add-role-to-instance-profile --instance-profile-name "$NAME" --role-name "$NAME"
  fi
}

# The install key its secrets are made under (deploy/install/install-secrets.yaml): applied once if
# this install doesn't have it yet.
ensure_install_key() {
  if aws kms describe-key --region "$STATE_REGION" --key-id "$INSTALL_KEY" >/dev/null 2>&1; then return; fi
  log "making the install key ($INSTALL_KEY)"
  AWS_REGION="$STATE_REGION" "$ROOT/deploy/aws/stack.sh" apply install-secrets --yes
}

# ensure_secret PARAM [KEY] [DESCRIPTION]: a random password in SSM, made once, under KEY (the
# install key without one). It's piped, never on a command line or printed.
ensure_secret() {
  if aws ssm get-parameter --name "$1" --query Parameter.Name --output text >/dev/null 2>&1; then return; fi
  [ -n "${2:-}" ] || ensure_install_key
  log "creating the password $1"
  local input
  input="$(mktemp)"
  chmod 600 "$input"
  openssl rand -hex 24 | tr -d '\n' | jq -Rs --arg name "$1" --arg key "${2:-$INSTALL_KEY}" \
    --arg description "${3:-forkrail ClickHouse password}" \
    '{Name: $name, Type: "SecureString", Value: ., Description: $description, KeyId: $key,
      Tags: [{Key: "project", Value: "forkrail"}]}' > "$input"
  aws ssm put-parameter --cli-input-json "file://$input" >/dev/null || { rm -f "$input"; return 1; }
  rm -f "$input"
}

vpc_id() {
  aws ec2 describe-vpcs --filters Name=is-default,Values=true --query 'Vpcs[0].VpcId' --output text
}

# The state region's router: its instance, zone and security group.
router_instance() {
  aws ec2 describe-instances --filters "Name=tag:Name,Values=$ROUTER_NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
    Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[0].Instances[0].[Placement.AvailabilityZone, SecurityGroups[0].GroupId]' --output text
}

allow_cidr() {
  local out
  if ! out="$(aws ec2 authorize-security-group-ingress --group-id "$1" --ip-permissions "$(jq -n \
    --argjson port "$2" --arg cidr "$3" --arg description "$4" \
    '[{IpProtocol: "tcp", FromPort: $port, ToPort: $port, IpRanges: [{CidrIp: $cidr, Description: $description}]}]')" 2>&1)"; then
    grep -q InvalidPermission.Duplicate <<<"$out" || { echo "$out" >&2; return 1; }
  fi
}

allow_group() {
  local out
  if ! out="$(aws ec2 authorize-security-group-ingress --group-id "$1" --ip-permissions "$(jq -n \
    --argjson port "$2" --arg group "$3" --arg description "$4" \
    '[{IpProtocol: "tcp", FromPort: $port, ToPort: $port, UserIdGroupPairs: [{GroupId: $group, Description: $description}]}]')" 2>&1)"; then
    grep -q InvalidPermission.Duplicate <<<"$out" || { echo "$out" >&2; return 1; }
  fi
}

security_group() {
  aws ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" "Name=vpc-id,Values=$(vpc_id)" \
    --query 'SecurityGroups[0].GroupId' --output text
}

ensure_security_group() {
  local router_sg="$1" region ip
  SG="$(security_group)"
  if [ "$SG" = None ]; then
    log "creating security group $NAME"
    SG="$(aws ec2 create-security-group --group-name "$NAME" --vpc-id "$(vpc_id)" \
      --description "forkrail analytics: ClickHouse for the routers and the CLI" \
      --tag-specifications "ResourceType=security-group,Tags=[{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
      --query GroupId --output text)"
  fi
  allow_group "$SG" 8123 "$router_sg" "ClickHouse HTTP from the router in this region"
  allow_group "$SG" 8443 "$router_sg" "ClickHouse HTTPS from the router in this region (app egress)"
  for region in $OTHER_ROUTER_REGIONS; do
    ip="$(aws ec2 describe-addresses --region "$region" \
      --filters "Name=tag:Name,Values=$ROUTER_EIP_NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
      --query 'Addresses[0].PublicIp' --output text)"
    [ "$ip" = None ] || allow_cidr "$SG" 8443 "$ip/32" "ClickHouse HTTPS from the router in $region"
  done
  allow_me
}

# allow_postgres: Postgres for the routers alone, through which the dashboard connects (egress
# `postgres`): the state region's over the VPC, the others from their Elastic IPs.
allow_postgres() {
  local router_sg sg region ip
  read -r _ router_sg <<<"$(router_instance)"
  [ -n "${router_sg:-}" ] && [ "$router_sg" != None ] || { echo "no $ROUTER_NAME in $STATE_REGION" >&2; exit 2; }
  sg="$(security_group)"
  allow_group "$sg" "$POSTGRES_PORT" "$router_sg" "Postgres from the router in this region (app egress)"
  for region in $OTHER_ROUTER_REGIONS; do
    ip="$(aws ec2 describe-addresses --region "$region" \
      --filters "Name=tag:Name,Values=$ROUTER_EIP_NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
      --query 'Addresses[0].PublicIp' --output text)"
    [ "$ip" = None ] || allow_cidr "$sg" "$POSTGRES_PORT" "$ip/32" "Postgres from the router in $region (app egress)"
  done
}

# Apps on forkrail reach the box through their router (egress `analytics` and `postgres`), TLS end
# to end: the state region's router over the VPC, the others at the Elastic IP.
egress() {
  local router_sg region
  read -r _ router_sg <<<"$(router_instance)"
  [ -n "${router_sg:-}" ] && [ "$router_sg" != None ] || { echo "no $ROUTER_NAME in $STATE_REGION" >&2; exit 2; }
  allow_group "$(security_group)" 8443 "$router_sg" "ClickHouse HTTPS from the router in this region (app egress)"
  allow_postgres
  log "routers' egress targets (deploy/aws/test-env.sh router-env, with AWS_REGION for the others):"
  echo "  $STATE_REGION: FORKRAIL_EGRESS_TARGETS=analytics=$(private_ip):8443,postgres=$(private_ip):$POSTGRES_PORT"
  for region in $OTHER_ROUTER_REGIONS; do
    echo "  $region: FORKRAIL_EGRESS_TARGETS=analytics=$(eip_address):8443,postgres=$(eip_address):$POSTGRES_PORT"
  done
}

allow_me() {
  local me
  me="$(curl -sf https://checkip.amazonaws.com)"
  allow_cidr "${SG:-$(security_group)}" 8443 "$me/32" "ClickHouse HTTPS, tester (CLI)"
  allow_cidr "${SG:-$(security_group)}" "$POSTGRES_PORT" "$me/32" "Postgres, tester (the dashboard run locally)"
}

instance_id() {
  aws ec2 describe-instances --filters "Name=tag:Name,Values=$NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
    Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[].Instances[].InstanceId' --output text
}

ensure_instance() {
  local zone="$1" ami subnet tries=0
  INSTANCE="$(instance_id)"
  if [ -n "$INSTANCE" ]; then return; fi
  log "launching a $INSTANCE_TYPE in $zone"
  ami="$(aws ssm get-parameter --name /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64 \
    --query Parameter.Value --output text)"
  subnet="$(aws ec2 describe-subnets \
    --filters "Name=vpc-id,Values=$(vpc_id)" "Name=availability-zone,Values=$zone" Name=default-for-az,Values=true \
    --query 'Subnets[0].SubnetId' --output text)"
  until INSTANCE="$(aws ec2 run-instances --image-id "$ami" --instance-type "$INSTANCE_TYPE" \
    --subnet-id "$subnet" --security-group-ids "$SG" --iam-instance-profile "Name=$NAME" \
    --metadata-options HttpTokens=required,HttpEndpoint=enabled \
    --block-device-mappings "DeviceName=/dev/xvda,Ebs={VolumeSize=$DISK_GB,VolumeType=gp3,DeleteOnTermination=true}" \
    --tag-specifications \
    "ResourceType=instance,Tags=[{Key=Name,Value=$NAME},{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
    "ResourceType=volume,Tags=[{Key=Name,Value=$NAME},{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
    --query 'Instances[0].InstanceId' --output text 2>/dev/null)"; do
    tries=$((tries + 1))
    [ "$tries" -lt 10 ] || { echo "run-instances kept failing" >&2; return 1; }
    sleep 3
  done
  aws ec2 wait instance-running --instance-ids "$INSTANCE"
}

eip_allocation() {
  aws ec2 describe-addresses --filters "Name=tag:Name,Values=$EIP_NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
    --query 'Addresses[0].AllocationId' --output text
}

ensure_eip() {
  local alloc attached
  alloc="$(eip_allocation)"
  if [ "$alloc" = None ]; then
    log "allocating Elastic IP $EIP_NAME"
    alloc="$(aws ec2 allocate-address --domain vpc \
      --tag-specifications "ResourceType=elastic-ip,Tags=[{Key=Name,Value=$EIP_NAME},{Key=$TAG_KEY,Value=$TAG_VALUE}]" \
      --query AllocationId --output text)"
  fi
  attached="$(aws ec2 describe-addresses --allocation-ids "$alloc" --query 'Addresses[0].InstanceId' --output text)"
  if [ "$attached" != "$INSTANCE" ]; then
    log "associating $EIP_NAME with $INSTANCE"
    aws ec2 associate-address --allocation-id "$alloc" --instance-id "$INSTANCE" >/dev/null
  fi
}

eip_address() {
  aws ec2 describe-addresses --allocation-ids "$(eip_allocation)" --query 'Addresses[0].PublicIp' --output text
}

private_ip() {
  aws ec2 describe-instances --instance-ids "$(instance_id)" \
    --query 'Reservations[0].Instances[0].PrivateIpAddress' --output text
}

wait_for_ssm() {
  log "waiting for SSM on $INSTANCE"
  until [ "$(aws ssm describe-instance-information --filters "Key=InstanceIds,Values=$INSTANCE" \
    --query 'length(InstanceInformationList)' --output text 2>/dev/null)" = 1 ]; do
    sleep 5
  done
}

# run CMD: a shell command on the box; prints its output.
run() {
  local instance id status=Pending
  instance="$(instance_id)"
  [ -n "$instance" ] || { echo "no $NAME instance: run up first" >&2; return 2; }
  id="$(aws ssm send-command --instance-ids "$instance" --document-name AWS-RunShellScript \
    --parameters "$(jq -n --arg command "$1" '{commands: [$command], executionTimeout: ["3600"]}')" \
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
  [ "$status" = Success ]
}

# to_box FILE DEST: shell lines for the box that put FILE (from this machine) at DEST.
to_box() {
  aws s3 cp --quiet "$1" "s3://$BUCKET/tools/analytics/$(basename "$1")"
  echo "aws s3 cp --quiet s3://$BUCKET/tools/analytics/$(basename "$1") $2"
}

setup() {
  local public="$1" private="$2"
  log "installing ClickHouse $CLICKHOUSE_VERSION and its settings"
  run "set -e
export AWS_REGION=$STATE_REGION
if ! rpm -q clickhouse-server-$CLICKHOUSE_VERSION >/dev/null 2>&1; then
  curl -fsSL https://packages.clickhouse.com/rpm/clickhouse.repo -o /etc/yum.repos.d/clickhouse.repo
  dnf install -y -q clickhouse-server-$CLICKHOUSE_VERSION clickhouse-client-$CLICKHOUSE_VERSION clickhouse-common-static-$CLICKHOUSE_VERSION
fi
mkdir -p /etc/clickhouse-server/tls
if [ ! -f /etc/clickhouse-server/tls/cert.pem ] || ! openssl x509 -in /etc/clickhouse-server/tls/cert.pem -noout -ext subjectAltName | grep -q 'IP Address:$public'; then
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 \
    -subj /CN=$NAME -addext 'subjectAltName=IP:$public,IP:$private,DNS:$NAME' \
    -keyout /etc/clickhouse-server/tls/key.pem -out /etc/clickhouse-server/tls/cert.pem 2>/dev/null
fi
chown -R clickhouse:clickhouse /etc/clickhouse-server/tls
chmod 600 /etc/clickhouse-server/tls/key.pem
$(to_box "$ROOT/deploy/aws/clickhouse/config.xml" /etc/clickhouse-server/config.d/forkrail.xml)
chown clickhouse:clickhouse /etc/clickhouse-server/config.d/forkrail.xml
$(users_lines)
systemctl enable --quiet clickhouse-server
systemctl restart clickhouse-server
for i in \$(seq 60); do curl -sf http://127.0.0.1:8123/ping >/dev/null && break; sleep 1; done
curl -sf http://127.0.0.1:8123/ping >/dev/null || { journalctl -u clickhouse-server --no-pager -n 40; tail -n 40 /var/log/clickhouse-server/clickhouse-server.err.log; exit 1; }
aws s3 cp --quiet /etc/clickhouse-server/tls/cert.pem s3://$BUCKET/$STATE_PREFIX/ca.pem
clickhouse-client --query 'SELECT version()'"
}

# users_lines: shell lines for the box that write users.d/forkrail.xml with each user's password
# hash from SSM, whole: the file is written beside users.d and moved in, so ClickHouse (which reloads
# users.d by itself) never reads half of it.
users_lines() {
  to_box "$ROOT/deploy/aws/clickhouse/users.xml" /tmp/forkrail-users.xml
  cat <<'BOX'
hash() { aws ssm get-parameter --name "$1" --with-decryption --query Parameter.Value --output text | tr -d '\n' | sha256sum | cut -d' ' -f1; }
BOX
  cat <<BOX
ingest=\$(hash $INGEST_PARAM)
reader=\$(hash $READER_PARAM)
buildlog=\$(hash $BUILDLOG_PARAM)
BOX
  cat <<'BOX'
# A password SSM didn't give hashes as the empty string's hash.
for h in "$ingest" "$reader" "$buildlog"; do
  [ "$h" != e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 ] || { echo "a password is missing from SSM" >&2; exit 1; }
done
sed -e "s/@INGEST_SHA256@/$ingest/" -e "s/@READER_SHA256@/$reader/" -e "s/@BUILDLOG_SHA256@/$buildlog/" /tmp/forkrail-users.xml > /etc/clickhouse-server/forkrail-users.xml.tmp
rm -f /tmp/forkrail-users.xml
chown clickhouse:clickhouse /etc/clickhouse-server/forkrail-users.xml.tmp
chmod 640 /etc/clickhouse-server/forkrail-users.xml.tmp
mv /etc/clickhouse-server/forkrail-users.xml.tmp /etc/clickhouse-server/users.d/forkrail.xml
BOX
}

# users: the users file alone, for a new user or grant; ClickHouse picks it up within seconds.
users() {
  ensure_secret "$INGEST_PARAM"
  ensure_secret "$READER_PARAM"
  ensure_secret "$BUILDLOG_PARAM"
  log "installing the users file"
  run "set -e
export AWS_REGION=$STATE_REGION
$(users_lines)
for i in \$(seq 30); do
  clickhouse-client --query \"SELECT count() FROM system.users WHERE name = 'buildlog'\" | grep -qx 1 && break
  sleep 1
done
clickhouse-client --query \"SELECT name FROM system.users ORDER BY name FORMAT TSV\""
}

apply_schema() {
  log "applying the schema"
  run "set -e
$(to_box "$ROOT/deploy/aws/clickhouse/schema.sql" /tmp/forkrail-schema.sql)
clickhouse-client --multiquery < /tmp/forkrail-schema.sql
clickhouse-client --query \"SELECT name FROM system.tables WHERE database = 'forkrail' ORDER BY name FORMAT TSV\""
}

write_endpoint() {
  local public="$1" private="$2"
  jq -n --arg public "$public" --arg private "$private" --arg region "$STATE_REGION" \
    --argjson postgres_port "$POSTGRES_PORT" --arg postgres_database "$POSTGRES_DATABASE" --arg postgres_user "$POSTGRES_USER" \
    '{region: $region, private_ip: $private, public_ip: $public, http_port: 8123, https_port: 8443,
      database: "forkrail", ingest_user: "ingest", reader_user: "reader",
      postgres_port: $postgres_port, postgres_database: $postgres_database, postgres_user: $postgres_user}' \
    | aws s3 cp --quiet - "s3://$BUCKET/$STATE_PREFIX/endpoint.json"
}

# scram_verifier: the dashboard's Postgres password (from SSM) as the SCRAM-SHA-256 verifier
# Postgres stores, so the password itself is never sent to the box or printed.
scram_verifier() {
  aws ssm get-parameter --name "$POSTGRES_PARAM" --with-decryption --query Parameter.Value --output text \
    | tr -d '\n' | python3 -c '
import base64, hashlib, hmac, os, sys
password = sys.stdin.buffer.read()
if len(password) < 24:
    sys.exit("the Postgres password is missing from SSM")
salt = os.urandom(16)
salted = hashlib.pbkdf2_hmac("sha256", password, salt, 4096)
stored = hashlib.sha256(hmac.new(salted, b"Client Key", "sha256").digest()).digest()
server = hmac.new(salted, b"Server Key", "sha256").digest()
b64 = lambda raw: base64.b64encode(raw).decode()
print(f"SCRAM-SHA-256$4096:{b64(salt)}${b64(stored)}:{b64(server)}")'
}

# ensure_backup_expiry: the bucket deletes Postgres backups after BACKUP_DAYS. The bucket's other
# lifecycle rules are read first and kept.
ensure_backup_expiry() {
  local out rules wanted
  if out="$(aws s3api get-bucket-lifecycle-configuration --bucket "$BUCKET" --query Rules --output json 2>&1)"; then
    rules="$out"
  elif grep -q NoSuchLifecycleConfiguration <<<"$out"; then
    rules='[]'
  else
    echo "$out" >&2
    return 1
  fi
  wanted="$(jq -n --arg prefix "$BACKUP_PREFIX/" --argjson days "$BACKUP_DAYS" \
    '{ID: "forkrail-postgres-backups", Status: "Enabled", Filter: {Prefix: $prefix}, Expiration: {Days: $days}}')"
  if jq -e --argjson wanted "$wanted" 'any(.[]; .ID == $wanted.ID and .Status == "Enabled" and .Filter.Prefix == $wanted.Filter.Prefix and .Expiration.Days == $wanted.Expiration.Days)' <<<"$rules" >/dev/null; then
    return
  fi
  log "bucket rule: delete $BACKUP_PREFIX/ after $BACKUP_DAYS days"
  aws s3api put-bucket-lifecycle-configuration --bucket "$BUCKET" --lifecycle-configuration "$(jq \
    --argjson wanted "$wanted" '{Rules: ([.[] | select(.ID != $wanted.ID)] + [$wanted])}' <<<"$rules")"
}

# postgres: install or update Postgres on the box (deploy/aws/postgres/install.sh does the work).
postgres() {
  local verifier file lines=""
  ensure_role
  ensure_secret "$POSTGRES_PARAM" "$ENV_KEY" "forkrail dashboard's Postgres password"
  verifier="$(scram_verifier)"
  allow_postgres
  ensure_backup_expiry
  for file in install.sh restore.sh forkrail.conf pg_hba.conf backup.sh forkrail-pg-backup.service forkrail-pg-backup.timer; do
    lines+="$(to_box "$ROOT/deploy/aws/postgres/$file" "\$files/$file")"$'\n'
  done
  log "installing Postgres and its settings"
  run "set -e
export AWS_REGION=$STATE_REGION
files=\$(mktemp -d)
trap 'rm -rf \$files' EXIT
$lines
install -m 755 \$files/restore.sh /usr/local/bin/forkrail-pg-restore
BUCKET=$BUCKET VERIFIER='$verifier' FILES=\$files bash \$files/install.sh"
  aws s3 cp --quiet "s3://$BUCKET/$STATE_PREFIX/endpoint.json" - \
    | jq --argjson port "$POSTGRES_PORT" --arg database "$POSTGRES_DATABASE" --arg user "$POSTGRES_USER" \
      '. + {postgres_port: $port, postgres_database: $database, postgres_user: $user}' \
    | aws s3 cp --quiet - "s3://$BUCKET/$STATE_PREFIX/endpoint.json"
}

# newest_backup: the key of the newest Postgres backup in S3.
newest_backup() {
  aws s3api list-objects-v2 --bucket "$BUCKET" --prefix "$BACKUP_PREFIX/" \
    --query 'sort_by(Contents || `[]`, &Key)[-1].Key' --output text
}

# meta: the routing table's deployments and custom domains into forkrail.meta_* (see schema.sql).
meta() {
  local sql
  sql="$(sed -e "s|@BUCKET@|$BUCKET|g" -e "s|@REGION@|$STATE_REGION|g" "$ROOT/deploy/aws/clickhouse/meta.sql")"
  log "loading the routing table"
  run "set -e
cat > /tmp/forkrail-meta.sql <<'SQL'
$sql
SQL
clickhouse-client --multiquery < /tmp/forkrail-meta.sql
clickhouse-client --query 'SELECT count() FROM forkrail.meta_deployments FINAL'"
}

# The routers' instances and regions, as ClickHouse arrays: ['i-1', 'i-2'] and ['us-east-1', ...].
router_instances() {
  local region ids=() regions=() id
  for region in $STATE_REGION $OTHER_ROUTER_REGIONS; do
    for id in $(aws ec2 describe-instances --region "$region" \
      --filters "Name=tag:Name,Values=$ROUTER_NAME" "Name=tag:$TAG_KEY,Values=$TAG_VALUE" \
      --query 'Reservations[].Instances[].InstanceId' --output text); do
      ids+=("'$id'")
      regions+=("'$region'")
    done
  done
  local IFS=,
  echo "[${ids[*]}] [${regions[*]}]"
}

# backfill DAY...: those days of the S3 log export into the tables, up to when Vector took over on
# each router. The first run notes that moment, as the earliest row Vector sent from each router,
# so run it only once Vector is running on all of them. See clickhouse/backfill.sql.
backfill() {
  local day sql base instances regions missing
  for day in "$@"; do
    [[ "$day" =~ ^[0-9]{4}/[0-9]{2}/[0-9]{2}$ ]] || { echo "DAY is YYYY/MM/DD, not $day" >&2; return 2; }
  done
  base="$(aws s3 cp --quiet "s3://$BUCKET/state/settings.json" - | jq -r '.base_domain // empty')"
  read -r instances regions <<<"$(router_instances)"
  meta
  missing="$(run "clickhouse-client --query \"
    INSERT INTO forkrail.backfill_cutover
    SELECT router, min(t) FROM (
      SELECT router, min(time) AS t FROM forkrail.requests GROUP BY router
      UNION ALL SELECT router, min(time) FROM forkrail.app_logs GROUP BY router
      UNION ALL SELECT router, min(time) FROM forkrail.invocations GROUP BY router
      UNION ALL SELECT router, min(time) FROM forkrail.router_events GROUP BY router)
    WHERE router != '' AND (SELECT count() FROM forkrail.backfill_cutover) = 0
    GROUP BY router\"
clickhouse-client --query \"SELECT arrayStringConcat(arrayFilter(i -> i NOT IN (SELECT router FROM forkrail.backfill_cutover), $instances), ' ')\"")"
  if [ -n "$(tr -d '[:space:]' <<<"$missing")" ]; then
    echo "no cutover for router(s) $missing: Vector hasn't sent anything from them yet" >&2
    return 1
  fi
  run "clickhouse-client --query 'SELECT router, cutover FROM forkrail.backfill_cutover FINAL ORDER BY router FORMAT PrettyCompactMonoBlock'"
  sql="$(sed -e "s|@BUCKET@|$BUCKET|g" -e "s|@REGION@|$STATE_REGION|g" -e "s|@BASE@|$base|g" \
    -e "s|@INSTANCES@|$instances|g" -e "s|@INSTANCE_REGIONS@|$regions|g" "$ROOT/deploy/aws/clickhouse/backfill.sql")"
  for day in "$@"; do
    log "backfilling $day"
    run "set -e
cat > /tmp/forkrail-backfill.sql <<'SQL'
${sql//@DAY@/$day}
SQL
time clickhouse-client --multiquery < /tmp/forkrail-backfill.sql"
  done
}

status() {
  run "set -e
df -h / | tail -1
free -m | sed -n 2p
uptime
clickhouse-client --query \"SELECT table, formatReadableQuantity(sum(rows)) AS rows,
  formatReadableSize(sum(data_compressed_bytes)) AS compressed, formatReadableSize(sum(data_uncompressed_bytes)) AS raw,
  count() AS parts FROM system.parts WHERE active AND database = 'forkrail' GROUP BY table ORDER BY table FORMAT PrettyCompactMonoBlock\"
if systemctl is-active --quiet postgresql; then
  runuser -u postgres -- psql -qAt -c \"SELECT 'postgres ' || current_setting('server_version') || ': forkrail is ' || pg_size_pretty(pg_database_size('$POSTGRES_DATABASE')) || ', ' || (SELECT count(*) FROM pg_stat_activity WHERE datname = '$POSTGRES_DATABASE') || ' connections'\"
  systemctl list-timers forkrail-pg-backup.timer --no-pager | sed -n 2p
else
  echo 'postgres: not installed (analytics.sh postgres)'
fi"
}

down() {
  local instance alloc tag sg
  instance="$(instance_id)"
  if [ -n "$instance" ]; then
    log "terminating $instance"
    aws ec2 terminate-instances --instance-ids "$instance" >/dev/null
    aws ec2 wait instance-terminated --instance-ids "$instance"
  fi
  alloc="$(eip_allocation)"
  if [ "$alloc" != None ]; then
    log "releasing $EIP_NAME"
    aws ec2 release-address --allocation-id "$alloc"
  fi
  sg="$(security_group)"
  if [ "$sg" != None ]; then
    tag="$(aws ec2 describe-security-groups --group-ids "$sg" --query "SecurityGroups[0].Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)"
    if [ "$tag" = "$TAG_VALUE" ]; then
      log "deleting security group $sg"
      aws ec2 delete-security-group --group-id "$sg"
    fi
  fi
  if aws iam get-role --role-name "$NAME" >/dev/null 2>&1; then
    tag="$(aws iam list-role-tags --role-name "$NAME" --query "Tags[?Key=='$TAG_KEY'].Value | [0]" --output text)"
    if [ "$tag" = "$TAG_VALUE" ]; then
      log "deleting role $NAME"
      aws iam remove-role-from-instance-profile --instance-profile-name "$NAME" --role-name "$NAME" 2>/dev/null || true
      aws iam delete-instance-profile --instance-profile-name "$NAME" 2>/dev/null || true
      aws iam detach-role-policy --role-name "$NAME" --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore
      aws iam delete-role-policy --role-name "$NAME" --policy-name "$NAME"
      aws iam delete-role --role-name "$NAME"
    fi
  fi
}

case "${1:-}" in
  up)
    read -r zone router_sg <<<"$(router_instance)"
    [ -n "${zone:-}" ] && [ "$zone" != None ] || { echo "no $ROUTER_NAME in $STATE_REGION" >&2; exit 2; }
    ensure_role
    ensure_secret "$INGEST_PARAM"
    ensure_secret "$READER_PARAM"
    ensure_secret "$BUILDLOG_PARAM"
    ensure_security_group "$router_sg"
    ensure_instance "$zone"
    ensure_eip
    wait_for_ssm
    public="$(eip_address)"
    private="$(private_ip)"
    setup "$public" "$private"
    apply_schema
    write_endpoint "$public" "$private"
    postgres
    log "ClickHouse up: http://$private:8123 (the router's VPC), https://$public:8443"
    ;;
  role) ensure_role ;;
  schema) apply_schema ;;
  users)
    ensure_role
    users
    ;;
  sql)
    [ $# -eq 2 ] || { echo "usage: $0 sql 'QUERY'" >&2; exit 2; }
    run "clickhouse-client --format PrettyCompactMonoBlock --query $(printf %q "$2")"
    ;;
  status) status ;;
  meta) meta ;;
  backfill)
    shift
    [ $# -ge 1 ] || { echo "usage: $0 backfill YYYY/MM/DD..." >&2; exit 2; }
    backfill "$@"
    ;;
  postgres) postgres ;;
  psql)
    [ $# -eq 2 ] || { echo "usage: $0 psql 'SQL'" >&2; exit 2; }
    run "runuser -u postgres -- psql -d $POSTGRES_DATABASE -c $(printf %q "$2")"
    ;;
  pg-backup) run /usr/local/bin/forkrail-pg-backup ;;
  pg-backups) aws s3 ls "s3://$BUCKET/$BACKUP_PREFIX/" ;;
  pg-restore-check)
    key="${2:-$(newest_backup)}"
    [ -n "$key" ] && [ "$key" != None ] || { echo "no backups under s3://$BUCKET/$BACKUP_PREFIX/ yet" >&2; exit 1; }
    log "restoring $key into a scratch database"
    run "/usr/local/bin/forkrail-pg-restore check $BUCKET $(printf %q "$key")"
    ;;
  pg-restore)
    [ $# -eq 3 ] && [ "$3" = --yes ] || { echo "usage: $0 pg-restore KEY --yes (replaces the live database; pg-backups lists keys)" >&2; exit 2; }
    log "replacing the live database with $2"
    run "/usr/local/bin/forkrail-pg-restore replace $BUCKET $(printf %q "$2")"
    ;;
  allow-me) allow_me ;;
  egress) egress ;;
  run)
    [ $# -eq 2 ] || { echo "usage: $0 run 'COMMAND'" >&2; exit 2; }
    run "$2"
    ;;
  ip) eip_address ;;
  down) down ;;
  *)
    sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
