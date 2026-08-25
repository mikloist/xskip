#!/usr/bin/env bash
# Phase 1 harness. Run with sudo. Builds a netns + veth pair, runs the AF_XDP
# consumer inside the namespace, then sends MATCHED traffic (UDP with source
# port = tracked server port) and UNMATCHED traffic (ICMP ping). Expectation:
# the consumer prints the matched UDP frames; the ping still gets kernel replies.
set -euo pipefail

NS=rustssi
VETH_HOST=vhost
VETH_NS=vpeer
HOST_IP=10.200.0.1
NS_IP=10.200.0.2
PREFIX=24
SERVER_PORT=6667
BIN="${BIN:-target/debug/rustssi}"

if [[ $EUID -ne 0 ]]; then
	echo "must run as root (sudo $0)" >&2
	exit 1
fi
if [[ ! -x "$BIN" ]]; then
	echo "binary not found at $BIN (run: cargo build)" >&2
	exit 1
fi

cleanup() {
	ip netns del "$NS" 2>/dev/null || true
	ip link del "$VETH_HOST" 2>/dev/null || true
}
trap cleanup EXIT
cleanup

# Topology: consumer runs inside $NS on $VETH_NS. The host side ($VETH_HOST)
# plays the "server": it sends from source port $SERVER_PORT, so the tracked
# endpoint is ($HOST_IP, $SERVER_PORT).
ip netns add "$NS"
ip link add "$VETH_HOST" type veth peer name "$VETH_NS"
ip link set "$VETH_NS" netns "$NS"
ip addr add "$HOST_IP/$PREFIX" dev "$VETH_HOST"
ip link set "$VETH_HOST" up
ip netns exec "$NS" ip addr add "$NS_IP/$PREFIX" dev "$VETH_NS"
ip netns exec "$NS" ip link set "$VETH_NS" up
ip netns exec "$NS" ip link set lo up

echo "== starting consumer inside netns (tracking $HOST_IP:$SERVER_PORT) =="
ip netns exec "$NS" timeout 10 "$BIN" "$VETH_NS" "$HOST_IP" "$SERVER_PORT" &
CONS=$!
sleep 2

echo
echo "== MATCHED: 5x UDP from $HOST_IP:$SERVER_PORT -> $NS_IP:12345 (expect prints) =="
python3 - "$HOST_IP" "$SERVER_PORT" "$NS_IP" <<'PY'
import socket, sys, time
host_ip, sport, ns_ip = sys.argv[1], int(sys.argv[2]), sys.argv[3]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((host_ip, sport))
for i in range(5):
    s.sendto(b"irc-match-%d" % i, (ns_ip, 12345))
    time.sleep(0.1)
PY

echo
echo "== UNMATCHED: ping $NS_IP (expect kernel replies, no consumer prints) =="
if ping -c 3 -W 1 "$NS_IP" >/dev/null 2>&1; then
	echo "PING OK  -> unmatched traffic reached the kernel stack"
else
	echo "PING FAILED"
fi

echo
echo "== consumer output =="
wait "$CONS" || true
