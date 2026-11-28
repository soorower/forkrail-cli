#!/bin/bash
# Runs as root on the analytics box, through SSM Run Command, when `forkrail install` sets it up
# (crates/cli/src/install/boxes.rs). ClickHouse for logs and metrics, as deploy/aws/analytics.sh
# `up` makes it on an install set up by hand: the same version, settings, users and schema.
# Postgres isn't installed: the dashboard keeps its people in its DynamoDB table.
# Safe to run again: it installs what's missing and writes the settings again.
#
#   BUCKET              the install's bucket
#   REGION              its region
#   PUBLIC_IP           the box's Elastic IP, and
#   PRIVATE_IP          its private address, both in its certificate
#   FILES               the folder with config.xml, users.xml and schema.sql
#   CLICKHOUSE_VERSION  the server's version
set -euo pipefail
: "${BUCKET:?}" "${REGION:?}" "${PUBLIC_IP:?}" "${PRIVATE_IP:?}" "${FILES:?}" "${CLICKHOUSE_VERSION:?}"
export AWS_REGION="$REGION"
NAME=forkrail-analytics

step() { echo "--- $*"; }

step "ClickHouse $CLICKHOUSE_VERSION"
if ! rpm -q "clickhouse-server-$CLICKHOUSE_VERSION" >/dev/null 2>&1; then
  curl -fsSL --retry 3 https://packages.clickhouse.com/rpm/clickhouse.repo -o /etc/yum.repos.d/clickhouse.repo
  dnf install -y -q "clickhouse-server-$CLICKHOUSE_VERSION" "clickhouse-client-$CLICKHOUSE_VERSION" "clickhouse-common-static-$CLICKHOUSE_VERSION"
fi

step "its certificate (for $PUBLIC_IP and $PRIVATE_IP)"
mkdir -p /etc/clickhouse-server/tls
if [ ! -f /etc/clickhouse-server/tls/cert.pem ] || ! openssl x509 -in /etc/clickhouse-server/tls/cert.pem -noout -ext subjectAltName | grep -q "IP Address:$PUBLIC_IP"; then
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 \
    -subj "/CN=$NAME" -addext "subjectAltName=IP:$PUBLIC_IP,IP:$PRIVATE_IP,DNS:$NAME" \
    -keyout /etc/clickhouse-server/tls/key.pem -out /etc/clickhouse-server/tls/cert.pem 2>/dev/null
fi
chown -R clickhouse:clickhouse /etc/clickhouse-server/tls
chmod 600 /etc/clickhouse-server/tls/key.pem

step "settings and users"
install -o clickhouse -g clickhouse -m 644 "$FILES/config.xml" /etc/clickhouse-server/config.d/forkrail.xml
hash() { aws ssm get-parameter --name "$1" --with-decryption --query Parameter.Value --output text | tr -d '\n' | sha256sum | cut -d' ' -f1; }
ingest=$(hash /forkrail/clickhouse/ingest)
reader=$(hash /forkrail/clickhouse/reader)
buildlog=$(hash /forkrail/clickhouse/buildlog)
# A password SSM didn't give hashes as the empty string's hash.
for h in "$ingest" "$reader" "$buildlog"; do
  [ "$h" != e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 ] || { echo "a ClickHouse password is missing from SSM" >&2; exit 1; }
done
sed -e "s/@INGEST_SHA256@/$ingest/" -e "s/@READER_SHA256@/$reader/" -e "s/@BUILDLOG_SHA256@/$buildlog/" "$FILES/users.xml" > /etc/clickhouse-server/forkrail-users.xml.tmp
chown clickhouse:clickhouse /etc/clickhouse-server/forkrail-users.xml.tmp
chmod 640 /etc/clickhouse-server/forkrail-users.xml.tmp
mv /etc/clickhouse-server/forkrail-users.xml.tmp /etc/clickhouse-server/users.d/forkrail.xml

step "starting ClickHouse"
systemctl enable --quiet clickhouse-server
systemctl restart clickhouse-server
for _ in $(seq 90); do curl -sf http://127.0.0.1:8123/ping >/dev/null && break; sleep 1; done
if ! curl -sf http://127.0.0.1:8123/ping >/dev/null; then
  echo "ClickHouse didn't start:" >&2
  journalctl -u clickhouse-server --no-pager -n 30 >&2
  tail -n 30 /var/log/clickhouse-server/clickhouse-server.err.log >&2 || true
  exit 1
fi

step "the schema"
clickhouse-client --multiquery < "$FILES/schema.sql"
for _ in $(seq 30); do
  clickhouse-client --query "SELECT count() FROM system.users WHERE name = 'buildlog'" | grep -qx 1 && break
  sleep 1
done
clickhouse-client --query "SELECT count() FROM system.users WHERE name IN ('ingest', 'reader', 'buildlog')" | grep -qx 3 || { echo "ClickHouse didn't load the users file" >&2; exit 1; }

step "the certificate for the routers and the dashboard"
aws s3 cp --quiet /etc/clickhouse-server/tls/cert.pem "s3://$BUCKET/state/analytics/ca.pem"
echo "ClickHouse $(clickhouse-client --query 'SELECT version()') ready, tables: $(clickhouse-client --query "SELECT count() FROM system.tables WHERE database = 'forkrail'")"
