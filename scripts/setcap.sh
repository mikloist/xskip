#!/usr/bin/env bash
# Build and grant the binary the file capabilities it needs to run WITHOUT sudo:
#   cap_bpf       - load the XDP program
#   cap_net_admin - attach XDP to the interface
#   cap_net_raw   - open + bind the AF_XDP socket
# File caps are dropped whenever cargo rewrites the binary, so re-run this after
# every build. This does NOT grant namespace privileges (see note below).
set -euo pipefail

BIN="${BIN:-target/debug/rustssi}"
cargo build
sudo setcap cap_bpf,cap_net_admin,cap_net_raw+ep "$BIN"
echo "granted caps to $BIN:"
getcap "$BIN"
echo
echo "now runnable without sudo against a REAL interface, e.g.:"
echo "  $BIN <iface> <our-ip> <server-ip> 6667 <server-mac> nick"
