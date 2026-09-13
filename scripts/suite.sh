#!/bin/bash
# Kernel socket vs SpeedySocket, both transports, on the guest.
#
#     ./scripts/suite.sh throughput            how fast it drains a blast
#     ./scripts/suite.sh latency               round trip time, guest echoes
#     ./scripts/suite.sh profile <stack> <proto>   flamegraph of one run
#
# The host runs the load generator on the bridge (10.99.1.1); the guest runs
# the bench binary against its virtio-net eth1 (10.99.1.2). Assumes the VM is
# up: scripts/run.sh does that for you.
set -u -o pipefail

HERE=$(cd -- "$(dirname -- "$0")" && pwd)

MODE=${1:-}
case $MODE in
throughput) DEF_COUNT=100000 DEF_SIZES=1024 ;;
# A sweep, because one size hides the per-byte half of the cost. 1472 is the
# largest UDP payload that still fits one MTU: bigger and the reply comes back
# IP-fragmented, which this stack drops by design.
latency) DEF_COUNT=20000 DEF_SIZES="64 512 1472" ;;
# Long enough to collect real samples, not a 150 ms blip.
profile) DEF_COUNT=4000000 DEF_SIZES=1024 P_STACK=${2:-xskip} P_PROTO=${3:-tcp} ;;
*) sed -n '2,9p' "$0" >&2; exit 1 ;;
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
# Bigger than one datagram, so `send` has to split it.
CHUNK_SIZE=${CHUNK_SIZE:-4000}
CPU=${CPU:-2}
PEER_CPU=${PEER_CPU:-12}
QUEUE=${QUEUE:-0}
BENCH=${BENCH:-/home/fedora/xskip-bench}

PEER_PY=$HERE/bench_peer.py
PEER_LOG=/tmp/xskip_${MODE}_peer.$$.log
ERR_LOG=/tmp/xskip_${MODE}_err.$$
RESULTS=/tmp/xskip_${MODE}_results.$$
PEER_PID=
MAX_CHANNELS=

teardown() {
    [[ -n $PEER_PID ]] && kill "$PEER_PID" 2>/dev/null
    # Put the NIC back the way we found it.
    [[ -n $MAX_CHANNELS ]] &&
        $SSH sudo ethtool -L "$GUEST_IF" combined "$MAX_CHANNELS" >/dev/null 2>&1
    rm -f "$ERR_LOG" "$RESULTS" "$RESULTS.script" "$PEER_LOG"
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
# Without it teardown cannot restore the NIC, and every later run inherits one
# ring without being told.
[[ -n $MAX_CHANNELS ]] || { echo "cannot read channel count for $GUEST_IF" >&2; exit 1; }
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
    # Demand zero-copy rather than accepting Auto's fallback: a guest that
    # regressed on VIRTIO_F_ACCESS_PLATFORM would otherwise post a full set of
    # "xskip" rows quietly measured in copy mode. Override with XDP_MODE=auto.
    [[ $stack == xskip ]] && mode_args+=(--xdp-mode "${XDP_MODE:-zerocopy}")
    # A wedged guest should cost a minute, not the whole session.
    out=$(timeout "${RUN_TIMEOUT:-120}" $SSH sudo "$BENCH" \
        --stack "$stack" --proto "$proto" "${mode_args[@]}" \
        --if "$GUEST_IF" --local-ip "$GUEST_IP" --peer-ip "$PEER_IP" \
        --port "$port" --peer-mac "$PEER_MAC" --cpu "$CPU" --queue "$QUEUE" \
        --count "$COUNT" --size "$size" 2>"$ERR_LOG")
    sed 's/^/   /' "$ERR_LOG"
    # dhat writes dhat-heap.json into the guest's home on every run, so name it
    # before the next case overwrites it. scripts/run.sh copies them back.
    [[ ${DHAT:-no} == yes ]] &&
        $SSH "sudo mv -f ~/dhat-heap.json ~/dhat-$stack-$proto.json" 2>/dev/null

    if [[ $MODE == throughput ]]; then
        if [[ $out != \{* ]]; then
            echo "   FAIL $stack/$proto: no JSON line on stdout"
            rc=1
            return
        fi
        echo "   $out"
        # A run that received nothing is well-formed JSON, and the usual cause
        # is a misconfigured queue or bind mode, not a slow peer. Treating it
        # as a result is how a broken harness reports PASS.
        if [[ $out == *'"received":0,'* ]]; then
            echo "   FAIL $stack/$proto: received nothing"
            rc=1
            return
        fi
        printf '%s\n' "$out" >> "$RESULTS"
        return
    fi

    # The guest exits as soon as it echoes the last message; the peer still has
    # to receive it and print its summary, so the line lands after ssh returns.
    # Match the success form only: the peer also logs "<proto> rtt: nothing
    # came back", which would otherwise pass as a completed run.
    for _ in $(seq 50); do
        line=$(tail -n "+$((mark + 1))" "$PEER_LOG" | grep "$proto rtt .*p50" | tail -1)
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

if [[ $MODE == profile ]]; then
    port=$UDP_PORT
    [[ $P_PROTO == tcp ]] && port=$TCP_PORT
    out=${OUT:-$HERE/../flame-$P_STACK-$P_PROTO.svg}
    script=$RESULTS.script
    # One profile, one size: a sweep would mix unrelated stacks in one graph.
    psize=${SIZES%% *}
    echo "profiling $P_STACK/$P_PROTO, $COUNT x $psize bytes at ${FREQ:-997}Hz"
    # cpu-clock, not cycles: a guest without a vPMU counts nothing and the
    # record comes back with only the exec stacks. -g walks frame pointers,
    # which scripts/run.sh forces on; dwarf unwinding drops nearly every
    # sample here.
    $SSH "sudo perf record -q -e cpu-clock -F ${FREQ:-997} -g -o /tmp/perf.data -- \
        $BENCH --stack $P_STACK --proto $P_PROTO --if $GUEST_IF \
        --local-ip $GUEST_IP --peer-ip $PEER_IP --port $port \
        --peer-mac $PEER_MAC --cpu $CPU --queue $QUEUE --count $COUNT --size $psize" \
        2>/dev/null | tail -1
    # --no-inline: perf otherwise shells out to addr2line per frame, which
    # fails against the build-id cache here and emits binary junk.
    $SSH 'sudo perf script -i /tmp/perf.data --no-inline \
        -F comm,pid,tid,time,event,ip,sym,dso' > "$script" 2>/dev/null
    [[ -s $script ]] || { echo "perf produced no samples" >&2; exit 1; }
    inferno-collapse-perf < "$script" > "$RESULTS" || {
        echo "could not fold the profile; first lines were:" >&2
        head -3 "$script" >&2
        exit 1
    }
    inferno-flamegraph --title "xskip \$P_STACK/$P_PROTO" < "$RESULTS" > "$out"
    rm -f "$script"
    echo "$(wc -l < "$RESULTS") unique stacks -> $out"
    exit 0
fi

for size in $SIZES; do
    for stack in kernel xskip; do
        run_case "$stack" udp "$UDP_PORT" "$size"
        run_case "$stack" tcp "$TCP_PORT" "$size"
    done
done

if [[ $MODE == throughput ]]; then
    # The only check that UDP splits by the interface MTU rather than by a
    # constant, and the only one that looks at the bytes rather than counting
    # them. 4000 over a 1500 MTU is 1472 + 1472 + 1056.
    echo
    echo "== xskip/udp chunking, one $CHUNK_SIZE byte send"
    mark=$(wc -l < "$PEER_LOG")
    mtu=$($SSH cat "/sys/class/net/$GUEST_IF/mtu" | tr -d '\r')
    payload=$((mtu - 28))
    want=""
    left=$CHUNK_SIZE
    while [[ $left -gt 0 ]]; do
        [[ $left -gt $payload ]] && n=$payload || n=$left
        want="$want $n"
        left=$((left - n))
    done
    timeout "${RUN_TIMEOUT:-120}" $SSH sudo "$BENCH" --stack xskip --proto udp \
        --mode chunk --xdp-mode "${XDP_MODE:-zerocopy}" \
        --if "$GUEST_IF" --local-ip "$GUEST_IP" --peer-ip "$PEER_IP" \
        --port "$UDP_PORT" --peer-mac "$PEER_MAC" --cpu "$CPU" --queue "$QUEUE" \
        --count 1 --size "$CHUNK_SIZE" >/dev/null 2>"$ERR_LOG"
    sed 's/^/   /' "$ERR_LOG"
    for _ in $(seq 30); do
        got=$(tail -n "+$((mark + 1))" "$PEER_LOG" | grep "udp chunks" | tail -1)
        [[ -n $got ]] && break
        sleep 0.1
    done
    if [[ $got == "udp chunks${want} intact=True" ]]; then
        echo "   PASS $got"
    else
        echo "   FAIL got [$got], want [udp chunks${want} intact=True]"
        rc=1
    fi

    echo
    echo "-- load generator --"
    grep blast "$PEER_LOG"
fi

echo
# The reporter is also a check: it exits non-zero on a row it cannot pair or
# parse, which is the shape a half-failed suite leaves behind.
python3 "$HERE/report.py" "$MODE" "$RESULTS" || rc=1

echo
[[ $rc -eq 0 ]] && echo "RESULT: PASS" || echo "RESULT: FAIL"
exit $rc
