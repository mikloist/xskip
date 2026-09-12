#!/usr/bin/env python3
"""Render a benchmark result table.

usage: report.py throughput <file>   # one bench JSON line per run
       report.py latency <file>      # "stack proto min p50 p99 max" per run
"""

import json
import sys

STACKS = ("kernel", "speedy")
PROTOS = ("udp", "tcp")


def throughput(path):
    rows = [json.loads(l) for l in open(path) if l.strip()]
    print("%-8s %-5s %12s %10s %8s %10s %8s"
          % ("stack", "proto", "msgs/s", "MiB/s", "loss %", "cpu us/msg", "allocs"))
    for r in rows:
        print("%-8s %-5s %12.1f %10.1f %8.3f %10.3f %8d"
              % (r["stack"], r["proto"], r["msgs_per_s"],
                 r["mbps"] * 1e6 / 8 / (1 << 20), r["loss_pct"],
                 r.get("cpu_us_per_msg", 0.0), r.get("allocs", 0)))
    by = {(r["stack"], r["proto"]): r for r in rows}
    for proto in PROTOS:
        k, s = by.get(("kernel", proto)), by.get(("speedy", proto))
        if k and s:
            print("%s: speedy is %.2fx the rate, %.2fx the cpu per message"
                  % (proto, s["msgs_per_s"] / k["msgs_per_s"],
                     s.get("cpu_us_per_msg", 0) / (k.get("cpu_us_per_msg") or 1)))


def latency(path):
    by = {}
    for line in open(path):
        f = line.split()
        if len(f) == 7:
            by[(f[0], f[1], int(f[2]))] = [float(x) for x in f[3:]]
    sizes = sorted({k[2] for k in by})
    print("%-8s %-5s %6s %8s %8s %8s %8s"
          % ("stack", "proto", "bytes", "min", "p50", "p99", "max"))
    for proto in PROTOS:
        for size in sizes:
            for stack in STACKS:
                v = by.get((stack, proto, size))
                if v:
                    print("%-8s %-5s %6d %8.1f %8.1f %8.1f %8.1f"
                          % (stack, proto, size, *v))
    print()
    for proto in PROTOS:
        for size in sizes:
            k, s = by.get(("kernel", proto, size)), by.get(("speedy", proto, size))
            if k and s:
                print("%s %4dB: speedy p50 %6.1f us vs kernel %6.1f us (%.2fx lower)"
                      % (proto, size, s[1], k[1], k[1] / s[1]))


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("throughput", "latency"):
        sys.exit(__doc__.strip())
    (throughput if sys.argv[1] == "throughput" else latency)(sys.argv[2])


if __name__ == "__main__":
    main()
