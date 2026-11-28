#!/bin/bash
# EC2 user data for the forkrail test router (Amazon Linux 2023, arm64).
# test-env.sh fills in @BUCKET@, @FUNCTION@, @REGION@ and @STATE_REGION@ before launch.
set -euo pipefail

useradd --system --shell /sbin/nologin forkrail || true
mkdir -p /opt/forkrail /etc/forkrail

cat > /etc/forkrail/router.env <<'EOF'
AWS_REGION=@REGION@
FORKRAIL_STATE_REGION=@STATE_REGION@
FORKRAIL_HOME_REGION=@STATE_REGION@
FORKRAIL_ROUTES=s3://@BUCKET@/state/routes.json
FORKRAIL_FUNCTION=@FUNCTION@
RUST_LOG=info
EOF

# Workers outside a VPC dial the router's public IP: its Elastic IP once test-env.sh has attached one.
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

# Kept the same as router_unit in test-env.sh, which writes it again on push and domain. A template:
# restarts start @blue or @green beside the other (ROUTER_RESTART in test-env.sh).
cat > /etc/systemd/system/forkrail-router@.service <<'EOF'
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
# the verified crawlers' published ranges (bots.rs), pages (cache/disk.rs), and each deployment's
# busiest hours, for keep-warm's auto across restarts (keepwarm.rs).
StateDirectory=forkrail/redirects forkrail/bots forkrail/pages forkrail/keepwarm
EnvironmentFile=/etc/forkrail/router.env
# The binary push staged goes in place; without one (after a reboot, say), the latest build comes from S3.
ExecStartPre=+/bin/sh -c 'if [ -f /opt/forkrail/forkrail-router.new ]; then mv -f /opt/forkrail/forkrail-router.new /opt/forkrail/forkrail-router; else /usr/bin/aws s3 --region @STATE_REGION@ cp --quiet s3://@BUCKET@/forkrail-router /opt/forkrail/forkrail-router; fi; chmod 755 /opt/forkrail/forkrail-router'
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
EOF

# Connections queued on a router that stops are handed to the one beside it, not reset.
echo 'net.ipv4.tcp_migrate_req = 1' > /etc/sysctl.d/90-forkrail.conf
sysctl -q -p /etc/sysctl.d/90-forkrail.conf

# TCP for users far away; test-env.sh puts deploy/aws/tcp-tuning.sh here.
@TCP_TUNING@

systemctl daemon-reload
systemctl enable --now forkrail-router@blue
