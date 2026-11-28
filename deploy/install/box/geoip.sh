#!/bin/bash
# Runs on the router (deploy/install/box/router.sh): the GeoIP databases and the daily timer that
# keeps them current, as deploy/aws/test-env.sh install_geoip does. Keep the two the same.
set -euo pipefail
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
