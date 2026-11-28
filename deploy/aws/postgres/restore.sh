#!/usr/bin/env bash
# Runs on the analytics box as root (deploy/aws/analytics.sh pg-restore-check | pg-restore): a
# backup from S3 back into Postgres.
#
#   restore.sh check BUCKET KEY     restore into a scratch database, print each table's rows beside
#                                   the live database's, drop the scratch database. Changes nothing
#   restore.sh replace BUCKET KEY   put the live database back as the backup has it, in one
#                                   transaction: every table is dropped and made again
set -euo pipefail

mode="$1" bucket="$2" key="$3"
export AWS_REGION=us-east-1
DATABASE=forkrail
SCRATCH=forkrail_restore_check
file="$(mktemp /var/tmp/forkrail-pg-restore.XXXXXX)"
trap 'rm -f "$file"' EXIT
chmod 644 "$file"

sql() { runuser -u postgres -- psql -v ON_ERROR_STOP=1 -qAt "$@"; }

# rows DB: "table rows" for every table of ours, counted (statistics are empty after a restore).
rows() {
  local table
  for table in $(sql -d "$1" -c "SELECT quote_ident(schemaname) || '.' || quote_ident(tablename) FROM pg_tables
    WHERE schemaname NOT IN ('pg_catalog', 'information_schema') ORDER BY 1"); do
    echo "$table $(sql -d "$1" -c "SELECT count(*) FROM $table")"
  done
}

aws s3 cp --quiet "s3://$bucket/$key" "$file"
case "$mode" in
  check)
    sql -c "DROP DATABASE IF EXISTS $SCRATCH"
    sql -c "CREATE DATABASE $SCRATCH"
    runuser -u postgres -- pg_restore --exit-on-error --dbname "$SCRATCH" "$file"
    echo "table backup live"
    join -a 1 -a 2 -e - -o 0,1.2,2.2 <(rows "$SCRATCH" | sort) <(rows "$DATABASE" | sort)
    sql -c "DROP DATABASE $SCRATCH"
    ;;
  replace)
    runuser -u postgres -- pg_restore --exit-on-error --single-transaction --clean --if-exists --dbname "$DATABASE" "$file"
    rows "$DATABASE"
    ;;
  *)
    echo "usage: restore.sh check|replace BUCKET KEY" >&2
    exit 2
    ;;
esac
