#!/bin/bash
# Runs every 10 minutes on the router instance (forkrail-log-export.timer), as the forkrail user:
# 1. the router's log lines since the last run, from its hourly files in LOG_DIR, as JSON lines
#    (log-export.py), to s3://BUCKET/logs/router/YYYY/MM/DD/HHMMSS-<instance>.jsonl.gz, so the logs
#    outlive the instance;
# 2. requests, 5xx and p95 on the public HTTPS listener for those 10 minutes, as CloudWatch metrics
#    in the `forkrail` namespace, which the 5xx alarm watches; and on the issuing router, the days
#    left on the soonest certificate to expire, which the expiry alarm watches;
# 3. past hours that were sent in full compressed, and compressed ones older than two days deleted.
# The files' positions are kept only once the upload worked, so a failed run is sent again next
# time and nothing is lost or sent twice.
#
#   log-export.sh BUCKET [LOG_DIR]
set -euo pipefail

bucket="$1"
logs="${2:-/var/log/forkrail}"
dir=/var/lib/forkrail-log-export
mkdir -p "$dir"
state="$dir/positions.json" batch="$dir/batch.jsonl"
[ -d "$logs" ] || { echo "no $logs yet"; exit 0; }

summary="$(python3 /opt/forkrail/log-export.py collect "$logs" "$state" "$batch")"

if [ -s "$batch" ]; then
  token=$(curl -sf -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
  instance=$(curl -sf -H "X-aws-ec2-metadata-token: $token" http://169.254.169.254/latest/meta-data/instance-id)
  key="logs/router/$(date -u +%Y/%m/%d/%H%M%S)-$instance.jsonl.gz"
  gzip -c "$batch" | aws s3 cp --quiet --content-type application/gzip - "s3://$bucket/$key"
fi
mv -f "$state.next" "$state"
rm -f "$batch"
python3 /opt/forkrail/log-export.py tidy "$logs" "$state"

data="$(python3 -c '
import json, sys
s = json.loads(sys.argv[1])
metrics = [{"MetricName": "Requests", "Value": s["requests"], "Unit": "Count"},
           {"MetricName": "Errors5xx", "Value": s["errors_5xx"], "Unit": "Count"}]
if s["p95_ms"] is not None:
    metrics.append({"MetricName": "P95", "Value": s["p95_ms"], "Unit": "Milliseconds"})
if s.get("cert_days_left") is not None:
    metrics.append({"MetricName": "CertificateDaysLeft", "Value": s["cert_days_left"], "Unit": "None"})
metrics.append({"MetricName": "MitigatedRequests", "Value": s.get("mitigated", 0), "Unit": "Count"})
print(json.dumps(metrics))' "$summary")"
aws cloudwatch put-metric-data --namespace forkrail --metric-data "$data"
echo "exported: $summary"
