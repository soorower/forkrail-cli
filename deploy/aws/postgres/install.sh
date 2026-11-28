#!/usr/bin/env bash
# Runs on the analytics box as root (deploy/aws/analytics.sh postgres puts it there): Postgres for
# the dashboard's own data (people, teams, members, sessions), beside ClickHouse, which keeps logs
# and metrics. Safe to run again: it installs what's missing, rewrites the settings, and restarts
# Postgres only when a setting changed.
#
#   BUCKET    the install's bucket, for backups
#   VERIFIER  the dashboard user's password as a SCRAM-SHA-256 verifier. The password itself never
#             comes to this box: it's in SSM, for the dashboard alone
#   FILES     the folder holding forkrail.conf, pg_hba.conf, backup.sh and the two units
set -euo pipefail

: "${BUCKET:?}" "${VERIFIER:?}" "${FILES:?}"
VERSION=18
DATA=/var/lib/pgsql/data
TLS=/var/lib/pgsql/tls
DATABASE=forkrail
ROLE=forkrail_web

sql() { runuser -u postgres -- psql -v ON_ERROR_STOP=1 -qAt "$@"; }

if ! rpm -q "postgresql$VERSION-server" >/dev/null 2>&1; then
  dnf install -y -q "postgresql$VERSION-server"
fi
if [ ! -f "$DATA/PG_VERSION" ]; then
  install -d -o postgres -g postgres -m 700 "$DATA"
  runuser -u postgres -- initdb -D "$DATA" --auth-local=peer --auth-host=scram-sha-256 \
    --encoding=UTF8 --locale=C.UTF-8 >/dev/null
fi

# What a restart depends on, before and after (the files are missing on a first run).
settings() { { cat "$DATA/conf.d/forkrail.conf" "$DATA/pg_hba.conf" "$TLS/cert.pem" 2>/dev/null || true; } | sha256sum; }
before="$(settings)"

# The box's one certificate (the dashboard pins it, as it does for ClickHouse).
install -d -o postgres -g postgres -m 700 "$TLS"
install -o postgres -g postgres -m 644 /etc/clickhouse-server/tls/cert.pem "$TLS/cert.pem"
install -o postgres -g postgres -m 600 /etc/clickhouse-server/tls/key.pem "$TLS/key.pem"

install -d -o postgres -g postgres -m 700 "$DATA/conf.d"
grep -q "^include_dir = 'conf.d'" "$DATA/postgresql.conf" || echo "include_dir = 'conf.d'" >> "$DATA/postgresql.conf"
install -o postgres -g postgres -m 600 "$FILES/forkrail.conf" "$DATA/conf.d/forkrail.conf"
install -o postgres -g postgres -m 600 "$FILES/pg_hba.conf" "$DATA/pg_hba.conf"

after="$(settings)"
systemctl enable --quiet postgresql
if ! systemctl is-active --quiet postgresql; then
  systemctl start postgresql
elif [ "$before" != "$after" ]; then
  systemctl restart postgresql
fi
for _ in $(seq 30); do runuser -u postgres -- pg_isready -q && break; sleep 1; done
runuser -u postgres -- pg_isready -q || { journalctl -u postgresql --no-pager -n 40; exit 1; }

[ "$(sql -c "SELECT 1 FROM pg_roles WHERE rolname = '$ROLE'")" = 1 ] || sql -c "CREATE ROLE $ROLE LOGIN"
sql -v verifier="$VERIFIER" -v role="$ROLE" <<'SQL'
ALTER ROLE :"role" NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION CONNECTION LIMIT 50 PASSWORD :'verifier';
ALTER ROLE :"role" SET statement_timeout = '15s';
ALTER ROLE :"role" SET idle_in_transaction_session_timeout = '30s';
SQL
[ "$(sql -c "SELECT 1 FROM pg_database WHERE datname = '$DATABASE'")" = 1 ] || sql -c "CREATE DATABASE $DATABASE OWNER $ROLE"
sql -c "REVOKE ALL ON DATABASE $DATABASE FROM PUBLIC"

# Backups: a dump to S3 every 6 hours (the bucket's lifecycle rule keeps them 30 days).
sed "s|@BUCKET@|$BUCKET|g" "$FILES/backup.sh" > /usr/local/bin/forkrail-pg-backup
chmod 755 /usr/local/bin/forkrail-pg-backup
install -m 644 "$FILES/forkrail-pg-backup.service" /etc/systemd/system/forkrail-pg-backup.service
install -m 644 "$FILES/forkrail-pg-backup.timer" /etc/systemd/system/forkrail-pg-backup.timer
systemctl daemon-reload
systemctl enable --quiet --now forkrail-pg-backup.timer

sql -c "SELECT 'postgres ' || current_setting('server_version') || ', ssl ' || current_setting('ssl') || ', ' || current_setting('max_connections') || ' connections'"
