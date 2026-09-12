#!/bin/bash
# One entry point: bring the VM up, deploy, benchmark, profile.
#
#   ./scripts/run.sh                 everything
#   ./scripts/run.sh --quick         smaller counts, no flamegraphs
#   ./scripts/run.sh --no-flame      benchmarks only
#   ./scripts/run.sh --down          tear the VM down afterwards
#
# Idempotent: an already-running VM is reused, and the image is fetched only
# the first time. Everything below runs through scripts/suite.sh, which is
# called with the ssh command this script derives, so it needs no configuring.
set -u

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(cd -- "$HERE/.." && pwd)
VM=$HERE/vm/vm.sh
CACHE=${VM_CACHE:-$HOME/.cache/rustssi-vm}
KEY=$CACHE/id_ed25519
SSH_PORT=${VM_SSH_PORT:-2222}

FLAME=yes
DOWN=no
COUNT=${COUNT:-500000}
LAT_COUNT=${LAT_COUNT:-20000}
FLAME_COUNT=${FLAME_COUNT:-4000000}

for a in "$@"; do
    case $a in
    --quick) COUNT=100000; LAT_COUNT=5000; FLAME=no ;;
    --no-flame) FLAME=no ;;
    --down) DOWN=yes ;;
    -h | --help) sed -n '2,9p' "$0"; exit 0 ;;
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
    -o LogLevel=ERROR "$ROOT/target/release/rustssi-bench" \
    fedora@localhost:/home/fedora/ || die "deploy failed"

# A stale generator from an earlier run owns the ports the suites need.
pkill -f bench_peer.py 2>/dev/null
sleep 0.3

rc=0
say "throughput: kernel vs speedy, udp and tcp"
COUNT=$COUNT "$HERE/suite.sh" throughput || rc=1

pkill -f bench_peer.py 2>/dev/null
sleep 0.3

say "latency: round trip over a size sweep, guest echoes"
COUNT=$LAT_COUNT "$HERE/suite.sh" latency || rc=1

if [[ $FLAME == yes ]]; then
    for combo in "speedy tcp" "kernel tcp" "speedy udp" "kernel udp"; do
        pkill -f bench_peer.py 2>/dev/null
        sleep 0.3
        say "profiling $combo"
        COUNT=$FLAME_COUNT "$HERE/suite.sh" profile $combo || rc=1
    done
fi

pkill -f bench_peer.py 2>/dev/null

say "done"
[[ $FLAME == yes ]] && ls -1 "$ROOT"/flame-*.svg 2>/dev/null
[[ $DOWN == yes ]] && echo "vm will be stopped"
echo "ssh: $VM ssh"
exit $rc
