#!/usr/bin/env bash
# One backup of forkrail's Postgres database to S3 (forkrail-pg-backup.timer runs it every 6
# hours). pg_dump's custom format: compressed, and restored whole or table by table with
# pg_restore (deploy/aws/analytics.sh pg-restore). The bucket's lifecycle rule deletes backups
# after 30 days. Roles aren't in a dump: `analytics.sh postgres` makes the dashboard's again.
set -euo pipefail

BUCKET=@BUCKET@
export AWS_REGION=us-east-1
key="backups/postgres/forkrail-$(date -u +%Y%m%dT%H%M%SZ).dump"
file="$(mktemp /var/tmp/forkrail-pg-backup.XXXXXX)"
trap 'rm -f "$file"' EXIT

runuser -u postgres -- pg_dump --format=custom forkrail > "$file"
# A dump pg_restore can't list isn't a backup.
pg_restore --list "$file" >/dev/null
aws s3 cp --quiet "$file" "s3://$BUCKET/$key"
echo "$key $(stat -c %s "$file") bytes"
