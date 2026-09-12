#!/bin/bash
# Kernel socket vs SpeedySocket, both transports, on the guest.
#
#     ./scripts/suite.sh throughput    how fast it drains a blast
#     ./scripts/suite.sh latency       round trip time, guest echoes
#
# The host runs the load generator on the bridge (10.99.1.1); the guest runs
# the bench binary against its virtio-net eth1 (10.99.1.2). Assumes the VM is
# up: scripts/run.sh does that for you.
set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)

MODE=${1:-}
case $MODE in
throughput) DEF_COUNT=100000 DEF_SIZES=1024 ;;
# A sweep, because one size hides the per-byte half of the cost. 1472 is the
# largest UDP payload that still fits one MTU: bigger and the reply comes back
# IP-fragmented, which this stack drops by design.
latency) DEF_COUNT=20000 DEF_SIZES="64 512 1472" ;;
*) sed -n '2,8p' "$0" >&2; exit 1 ;;
esac

SSH=${SSH:-"ssh -p 2222 -o StrictHostKeyChecking=no fedora@127.0.0.1"}
PEER_IP=${PEER_IP:-10.99.1.1}
HOST_IF=${HOST_IF:-virbr0}
GUEST_IF=${GUEST_IF:-eth1}
GUEST_IP=${GUEST_IP:-10.99.1.2}
UDP_PORT=${UDP_PORT:-7001}
TCP_PORT=${TCP_PORT:-7002}
COUNT=${COUNT:-$DEF_COUNT}
SIZES=${SIZES:-${SIZE:-$DEF_SIZES}}
CPU=${CPU:-2}
PEER_CPU=${PEER_CPU:-12}
QUEUE=${QUEUE:-0}
BENCH=${BENCH:-/home/fedora/rustssi-bench}

PEER_PY=$HERE/bench_peer.py
PEER_LOG=/tmp/rustssi_${MODE}_peer.$$.log
ERR_LOG=/tmp/rustssi_${MODE}_err.$$
RESULTS=/tmp/rustssi_${MODE}_results.$$
PEER_PID=
MAX_CHANNELS=

teardown() {
    [[ -n $PEER_PID ]] && kill "$PEER_PID" 2>/dev/null
    # Put the NIC back the way we found it.
    [[ -n $MAX_CHANNELS ]] &&
        $SSH sudo ethtool -L "$GUEST_IF" combined "$MAX_CHANNELS" >/dev/null 2>&1
    rm -f "$ERR_LOG" "$RESULTS"
}
trap teardown EXIT

# Options cannot be appended: anything after the host in $SSH is the command.
if ! timeout 10 $SSH true 2>"$ERR_LOG"; then
    echo "cannot ssh to the guest with: $SSH" >&2
    sed 's/^/   /' "$ERR_LOG" >&2
    echo "start the VM first (scripts/run.sh), or set SSH=..." >&2
    exit 1
fi

# The dst MAC of every frame the guest emits: the HOST side of the link.
PEER_MAC=$(cat "/sys/class/net/$HOST_IF/address") || exit 1

# One AF_XDP socket serves exactly one ring: it is registered as
# xsks_map[QUEUE], and bpf_redirect_map on any other ring finds no socket and
# falls through to the kernel, silently (received=0, no error anywhere).
#
# The RSS indirection table is NOT enough here. Measured on virtio-net: with
# the table collapsed to ring 0, inbound TCP still landed on ring 1 about half
# the time, because the steering decision belongs to the host tap, not to the
# guest's table. Cutting the channel count is what actually binds every flow.
[[ $QUEUE -eq 0 ]] || { echo "one ring is ring 0; use QUEUE=0" >&2; exit 1; }
MAX_CHANNELS=$($SSH ethtool -l "$GUEST_IF" 2>/dev/null | awk '/^Combined:/{n=$2} END{print n}')
$SSH sudo ethtool -L "$GUEST_IF" combined 1 >/dev/null 2>&1
now=$($SSH ethtool -l "$GUEST_IF" 2>/dev/null | awk '/^Combined:/{n=$2} END{print n}')
[[ $now == 1 ]] || { echo "cannot reduce $GUEST_IF to one ring (got ${now:-none})" >&2; exit 1; }

# Every latency sample includes this process's own syscalls, so keep the
# generator off the cores running the VM.
taskset -c "$PEER_CPU" python3 "$PEER_PY" "$PEER_IP" "$UDP_PORT" "$TCP_PORT" > "$PEER_LOG" 2>&1 &
PEER_PID=$!
# Both threads print concurrently and can share a line: count matches, not lines.
for _ in $(seq 50); do
    [[ $(grep -o listening "$PEER_LOG" 2>/dev/null | wc -l) -eq 2 ]] && break
    sleep 0.1
done
[[ $(grep -o listening "$PEER_LOG" 2>/dev/null | wc -l) -eq 2 ]] || {
    echo "load generator did not start:" >&2
    sed 's/^/   /' "$PEER_LOG" >&2
    exit 1
}

echo "guest $GUEST_IP ($GUEST_IF) -> peer $PEER_IP ($HOST_IF, mac $PEER_MAC)"
echo "$MODE: $COUNT messages of [$SIZES] bytes, guest on isolated cpu $CPU, generator on host cpu $PEER_CPU"

rc=0
: > "$RESULTS"

run_case() {
    local stack=$1 proto=$2 port=$3 size=$4 out line mark
    echo
    echo "== $stack/$proto ${size}B"
    # Only lines the peer writes from here on belong to this run; matching the
    # whole log would silently reuse the previous run's result when this one
    # produces nothing.
    mark=$(wc -l < "$PEER_LOG")
    # latency: the guest echoes and the peer times; throughput: the guest
    # times its own consume loop and prints JSON.
    local mode_args=()
    [[ $MODE == latency ]] && mode_args=(--mode echo)
    out=$($SSH sudo "$BENCH" --stack "$stack" --proto "$proto" "${mode_args[@]}" \
        --if "$GUEST_IF" --local-ip "$GUEST_IP" --peer-ip "$PEER_IP" \
        --port "$port" --peer-mac "$PEER_MAC" --cpu "$CPU" --queue "$QUEUE" \
        --count "$COUNT" --size "$size" 2>"$ERR_LOG")
    sed 's/^/   /' "$ERR_LOG"

    if [[ $MODE == throughput ]]; then
        if [[ $out != \{* ]]; then
            echo "   FAIL $stack/$proto: no JSON line on stdout"
            rc=1
            return
        fi
        echo "   $out"
        printf '%s\n' "$out" >> "$RESULTS"
        return
    fi

    # The guest exits as soon as it echoes the last message; the peer still has
    # to receive it and print its summary, so the line lands after ssh returns.
    for _ in $(seq 50); do
        line=$(tail -n "+$((mark + 1))" "$PEER_LOG" | grep "$proto rtt" | tail -1)
        [[ -n $line ]] && break
        sleep 0.1
    done
    if [[ -z $line ]]; then
        echo "   FAIL $stack/$proto: no round trips completed"
        rc=1
        return
    fi
    echo "   $line"
    # "... us: min A p50 B p99 C max D"
    echo "$stack $proto $size $(sed -n 's/.*min \([0-9.]*\) p50 \([0-9.]*\) p99 \([0-9.]*\) max \([0-9.]*\).*/\1 \2 \3 \4/p' <<<"$line")" \
        >> "$RESULTS"
}

for size in $SIZES; do
    for stack in kernel speedy; do
        run_case "$stack" udp "$UDP_PORT" "$size"
        run_case "$stack" tcp "$TCP_PORT" "$size"
    done
done

if [[ $MODE == throughput ]]; then
    echo
    echo "-- load generator --"
    grep blast "$PEER_LOG"
fi

echo
python3 "$HERE/report.py" "$MODE" "$RESULTS"

echo
[[ $rc -eq 0 ]] && echo "RESULT: PASS" || echo "RESULT: FAIL"
exit $rc
