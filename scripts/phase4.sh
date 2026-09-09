#!/usr/bin/env bash
# Phase 4 harness. Run with sudo from an interactive terminal. Brings up the
# netns + veth + ngircd, then runs the rustssi TUI client in the FOREGROUND so
# you can type. Quit the client with Esc or Ctrl-C; cleanup runs on exit.
#
# In the TUI: plain text -> PRIVMSG to the channel; "/..." -> raw IRC command
# (e.g. "/join #other", "/whois tester").
set -euo pipefail

NS=rustssi
VETH_HOST=vhost
VETH_NS=vpeer
HOST_IP=10.200.0.1
NS_IP=10.200.0.2
PREFIX=24
PORT=6667
NICK="${NICK:-tester}"
CHANNEL="${CHANNEL:-#test}"
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

cat >"$CONF" <<EOF
[Global]
Name = irc.test.local
Info = rustssi test server
Ports = $PORT
Listen = $HOST_IP
[Options]
PAM = no
EOF

ngircd -f "$CONF" &
NGIRCD_PID=$!
sleep 1

echo "server $HOST_IP:$PORT ready (MAC $SERVER_MAC); launching TUI (Esc/Ctrl-C to quit)..."
sleep 1

# Foreground, interactive: inherits this terminal's tty for ratatui.
ip netns exec "$NS" "$BIN" "$VETH_NS" "$NS_IP" "$HOST_IP" "$PORT" "$SERVER_MAC" "$NICK" "$CHANNEL" || true
echo "client exited."
