# Shell lines, run as root on any host that sends responses to users far away, such as the
# router. test-env.sh pastes them into the router's user data and `push`.
# Safe to run again, and on a host serving users: nothing is dropped or reconnected.
#
# Measured on 2026-11-07 from probes in 9 AWS regions and 11 countries, 1 MB responses:
# 1. Keep a connection's window when it goes quiet. Linux's default drops it back to 10 packets
#    (about 14 KB) after about one retransmit timeout idle, so a kept-alive connection starts slow
#    again. Reused connections from Frankfurt went from 196 to 118 ms, from Dhaka 624 to 365 ms.
# 2. Start new connections at 64 packets (about 90 KB) instead of 10, so 1 MB far away takes about
#    4 round trips instead of 7. Sydney through Global Accelerator went from 1.97 to 1.34 s.
# Congestion control stays cubic, Linux's default: BBR made reused requests 40–50% slower.
cat > /etc/sysctl.d/91-forkrail-tcp.conf <<'SYSCTL'
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_congestion_control = cubic
SYSCTL
sysctl -q -p /etc/sysctl.d/91-forkrail-tcp.conf

# The initial window belongs to the route. systemd 252 (Amazon Linux 2023) can't put it on the
# route DHCP installs ([DHCPv4] InitialCongestionWindow= came in a later systemd), and a window set
# on that route by hand is gone after a reboot, while a lease renewal adds a second route without
# it. So networkd adds a default route of its own through the DHCP gateway, at a lower metric,
# which wins; DHCP's own (metric 512, from amazon-ec2-net-utils) stays behind it as the fallback.
# networkd reads the file at boot. `networkctl reload` would apply it now, but it reconfigures the
# interface: the address and every route went for about 60 ms on the Mumbai test router. So the
# same route goes in by hand for this boot; renewals leave routes they didn't add alone.
tcp_iface="$(ip -o -4 route show default | awk '{print $5; exit}')"
tcp_gateway="$(ip -o -4 route show default | awk '{print $3; exit}')"
if [ -z "$tcp_iface" ] || [ ! -f "/run/systemd/network/70-$tcp_iface.network" ]; then
  echo "tcp: no amazon-ec2-net-utils network file for '$tcp_iface'; initial window left at Linux's default" >&2
else
  mkdir -p "/etc/systemd/network/70-$tcp_iface.network.d"
  cat > "/etc/systemd/network/70-$tcp_iface.network.d/forkrail-tcp.conf" <<'NETWORK'
# forkrail (deploy/aws/tcp-tuning.sh): new TCP connections start at 64 packets, not 10.
[Route]
Gateway=_dhcp4
Metric=100
InitialCongestionWindow=64
InitialAdvertisedReceiveWindow=64
NETWORK
  ip route replace default via "$tcp_gateway" dev "$tcp_iface" proto dhcp metric 100 initcwnd 64 initrwnd 64 ||
    echo "tcp: couldn't add the route; new connections start at Linux's default until the next boot" >&2
fi
echo "tcp: slow_start_after_idle=$(sysctl -n net.ipv4.tcp_slow_start_after_idle)" \
  "congestion=$(sysctl -n net.ipv4.tcp_congestion_control)" \
  "route: $(ip -o -4 route show default | head -1)"
