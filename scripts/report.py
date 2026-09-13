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
    print("%-8s %-5s %12s %10s %8s %10s %8s %8s"
          % ("stack", "proto", "msgs/s", "MiB/s", "loss %", "cpu us/msg",
             "core %", "allocs"))
    for r in rows:
        util = r.get("cpu_s", 0.0) / r["elapsed_s"] * 100 if r.get("elapsed_s") else 0.0
        print("%-8s %-5s %12.1f %10.1f %8.3f %10.3f %8.1f %8d"
              % (r["stack"], r["proto"], r["msgs_per_s"],
                 r["mbps"] * 1e6 / 8 / (1 << 20), r["loss_pct"],
                 r.get("cpu_us_per_msg", 0.0), util, r.get("allocs", 0)))
    by = {(r["stack"], r["proto"]): r for r in rows}
    missing = [f"{s}/{p}" for p in PROTOS for s in STACKS if (s, p) not in by]
    for proto in PROTOS:
        k, s = by.get(("kernel", proto)), by.get(("speedy", proto))
        if k and s and k["msgs_per_s"]:
            print("%s: speedy is %.2fx the rate" % (proto, s["msgs_per_s"] / k["msgs_per_s"]))
    # The generator is a single Python process and the receivers keep up, so
    # the rate is what the sender offered, not what the stack can take. cpu per
    # message means different things either side: the polling stack spins while
    # idle, the kernel one sleeps and is not charged for softirq.
    print("(sender-bound: rate is the generator's, not the stack's)")
    return missing


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
            if k and s and s[1]:
                print("%s %4dB: speedy p50 %6.1f us vs kernel %6.1f us (%.2fx lower)"
                      % (proto, size, s[1], k[1], k[1] / s[1]))
    return [f"{s}/{p} {z}B" for p in PROTOS for z in sizes for s in STACKS
            if (s, p, z) not in by]


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("throughput", "latency"):
        sys.exit(__doc__.strip())
    missing = (throughput if sys.argv[1] == "throughput" else latency)(sys.argv[2])
    # A row that never arrived is a failed run, not a blank cell.
    if missing:
        sys.exit("missing results: " + ", ".join(missing))


if __name__ == "__main__":
    main()
