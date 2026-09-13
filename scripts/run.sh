#!/bin/bash
# One entry point: bring the VM up, deploy, benchmark, profile.
#
#   ./scripts/run.sh                 everything
#   ./scripts/run.sh --quick         smaller counts, no flamegraphs
#   ./scripts/run.sh --no-flame      benchmarks only
#   ./scripts/run.sh --no-alloc      skip the dhat allocation pass
#   ./scripts/run.sh --down          tear the VM down afterwards
#
# Idempotent: an already-running VM is reused, and the image is fetched only
# the first time. Everything below runs through scripts/suite.sh, which is
# called with the ssh command this script derives, so it needs no configuring.
# pipefail: `cargo build | tail` otherwise reports tail's status, so a failed
# build would deploy and benchmark whatever binary was lying around.
set -u -o pipefail

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/.." && pwd)
VM=$HERE/vm/vm.sh
CACHE=${VM_CACHE:-$HOME/.cache/xskip-vm}
KEY=$CACHE/id_ed25519
SSH_PORT=${VM_SSH_PORT:-2222}

FLAME=yes
ALLOC=yes
DOWN=no
COUNT=${COUNT:-500000}
LAT_COUNT=${LAT_COUNT:-20000}
FLAME_COUNT=${FLAME_COUNT:-4000000}
# Small: dhat records a backtrace per allocation, and the answer wanted here is
# how many, not how fast.
ALLOC_COUNT=${ALLOC_COUNT:-20000}

for a in "$@"; do
    case $a in
    --quick) COUNT=100000; LAT_COUNT=5000; FLAME=no; ALLOC=no ;;
    --no-flame) FLAME=no ;;
    --no-alloc) ALLOC=no ;;
    --down) DOWN=yes ;;
    -h | --help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option $a" >&2; exit 1 ;;
    esac
done

say() { printf '\n=== %s\n' "$*"; }
die() { echo "run: $*" >&2; exit 1; }

trap '[[ $DOWN == yes ]] && "$VM" down >/dev/null 2>&1' EXIT

# 1. Binary first: no point booting a VM for code that does not compile.
#    Frame pointers so the flamegraph has an interior; -g needs them.
say "building"
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --manifest-path "$ROOT/Cargo.toml" \
    2>&1 | tail -2 || die "build failed"

# 2. VM: fetch the image once, start only if nothing is listening already.
SSH="ssh -i $KEY -p $SSH_PORT -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
-o LogLevel=ERROR -o ConnectTimeout=4 fedora@localhost"
if timeout 10 $SSH true 2>/dev/null; then
    say "vm already up"
else
    # Downloads only if the cache is cold, so it is safe to always call.
    say "preparing image"
    "$VM" image || die "image failed"
    say "starting vm"
    "$VM" up || die "vm up failed"
    timeout 10 $SSH true 2>/dev/null || die "vm is up but ssh does not answer"
fi

export SSH

say "deploying"
scp -i "$KEY" -P "$SSH_PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    -o LogLevel=ERROR "$ROOT/target/release/xskip-bench" \
    fedora@localhost:/home/fedora/ || die "deploy failed"

# A stale generator from an earlier run owns the ports the suites need.
pkill -f bench_peer.py 2>/dev/null
sleep 0.3

rc=0
say "throughput: kernel vs xskip, udp and tcp"
COUNT=$COUNT "$HERE/suite.sh" throughput || rc=1

pkill -f bench_peer.py 2>/dev/null
sleep 0.3

say "latency: round trip over a size sweep, guest echoes"
COUNT=$LAT_COUNT "$HERE/suite.sh" latency || rc=1

if [[ $FLAME == yes ]]; then
    for combo in "xskip tcp" "kernel tcp" "xskip udp" "kernel udp"; do
        pkill -f bench_peer.py 2>/dev/null
        sleep 0.3
        say "profiling $combo"
        COUNT=$FLAME_COUNT "$HERE/suite.sh" profile $combo || rc=1
    done
fi

# Last, and with its own binary: dhat replaces the global allocator, so a
# build carrying it would distort every number above.
if [[ $ALLOC == yes ]]; then
    pkill -f bench_peer.py 2>/dev/null
    sleep 0.3
    say "allocations: dhat over the consume loop"
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release --features dhat \
        --manifest-path "$ROOT/Cargo.toml" 2>&1 | tail -2 || die "dhat build failed"
    scp -i "$KEY" -P "$SSH_PORT" -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
        "$ROOT/target/release/xskip-bench" \
        fedora@localhost:/home/fedora/xskip-bench-dhat || die "dhat deploy failed"
    DHAT=yes COUNT=$ALLOC_COUNT BENCH=/home/fedora/xskip-bench-dhat \
        "$HERE/suite.sh" throughput || rc=1
    # The profiles are written in the guest; bring them here to look at.
    $SSH sudo chown fedora: '~/dhat-*.json' 2>/dev/null
    scp -i "$KEY" -P "$SSH_PORT" -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
        'fedora@localhost:/home/fedora/dhat-*.json' "$ROOT/" 2>/dev/null &&
        $SSH 'rm -f ~/dhat-*.json'
    # Leave the plain binary in place for the next run.
    RUSTFLAGS="-C force-frame-pointers=yes" cargo build --release \
        --manifest-path "$ROOT/Cargo.toml" 2>&1 | tail -1
fi

pkill -f bench_peer.py 2>/dev/null

say "done"
[[ $FLAME == yes ]] && ls -1 "$ROOT"/flame-*.svg 2>/dev/null
if [[ $ALLOC == yes ]] && ls "$ROOT"/dhat-*.json >/dev/null 2>&1; then
    ls -1 "$ROOT"/dhat-*.json
    echo "view at https://nnethercote.github.io/dh_view/dh_view.html (load the json)"
fi
[[ $DOWN == yes ]] && echo "vm will be stopped"
echo "ssh: $VM ssh"
exit $rc
