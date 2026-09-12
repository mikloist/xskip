#!/usr/bin/env python3
"""Echo peer for the SpeedySocket smoke test.

Listens on UDP and TCP at the given address and replies with b"echo:" + data.
Runs inside the peer netns; see scripts/smoke.sh.

usage: echo_peer.py <bind-ip> <udp-port> <tcp-port>
"""

import socket
import sys
import threading


def udp_echo(addr, port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind((addr, port))
    print(f"udp listening {addr}:{port}", flush=True)
    while True:
        data, peer = s.recvfrom(65535)
        print(f"udp recv {len(data)} from {peer}", flush=True)
        s.sendto(b"echo:" + data, peer)


def tcp_conn(conn, peer):
    print(f"tcp conn from {peer}", flush=True)
    with conn:
        while True:
            data = conn.recv(65535)
            if not data:
                break
            print(f"tcp recv {len(data)}", flush=True)
            conn.sendall(b"echo:" + data)
    print(f"tcp closed {peer}", flush=True)


def tcp_echo(addr, port):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((addr, port))
    s.listen(8)
    print(f"tcp listening {addr}:{port}", flush=True)
    while True:
        conn, peer = s.accept()
        threading.Thread(target=tcp_conn, args=(conn, peer), daemon=True).start()


def main():
    if len(sys.argv) != 4:
        sys.exit(__doc__.strip().splitlines()[-1])
    addr, udp_port, tcp_port = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    threading.Thread(target=udp_echo, args=(addr, udp_port), daemon=True).start()
    tcp_echo(addr, tcp_port)


if __name__ == "__main__":
    main()
