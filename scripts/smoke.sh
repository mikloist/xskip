#!/bin/bash
# End-to-end smoke test for SpeedySocket. Run as root:
#
#     cargo build && sudo ./scripts/smoke.sh
#
# Topology (the peer MUST be in its own netns, otherwise the kernel
# short-circuits 10.99.0.1 <-> 10.99.0.2 over lo and nothing ever reaches the
# veth, so AF_XDP sees no traffic):
#
#     host netns    sp0 10.99.0.1/24  <- rustssi binds AF_XDP + attaches XDP here
#     netns speedy  sp1 10.99.0.2/24  <- scripts/echo_peer.py, UDP/6667 TCP/6668
#
# Each case gets a freshly created veth. An AF_XDP pool release is deferred by
# the kernel and a netlink-attached XDP program outlives the process that
# attached it, so reusing one link across cases can fail the second bind with
# EBUSY for reasons that have nothing to do with the code under test.
#
# The "hello" cases prove the path end to end; the udp-large case sends 4000
# bytes, which `send` has to split into three 1472-byte datagrams.
set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/.." && pwd)

NS=speedy
HOST_IF=sp0
PEER_IF=sp1
HOST_IP=10.99.0.1
PEER_IP=10.99.0.2
UDP_PORT=6667
TCP_PORT=6668
# Applied to both veth ends by setup; a case may lower it to prove the socket
# reads the MTU off the interface.
LINK_MTU=1500
CPU=${CPU:-5}
BIN=${BIN:-$ROOT/target/debug/rustssi}
ECHO_PY=$HERE/echo_peer.py
ECHO_LOG=/tmp/speedy_echo.$$.log
ERR_LOG=/tmp/speedy_err.$$.log
ECHO_PID=
PEER_MAC=

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (needs netns + veth); try: sudo $0" >&2
    exit 1
fi
if [[ ! -x $BIN ]]; then
    echo "$BIN not found; run 'cargo build' first" >&2
    exit 1
fi
if [[ ! -f $ECHO_PY ]]; then
    echo "$ECHO_PY not found" >&2
    exit 1
fi

teardown() {
    [[ -n $ECHO_PID ]] && kill "$ECHO_PID" 2>/dev/null
    ECHO_PID=
    ip netns del $NS 2>/dev/null
    ip link del $HOST_IF 2>/dev/null
    rm -f "$ECHO_LOG" "$ERR_LOG"
}
trap teardown EXIT

setup() {
    teardown
    ip netns add $NS                                        || return 1
    ip link add $HOST_IF type veth peer name $PEER_IF       || return 1
    ip link set $PEER_IF netns $NS                          || return 1
    ip addr add $HOST_IP/24 dev $HOST_IF                    || return 1
    ip link set $HOST_IF up                                 || return 1
    ip netns exec $NS ip link set lo up                     || return 1
    ip netns exec $NS ip addr add $PEER_IP/24 dev $PEER_IF  || return 1
    ip netns exec $NS ip link set $PEER_IF up               || return 1
    # Both ends, or the smaller side silently drops full-size frames.
    ip link set $HOST_IF mtu $LINK_MTU                      || return 1
    ip netns exec $NS ip link set $PEER_IF mtu $LINK_MTU    || return 1

    PEER_MAC=$(ip netns exec $NS ip -br link show $PEER_IF | awk '{print $3}')

    ip netns exec $NS python3 "$ECHO_PY" $PEER_IP $UDP_PORT $TCP_PORT \
        > "$ECHO_LOG" 2>&1 &
    ECHO_PID=$!
    for _ in $(seq 50); do
        grep -q "tcp listening" "$ECHO_LOG" 2>/dev/null && return 0
        sleep 0.1
    done
    echo "   echo peer did not start:"; sed 's/^/   /' "$ECHO_LOG"
    return 1
}

rc=0
run_case() {
    local label=$1 proto=$2 port=$3 payload=$4 want_msgs=$5 want_sizes=${6:-}
    local out prog_rc msgs sizes
    echo
    echo "== $label: sending ${#payload} bytes to $PEER_IP:$port via AF_XDP"
    if ! setup; then
        echo "   FAIL $label: topology setup failed"
        rc=1
        return
    fi
    echo "   peer mac: $PEER_MAC"

    # stdout is the echoed data, stderr the socket's own chatter.
    out=$(printf '%s\n' "$payload" | timeout 15 "$BIN" "$proto" "$HOST_IF" \
              "$HOST_IP" "$PEER_IP" "$port" "$PEER_MAC" "$CPU" 2>"$ERR_LOG")
    prog_rc=$?
    sed 's/^/   /' "$ERR_LOG"
    # The peer prefixes every message it receives, so the markers count
    # messages and the rest has to come back byte for byte.
    msgs=$(grep -o 'echo:' <<<"$out" | wc -l)
    # What the peer actually received, one size per message.
    sizes=$(awk "/^$proto recv /"' {printf "%s%s", sep, $3; sep=" "}' "$ECHO_LOG")

    if [[ $prog_rc -eq 124 ]]; then
        # timeout killed it: the pipe should terminate on its own.
        echo "   FAIL $label: timed out (did not exit on its own)"
        rc=1
    elif [[ -n $want_sizes ]]; then
        # Return path not checked: the peer's reply is payload+5, so a
        # full-size datagram comes back IP-fragmented and we drop it.
        if [[ $sizes == "$want_sizes" ]]; then
            echo "   PASS $label (peer got [$sizes], exit $prog_rc)"
        else
            echo "   FAIL $label: peer got [$sizes], want [$want_sizes] (exit $prog_rc)"
            rc=1
        fi
    elif [[ $(sed 's/echo://g' <<<"$out") == "$payload" && $msgs -eq $want_msgs ]]; then
        echo "   PASS $label ($msgs msg, exit $prog_rc)"
    else
        echo "   FAIL $label: $msgs msg (want $want_msgs), $(wc -c <<<"$out") bytes back (exit $prog_rc)"
        rc=1
    fi

    echo "   -- peer log --"
    sed 's/^/   /' "$ECHO_LOG"
    teardown
}

run_case udp       udp $UDP_PORT hello 1
run_case tcp       tcp $TCP_PORT hello 1
# 4000 bytes + newline = 1472 + 1472 + 1057, so three datagrams on the wire.
run_case udp-large udp $UDP_PORT "$(printf 'a%.0s' {1..4000})" 0 "1472 1472 1057"

# Same bytes, smaller link: chunking has to follow the interface MTU, not a
# hardcoded 1500. 4001 = 1372 + 1372 + 1257.
LINK_MTU=1400
run_case udp-mtu1400 udp $UDP_PORT "$(printf 'a%.0s' {1..4000})" 0 "1372 1372 1257"

echo
[[ $rc -eq 0 ]] && echo "RESULT: PASS" || echo "RESULT: FAIL"
exit $rc
