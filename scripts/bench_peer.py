#!/usr/bin/env python3
"""Load generator for the rustssi benchmark. Runs on the host.

The guest is the client: it sends one control message "RUSTSSI <count> <size>\\n"
and we blast <count> messages of <size> bytes at it as fast as the socket takes
them. Serves forever, so repeated runs need no restart.

usage: bench_peer.py <bind-ip> <udp-port> <tcp-port>
"""

import socket
import sys
import threading
import time

SNDBUF = 16 << 20
CHUNK = 64 << 10


def log(msg):
    # One write: print() emits text and newline separately, which lets the two
    # blast threads interleave mid-line.
    sys.stdout.write(msg + "\n")
    sys.stdout.flush()


def parse_control(line):
    f = line.split()
    if len(f) == 4 and f[0] == b"RUSTSSI" and f[1] == b"ECHO":
        return int(f[2]), int(f[3]), True
    if len(f) == 3 and f[0] == b"RUSTSSI":
        return int(f[1]), int(f[2]), False
    raise ValueError(f"bad control message {line!r}")


def report(proto, count, size, sent, elapsed):
    rate = sent / elapsed / 1000 if elapsed > 0 else 0.0
    log(f"{proto} blast {count} x {size} in {elapsed:.3f}s ({rate:.1f} kmsg/s)")


def report_rtt(proto, size, rtts):
    if not rtts:
        log(f"{proto} rtt: nothing came back")
        return
    rtts.sort()
    p = lambda q: rtts[min(len(rtts) - 1, int(len(rtts) * q))]
    log(
        f"{proto} rtt {len(rtts)} x {size}B us: "
        f"min {rtts[0]:.1f} p50 {p(0.50):.1f} p99 {p(0.99):.1f} max {rtts[-1]:.1f}"
    )
    print(
        f'{{"proto":"{proto}","size":{size},"samples":{len(rtts)},'
        f'"rtt_us_min":{rtts[0]:.3f},"rtt_us_p50":{p(0.50):.3f},'
        f'"rtt_us_p99":{p(0.99):.3f},"rtt_us_max":{rtts[-1]:.3f}}}',
        flush=True,
    )


def udp_pingpong(s, peer, count, size):
    """One message out, wait for it back, both timestamps ours."""
    payload = bytes(size)
    s.settimeout(2.0)
    rtts = []
    for _ in range(count):
        t0 = time.perf_counter_ns()
        try:
            s.sendto(payload, peer)
            while True:
                data, src = s.recvfrom(65535)
                if src == peer and len(data) == size:
                    break
        except (OSError, socket.timeout):
            break
        rtts.append((time.perf_counter_ns() - t0) / 1000.0)
    s.settimeout(None)
    report_rtt("udp", size, rtts)


def udp_blast(addr, port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, SNDBUF)
    s.bind((addr, port))
    log(f"udp listening {addr}:{port}")
    while True:
        line, peer = s.recvfrom(65535)
        try:
            count, size, echo = parse_control(line)
        except ValueError as e:
            log(f"udp {e}")
            continue
        log(f"udp run {count} x {size} for {peer}{' (rtt)' if echo else ''}")
        if echo:
            udp_pingpong(s, peer, count, size)
            continue
        payload = bytes(size)
        sent = 0
        t0 = time.monotonic()
        for _ in range(count):
            try:
                s.sendto(payload, peer)
            except (ConnectionResetError, BrokenPipeError):
                # ICMP port-unreachable from a client that already left.
                break
            except OSError as e:
                # ENOBUFS/EAGAIN under load: the kernel dropped it, keep going.
                log(f"udp send: {e}")
                continue
            sent += 1
        report("udp", count, size, sent, time.monotonic() - t0)


def tcp_pingpong(conn, count, size):
    """`size` bytes out, the same `size` bytes back, timed here."""
    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    conn.settimeout(2.0)
    payload = bytes(size)
    rtts = []
    for _ in range(count):
        t0 = time.perf_counter_ns()
        try:
            conn.sendall(payload)
            left = size
            while left:
                data = conn.recv(left)
                if not data:
                    raise ConnectionResetError
                left -= len(data)
        except (OSError, socket.timeout):
            break
        rtts.append((time.perf_counter_ns() - t0) / 1000.0)
    report_rtt("tcp", size, rtts)


def tcp_blast(addr, port):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((addr, port))
    s.listen(8)
    log(f"tcp listening {addr}:{port}")
    buf = bytes(CHUNK)
    while True:
        conn, peer = s.accept()
        with conn:
            conn.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, SNDBUF)
            try:
                line = b""
                while b"\n" not in line:
                    data = conn.recv(256)
                    if not data:
                        break
                    line += data
                count, size, echo = parse_control(line)
            except (ValueError, OSError) as e:
                log(f"tcp {e}")
                continue
            log(f"tcp run {count} x {size} for {peer}{' (rtt)' if echo else ''}")
            if echo:
                tcp_pingpong(conn, count, size)
                continue
            total = count * size
            sent = 0
            t0 = time.monotonic()
            try:
                while sent < total:
                    n = min(CHUNK, total - sent)
                    conn.sendall(buf[:n] if n < CHUNK else buf)
                    sent += n
            except (ConnectionResetError, BrokenPipeError, TimeoutError) as e:
                log(f"tcp send: {e}")
            report("tcp", count, size, sent // size if size else 0,
                   time.monotonic() - t0)


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__.strip().splitlines()[-1])
    addr, udp_port, tcp_port = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    threading.Thread(target=udp_blast, args=(addr, udp_port), daemon=True).start()
    tcp_blast(addr, tcp_port)


if __name__ == "__main__":
    main()
