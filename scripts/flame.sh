#!/bin/bash
# Flamegraph of one bench run.
#
# perf records inside the guest, where the binary and its symbols are; only the
# perf script text crosses back to the host, which renders it with inferno
# (flamegraph-rs). Nothing needs a Rust toolchain in the guest.
#
# Build the binary it profiles with frame pointers, or -g loses the interior:
#   RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release
set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)

SSH=${SSH:?set SSH to the guest ssh command}
# The binary is expected to be deployed already; see scripts/bench.sh.
STACK=${1:-speedy}
PROTO=${2:-tcp}
PEER_IP=${PEER_IP:-10.99.1.1}
GUEST_IF=${GUEST_IF:-eth1}
GUEST_IP=${GUEST_IP:-10.99.1.2}
HOST_IF=${HOST_IF:-virbr0}
UDP_PORT=${UDP_PORT:-7001}
TCP_PORT=${TCP_PORT:-7002}
# Long enough to collect real samples, not a 150 ms blip.
COUNT=${COUNT:-4000000}
SIZE=${SIZE:-1024}
CPU=${CPU:-2}
FREQ=${FREQ:-997}
OUT=${OUT:-$HERE/../flame-$STACK-$PROTO.svg}

PEER_PY=$HERE/bench_peer.py
PEER_LOG=/tmp/rustssi_flame_peer.$$.log
FOLDED=/tmp/rustssi_flame.$$.folded
PEER_PID=

teardown() {
    [[ -n $PEER_PID ]] && kill "$PEER_PID" 2>/dev/null
    rm -f "$FOLDED"
}
trap teardown EXIT

case $PROTO in
udp) PORT=$UDP_PORT ;;
tcp) PORT=$TCP_PORT ;;
*) echo "proto must be udp or tcp" >&2; exit 1 ;;
esac

PEER_MAC=$(cat "/sys/class/net/$HOST_IF/address")
$SSH sudo ethtool -L "$GUEST_IF" combined 1 >/dev/null 2>&1

taskset -c "${PEER_CPU:-12}" python3 "$PEER_PY" "$PEER_IP" "$UDP_PORT" "$TCP_PORT" > "$PEER_LOG" 2>&1 &
PEER_PID=$!
for _ in $(seq 50); do
    [[ $(grep -o listening "$PEER_LOG" 2>/dev/null | wc -l) -eq 2 ]] && break
    sleep 0.1
done

echo "profiling $STACK/$PROTO, $COUNT x $SIZE bytes at ${FREQ}Hz"
# cpu-clock, not cycles: a guest without a vPMU counts nothing and the record
# comes back with only the exec stacks. -g walks frame pointers, which the
# release profile forces on; dwarf unwinding drops nearly every sample here.
$SSH "sudo perf record -q -e cpu-clock -F $FREQ -g -o /tmp/perf.data -- \
    /home/fedora/rustssi-bench --stack $STACK --proto $PROTO \
    --if $GUEST_IF --local-ip $GUEST_IP --peer-ip $PEER_IP --port $PORT \
    --peer-mac $PEER_MAC --cpu $CPU --queue 0 --count $COUNT --size $SIZE" \
    2>/dev/null | tail -1

# --no-inline: perf otherwise shells out to addr2line per frame, which fails
# against the build-id cache here and emits binary junk instead of stacks.
# The explicit field list is what inferno-collapse-perf expects.
$SSH 'sudo perf script -i /tmp/perf.data --no-inline \
    -F comm,pid,tid,time,event,ip,sym,dso' > "$FOLDED.script" 2>/dev/null
if [[ ! -s $FOLDED.script ]]; then
    echo "perf produced no samples" >&2
    exit 1
fi

if ! inferno-collapse-perf < "$FOLDED.script" > "$FOLDED"; then
    echo "could not fold the profile; first lines were:" >&2
    head -3 "$FOLDED.script" >&2
    exit 1
fi
inferno-flamegraph --title "rustssi $STACK/$PROTO" < "$FOLDED" > "$OUT"
rm -f "$FOLDED.script"

echo "samples: $(wc -l < "$FOLDED") unique stacks"
echo "wrote $OUT"
