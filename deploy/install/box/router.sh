#!/bin/bash
# Runs as root on the router, through SSM Run Command, when `forkrail install` sets it up
# (crates/cli/src/install/boxes.rs): the router as deploy/aws/router-init.sh and test-env.sh `push`
# make it on an install set up by hand. Its systemd unit (forkrail-router@blue and @green, for
# restarts with no gap), the TCP settings, the log export to S3 every 10 minutes, the GeoIP
# databases, Vector (the router's log files to the analytics box), then the router itself.
# Safe to run again: it writes everything again and restarts the router beside the running one.
#
#   BUCKET          the install's bucket
#   REGION          its region (the router's too)
#   FILES           the folder with router.env, tcp-tuning.sh, log-export.sh, log-export.py and
#                   vector.yaml
#   VECTOR_KEY      Vector's tarball in the bucket, and
#   VECTOR_SHA256   its SHA-256
#   CLICKHOUSE_URL  the analytics box, as the router's log sink and Vector reach it
set -euo pipefail
: "${BUCKET:?}" "${REGION:?}" "${FILES:?}" "${VECTOR_KEY:?}" "${VECTOR_SHA256:?}" "${CLICKHOUSE_URL:?}"
export AWS_REGION="$REGION"

step() { echo "--- $*"; }

imds() {
  local token
  token=$(curl -sf -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
  curl -sf -H "X-aws-ec2-metadata-token: $token" "http://169.254.169.254/latest/meta-data/$1"
}
INSTANCE=$(imds instance-id)

step "the router's user, settings and unit"
useradd --system --shell /sbin/nologin forkrail 2>/dev/null || true
mkdir -p /opt/forkrail /etc/forkrail
{
  cat "$FILES/router.env"
  echo "FORKRAIL_ROUTER_ID=$INSTANCE"
} > /etc/forkrail/router.env.new
chmod 644 /etc/forkrail/router.env.new
mv -f /etc/forkrail/router.env.new /etc/forkrail/router.env

# Workers outside a VPC dial the router's public address: its Elastic IP.
cat > /opt/forkrail/start.sh <<'EOF'
#!/bin/bash
set -euo pipefail
imds=http://169.254.169.254/latest
token=$(curl -sf -X PUT "$imds/api/token" -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
ip=$(curl -sf -H "X-aws-ec2-metadata-token: $token" "$imds/meta-data/public-ipv4")
export FORKRAIL_TUNNEL_ADDR="$ip:7443"
exec /opt/forkrail/forkrail-router
EOF
chmod 755 /opt/forkrail/start.sh

cat > /etc/systemd/system/forkrail-router@.service <<UNIT
[Unit]
Description=forkrail router (%i)
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
# Ready once every listener is up, HTTPS included; only then does a restart stop the old router.
# HTTPS waits for the domain's first certificate from Let's Encrypt, which can take minutes.
Type=notify
NotifyAccess=main
TimeoutStartSec=600
# Stopping closes the listeners at once, then lets requests in flight finish (up to 30 s).
TimeoutStopSec=45
User=forkrail
# Two routers at once each take one of the tunnel ports; the log goes to hourly files.
Environment=FORKRAIL_TUNNEL_LISTEN=0.0.0.0:7443,0.0.0.0:7444
Environment=FORKRAIL_LOG_DIR=/var/log/forkrail
Environment=MALLOC_MMAP_THRESHOLD_=1048576
LogsDirectory=forkrail
StateDirectory=forkrail/redirects forkrail/bots forkrail/pages forkrail/keepwarm
EnvironmentFile=/etc/forkrail/router.env
# The binary staged beside it goes in place; without one (after a reboot, say), the latest from S3.
ExecStartPre=+/bin/sh -c 'if [ -f /opt/forkrail/forkrail-router.new ]; then mv -f /opt/forkrail/forkrail-router.new /opt/forkrail/forkrail-router; else /usr/bin/aws s3 --region $REGION cp --quiet s3://$BUCKET/forkrail-router /opt/forkrail/forkrail-router; fi; chmod 755 /opt/forkrail/forkrail-router'
ExecStart=/opt/forkrail/start.sh
AmbientCapabilities=CAP_NET_BIND_SERVICE
OOMScoreAdjust=-900
Restart=always
RestartSec=2
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT

step "TCP settings"
# Connections queued on a router that stops are handed to the one beside it, not reset.
echo 'net.ipv4.tcp_migrate_req = 1' > /etc/sysctl.d/90-forkrail.conf
sysctl -q -p /etc/sysctl.d/90-forkrail.conf
bash "$FILES/tcp-tuning.sh" || echo "tcp: the tuning didn't apply; the router runs with Linux's defaults" >&2

step "the log export (S3 and CloudWatch, every 10 minutes)"
install -m 755 "$FILES/log-export.sh" /opt/forkrail/log-export.sh
install -m 644 "$FILES/log-export.py" /opt/forkrail/log-export.py
cat > /etc/systemd/system/forkrail-log-export.service <<UNIT
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
Environment=AWS_REGION=$REGION
ExecStart=/opt/forkrail/log-export.sh $BUCKET
UNIT
cat > /etc/systemd/system/forkrail-log-export.timer <<'UNIT'
[Unit]
Description=forkrail log export every 10 minutes

[Timer]
OnCalendar=*:0/10
Persistent=true
AccuracySec=10s

[Install]
WantedBy=timers.target
UNIT
systemctl daemon-reload
systemctl enable --quiet --now forkrail-log-export.timer

step "GeoIP databases (DB-IP Lite, CC BY 4.0)"
if bash "$FILES/geoip.sh"; then :; else echo "GeoIP: not installed this time; visitors' locations stay empty until the daily timer gets them" >&2; fi

step "the router's log sink (the analytics box)"
(umask 077; aws ssm get-parameter --name /forkrail/clickhouse/ingest --with-decryption --query Parameter.Value --output text > /etc/forkrail/ch_password.new)
chown forkrail:forkrail /etc/forkrail/ch_password.new
mv -f /etc/forkrail/ch_password.new /etc/forkrail/ch_password

step "Vector"
if ! /opt/vector/bin/vector --version 2>/dev/null | grep -q "^vector $(basename "$VECTOR_KEY" | sed -n 's/^vector-\([0-9.]*\)-.*/\1/p') "; then
  aws s3 cp --quiet "s3://$BUCKET/$VECTOR_KEY" /tmp/vector.tar.gz
  echo "$VECTOR_SHA256  /tmp/vector.tar.gz" | sha256sum -c --quiet
  rm -rf /opt/vector.new && mkdir -p /opt/vector.new
  tar xzf /tmp/vector.tar.gz -C /opt/vector.new --strip-components=2
  rm -rf /opt/vector && mv /opt/vector.new /opt/vector && rm -f /tmp/vector.tar.gz
fi
mkdir -p /etc/vector
grep -v '^#TLS#$' "$FILES/vector.yaml" | sed -e "s|@REGION@|$REGION|" -e "s|@INSTANCE@|$INSTANCE|" -e "s|@ENDPOINT@|$CLICKHOUSE_URL|" > /etc/vector/vector.yaml
chmod 644 /etc/vector/vector.yaml
install -m 600 /etc/forkrail/ch_password /etc/vector/ch_password
chown root:root /etc/vector/ch_password
cat > /etc/systemd/system/forkrail-vector.service <<'UNIT'
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
ExecReload=/bin/kill -HUP $MAINPID
OOMScoreAdjust=500
MemoryMax=400M
Nice=5
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --quiet forkrail-vector

step "the router"
aws s3 cp --quiet "s3://$BUCKET/forkrail-router" /opt/forkrail/forkrail-router.new
chmod 755 /opt/forkrail/forkrail-router.new
active=""
for color in blue green; do systemctl is-active --quiet "forkrail-router@$color" && active=$color; done
next=blue; [ "$active" = blue ] && next=green
started=$(date +%s%N)
if ! systemctl start "forkrail-router@$next"; then
  echo "the router didn't get ready:" >&2
  journalctl -u "forkrail-router@$next" --no-pager -n 40 >&2
  systemctl stop "forkrail-router@$next" 2>/dev/null || true
  exit 1
fi
systemctl enable --quiet "forkrail-router@$next"
if [ -n "$active" ]; then
  systemctl disable --quiet "forkrail-router@$active"
  systemctl stop "forkrail-router@$active"
fi
echo "router: forkrail-router@$next serving, ready after $(( ($(date +%s%N) - started) / 1000000 )) ms"

# Vector starts once the router has written its first log file.
systemctl restart forkrail-vector
sleep 3
systemctl is-active --quiet forkrail-vector || { echo "Vector didn't start:" >&2; journalctl -u forkrail-vector --no-pager -n 20 >&2; exit 1; }
echo "vector: running"
