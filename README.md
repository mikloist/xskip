# xskip: zerocopy userspace networking stack

Kernel bypass exploratory work. Packets land in userspace via AF_XDP and stay uncopied until a consumer takes them off the queue.

## What this project is for
I know this is mostly a solved problem, by onload etc, however it never hurts to discover that how could they work in principle.

I used it to learn about:
- eBPF
- TCP/UDP stacks
- AF_XDP and queue management
- kernel bypass
- rust

I only really cared about the hot-path, the rest of the harness and benchmarks are vibed. It's not truly a zerocopy api, we could remove the send/recv copies into/from fix buffers, however I was mostly interested in how much latency the kernel adds.

## Results

Latency numbers are in microseconds:

|stack  |  proto | bytes  |    min   |   p50   |   p99   |   max|
|-------|--------|--------|----------|---------|---------|------|
|kernel |  udp   |    64  |   19.1   |  26.8   |  40.0   | 234.0|
|xskip  |  udp   |    64  |   12.1   |  16.7   |  27.1   | 161.1|
|kernel |  udp   |   512  |   21.3   |  25.6   |  37.0   | 157.5|
|xskip  |  udp   |   512  |   12.5   |  14.9   |  26.1   |  81.1|
|kernel |  udp   |  1472  |   20.5   |  25.9   |  37.4   | 133.1|
|xskip  |  udp   |  1472  |   12.6   |  14.7   |  25.7   |  52.6|
|kernel |  tcp   |    64  |   19.3   |  28.0   |  39.5   |  78.4|
|xskip  |  tcp   |    64  |   14.0   |  18.5   |  29.5   |  85.9|
|kernel |  tcp   |   512  |   19.1   |  28.4   |  42.4   |  94.6|
|xskip  |  tcp   |   512  |   13.5   |  17.9   |  27.2   |  52.8|
|kernel |  tcp   |  1472  |   25.8   |  33.0   |  44.3   |1035.3|
|xskip  |  tcp   |  1472  |   17.8   |  31.0   |  42.3   |  78.3|

all the packets are max MTU sized. Throughput wise kernel clearly wins because it has better load handling. (BBR, GRO etc) but throughput was never the goal.

## Requirements
The stack delivers raw payloads and nothing more: it terminates TCP and UDP in
userspace and hands the bytes to a consumer, which decides what they mean.
Framing, protocol decoding and application logic all live on the far side of
the queue. Plaintext only - TLS is out of scope because decrypting would force
a copy and break the zerocopy path.

The packet data plane comes first and must be proven before anything is built
on top of it. Development ran against a veth pair in a network namespace using
generic (SKB) XDP; zero-copy needs a driver veth does not have, so the harness
now boots a QEMU guest whose virtio-net does. The eBPF program is written in C
and driven from Rust through libbpf-rs.

The socket is shaped like a UNIX socket - create, `connect`, `send`, `recv` -
so a consumer does not have to know anything about XDP, UMEM or rings to use
it. One `connect` serves both transports.

## Pipeline
1. **eBPF/XDP filter** - redirect only traffic for the target address:port into
   an `XSKMAP`; everything else passes to the kernel stack.
2. **AF_XDP** - steer matched packets straight into `UMEM`.
3. **L4** - smoltcp for userspace TCP over the `UMEM` frames; UDP unwrapped
   directly from the frame.
4. **Consumer** - a blocking `recv` hands raw payloads to whoever called it,
   who does what they like with them, including writing bytes straight back.
   Backpressure is the transport's own: stop calling `recv` and TCP closes the
   window on the peer.

## Non-goals (for now)
- IPv6.
- ARP - the peer MAC is configured.
- Any application-level protocol inside the stack.
- Any support
