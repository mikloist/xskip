#!/usr/bin/env bash
# Phase 2 harness. Run with sudo. Brings up a netns + veth pair, runs ngircd on
# the host side as the IRC server, and runs the rustssi client inside the netns
# with its userspace TCP stack over AF_XDP. Expected: the client connects,
# sends NICK/USER, and prints "REGISTERED (received numeric 001)".
set -euo pipefail

NS=rustssi
VETH_HOST=vhost
VETH_NS=vpeer
HOST_IP=10.200.0.1
NS_IP=10.200.0.2
PREFIX=24
PORT=6667
NICK=tester
BIN="${BIN:-target/debug/rustssi}"
CONF="$(mktemp /tmp/ngircd.XXXXXX.conf)"

if [[ $EUID -ne 0 ]]; then
	echo "must run as root (sudo $0)" >&2
	exit 1
fi
if [[ ! -x "$BIN" ]]; then
	echo "binary not found at $BIN (run: cargo build)" >&2
	exit 1
fi
if ! command -v ngircd >/dev/null; then
	echo "ngircd not installed (sudo dnf install ngircd)" >&2
	exit 1
fi

NGIRCD_PID=""
cleanup() {
	[[ -n "$NGIRCD_PID" ]] && kill "$NGIRCD_PID" 2>/dev/null || true
	ip netns del "$NS" 2>/dev/null || true
	ip link del "$VETH_HOST" 2>/dev/null || true
	rm -f "$CONF"
}
trap cleanup EXIT
cleanup 2>/dev/null || true

ip netns add "$NS"
ip link add "$VETH_HOST" type veth peer name "$VETH_NS"
ip link set "$VETH_NS" netns "$NS"
ip addr add "$HOST_IP/$PREFIX" dev "$VETH_HOST"
ip link set "$VETH_HOST" up
ip netns exec "$NS" ip addr add "$NS_IP/$PREFIX" dev "$VETH_NS"
ip netns exec "$NS" ip link set "$VETH_NS" up
ip netns exec "$NS" ip link set lo up

SERVER_MAC="$(cat "/sys/class/net/$VETH_HOST/address")"
echo "server MAC ($VETH_HOST) = $SERVER_MAC"

cat >"$CONF" <<EOF
[Global]
Name = irc.test.local
Info = rustssi test server
Ports = $PORT
Listen = $HOST_IP
[Options]
PAM = no
EOF

echo "== starting ngircd on $HOST_IP:$PORT =="
ngircd -f "$CONF" -n &
NGIRCD_PID=$!
sleep 1

echo
echo "== starting rustssi client in netns ($NS_IP -> $HOST_IP:$PORT) =="
ip netns exec "$NS" timeout 10 "$BIN" "$VETH_NS" "$NS_IP" "$HOST_IP" "$PORT" "$SERVER_MAC" "$NICK" \
	&& echo "== client exited cleanly ==" || echo "== client exited (timeout/err) =="
