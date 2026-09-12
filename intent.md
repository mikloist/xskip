# Rustssi: zerocopy userspace networking stack

Stupidly fast, overkill-by-design transport. Packets land in userspace via
AF_XDP and stay uncopied until a consumer takes them off the queue.

## Requirements
The stack delivers raw payloads and nothing more: it terminates TCP and UDP in
userspace and hands the bytes to a consumer, which decides what they mean.
Framing, protocol decoding and application logic all live on the far side of
the queue. Plaintext only — TLS is out of scope because decrypting would force
a copy and break the zerocopy path.

The packet data plane comes first and must be proven before anything is built
on top of it. Development runs against a veth pair in a network namespace using
generic (SKB) XDP, so it needs no special NIC and stays reproducible on any box
and in CI. The eBPF program is written in C and driven from Rust through
libbpf-rs.

The socket is shaped like a UNIX socket — create, `connect`, `send`, `recv` —
so a consumer does not have to know anything about XDP, UMEM or rings to use
it. One `connect` serves both transports.

## Pipeline
1. **eBPF/XDP filter** — redirect only traffic for the target address:port into
   an `XSKMAP`; everything else passes to the kernel stack.
2. **AF_XDP** — steer matched packets straight into `UMEM`.
3. **L4** — smoltcp for userspace TCP over the `UMEM` frames; UDP unwrapped
   directly from the frame.
4. **Consumer** — a blocking `recv` hands raw payloads to whoever called it,
   who does what they like with them, including writing bytes straight back.
   Backpressure is the transport's own: stop calling `recv` and TCP closes the
   window on the peer.

## Build order
1. netns + veth + eBPF redirect into `XSKMAP`; verify with a raw AF_XDP echo.
2. AF_XDP socket owning its UMEM and rings, with both transports over it.
3. Blocking `send`/`recv` on a single thread, proven end to end by an echo peer.
4. Zero-copy on a real NIC, then lend the consumer the UMEM frame instead of
   copying the payload into its buffer.

Steps 1–3 are done and covered by `scripts/smoke.sh`.

## Non-goals (for now)
- TLS / SASL.
- IPv6.
- ARP — the peer MAC is configured.
- Any application-level protocol inside the stack.
