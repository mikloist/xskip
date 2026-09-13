# Rustssi Spec

A fast userspace networking stack. Packets bypass the kernel stack via XDP,
land in an AF_XDP UMEM, and are reassembled (TCP) or unwrapped (UDP) in
userspace. A consumer polls the raw payloads out with `recv` and
does whatever it likes with them — echoing them back, decoding a protocol.

The stack has no opinion about what the bytes mean. Framing, parsing and
application logic all live in the consumer.

## Architecture

Single-threaded by construction. `SpeedySocket` owns the AF_XDP socket, its
UMEM and the smoltcp state, and `send`/`recv` drive the rings directly on
whatever thread calls them — pin that thread with `pin_cpu` for a hot core.
There is no background thread, no queue, no lock and no async runtime; `send`
and `recv` are one pass over the rings and never sleep, so the consumer's own
loop is the only loop.

```mermaid
flowchart LR
  NIC[veth / NIC] -->|XDP redirect| XSK[AF_XDP + UMEM]
  XSK --> L4[smoltcp TCP / raw UDP]
  L4 -->|recv| APP[consumer]
  APP -->|send| L4
```

This was not the first design. An earlier cut gave the socket its own pinned
thread and bounded MPSC queues so the handle could be `Send`; that bought a
heap allocation per message at the queue boundary, a deadlock between `drop`
and backpressure, and a pile of machinery nobody had asked for. Collapsing it
to one thread deleted all of it. The copy budget is now: TCP payloads go
straight from smoltcp's receive buffer into the caller's slice via
`recv_slice`, and UDP payloads are copied once out of the UMEM frame.

## Data plane

The eBPF program is written in C and loaded from Rust through libbpf-rs. It
attaches in native (driver) XDP mode, falling back to generic only on a driver
without it — a generic attachment cannot feed a zero-copy socket.

A BPF hash map holds the flows we own, keyed on the full 4-tuple: remote
address, remote port, local address, local port, packed and in network order.
For every packet the program builds that key from the headers and looks it up.
A hit is redirected into the `XSKMAP` and delivered to our AF_XDP socket;
everything else returns `XDP_PASS` and takes the normal kernel path.

Only the inbound direction is keyed — remote is the packet's *source*, local
its *destination*. That is the only direction an ingress hook ever sees, since
our own transmits go out the TX ring and never arrive anywhere. Two things
follow, and both are the reason the key is not just the remote endpoint:

- The host kernel keeps its own connections to the same server. A
  remote-only key captured those too, starving them, because the local port
  was not part of the match.
- A hairpinned copy of our own output cannot match, so it cannot be redirected
  back at us. With a remote-only key it matched on destination and was — which
  is what makes a loopback-style topology unusable.

`flow_key` in `src/speedy.rs` is the single place the key is laid out, used by
both the insert and the matching delete, and covered by a test: a wrong byte
order fails silently, with the lookup simply never matching.

The AF_XDP socket binds with `XDP_USE_NEED_WAKEUP`, and the consumer's loop
spins the RX ring directly, kicking the driver with `recvfrom`/`sendto` only
when a ring raises the need-wakeup flag. (`SO_PREFER_BUSY_POLL` is not set; the
loop already never sleeps, so there is nothing yet for it to buy.) The bind
mode is the only thing that differs between environments: `XdpMode::Copy` works
anywhere and `XdpMode::ZeroCopy` needs a driver with `ndo_xsk_wakeup`, which
veth does not have and virtio-net does. `Auto` asks for zero-copy and falls
back. The userspace code — UMEM, rings, XSKMAP, smoltcp — is identical either
way; the benchmark guest binds zero-copy and the suite demands it explicitly,
so a silent downgrade fails the run instead of quietly halving the result.

UMEM layout, one dedicated region per socket:

- Frame size: the interface MTU plus a 14-byte Ethernet header, rounded up to a
  power of two, floored at the kernel's 2 KiB minimum and capped at one page —
  2048 bytes on a standard 1500-byte link.
- Frame count: 4096, so the region is 8 MiB at that frame size and scales with
  the link. 2 MiB hugepages when the pool allows.
- Fill ring / RX ring: 2048 entries each.
- Completion ring / TX ring: 2048 entries each.

Frames `[0, 2048)` back the RX/FILL path and `[2048, 4096)` are the TX free
pool. Keeping them disjoint means RX and TX never contend for a frame.

The caller pins its own thread with `pin_cpu`, which wraps `sched_setaffinity`.
Deployment may additionally isolate that core with `isolcpus` / `nohz_full`,
but that is a runtime concern, not code.

## Socket API

`SpeedySocket` (`src/speedy.rs`) is shaped like a UNIX socket:

```rust
// The program and its maps belong to the interface, so the caller owns them.
let mut obj = MaybeUninit::uninit();
let skel = speedy::load_skel(&mut obj)?;
let _xdp = speedy::attach_xdp(&skel, speedy::ifindex("sp0")?)?;

// UMEM, rings, bind, xsks_map registration
let mut sock = SpeedySocket::new(&skel, Protocol::Tcp, config)?;

// config_map insert, plus the SYN; the handshake is the caller's loop
sock.connect(SocketAddrV4::new(peer_ip, port))?;
while !sock.poll_connect()? {}

let Sent(n) = sock.send(b"...")?;
let n = sock.recv(&mut buf); // Option: None until something arrives
```

`new` builds the UMEM and rings, binds, and registers the socket fd in
`xsks_map[queue]`. It *borrows* the skeleton rather than owning it, because
XDP attachment is interface-wide: one loaded program can serve several sockets
on different queues, and welding attach/detach to a single socket's lifetime
prevents that. `XdpAttachment` is a guard that detaches on drop, so no early
return can leak an attached program — a leaked one keeps stealing packets from
the kernel stack and holds the queue's AF_XDP pool, which makes the next bind
fail `EBUSY`.

The skeleton borrows caller-owned storage (`obj`), which is libbpf-rs's
intended shape: `open` writes the `OpenObject` into uninitialized storage, so
it cannot be handed an already-built one, and hiding it inside the socket
would need a self-referential struct or a leak.

`Drop` on the socket removes exactly what it registered — its `xsks_map` slot
and its `config_map` endpoint — and leaves the program attached.

`connect` is the single entry point for both protocols and is what inserts the
endpoint into `config_map`, which is the moment XDP starts redirecting. TCP
additionally completes the handshake before returning.

`send` returns how many bytes went out, as a `#[must_use] Sent(usize)` so the
count cannot be dropped by accident. UDP always takes the whole buffer; TCP
takes what smoltcp's send buffer will accept right now and returns short —
`Ok(0)` when the window is shut or the send half is closed — so a peer that
stops reading never traps the caller in a spin. `recv` is the same shape: one
pass over the rings, `None` when nothing has arrived, `Some(0)` at end of
stream. Neither call ever blocks, so the consumer owns the loop and decides
when to give up — `src/bin/bench.rs` muxes TX and RX in a single pass this way.
`connect` does not block either: it inserts the flow and sends the SYN, then
`poll_connect` drives the handshake one pass at a time, `Ok(true)` once the
peer answers and `ConnectionRefused` on an RST. Nothing in the socket knows
about deadlines or signals; `CONNECT_TIMEOUT` is a constant the caller applies.

Backpressure needs no machinery: TCP is bounded by smoltcp's own send and
receive buffers, which close the window on the peer when the consumer stops
calling `recv`. UDP simply drops a datagram when the TX pool is empty, which
is within its contract.

End of stream is TCP-only: `recv` reports it when the connection goes
inactive. UDP has no FIN, so a UDP `recv` just keeps reporting `None` — a
property consumers must handle, and the bench does it with an idle window.

## Transports

**TCP** runs smoltcp over the UMEM frames at `Medium::Ip`: we implement
smoltcp's `Device` so its `RxToken` reads out of a filled UMEM frame and its
`TxToken` writes into a frame handed to the TX ring, adding and stripping the
14-byte Ethernet header ourselves. There is no ARP — the peer MAC is
configured. smoltcp owns its socket buffers, so there is one unavoidable copy
from the UMEM frame into the smoltcp RX buffer.

**UDP** skips smoltcp's stack entirely and writes Ethernet/IPv4/UDP straight
into a UMEM frame, using `smoltcp::wire` reprs only to emit and parse headers
so checksums are not reinvented. UDP `send` splits the buffer into
`mtu - 28`-byte datagrams (1472 on a standard link) and sends all of them. A
short `recv` truncates like `SOCK_DGRAM`, and a full TX pool drops a datagram,
which is within the UDP contract.

The MTU is the interface's own, read from `/sys/class/net/<if>/mtu` by
`read_mtu` and passed in `Config`; it sizes UDP datagrams and smoltcp's TCP
segments alike. `new` rejects an MTU that cannot hold an IPv4+UDP header, and
`frame_size_for` rejects one whose frames would exceed a page — aligned AF_XDP
chunks cannot go past `PAGE_SIZE`, and one frame is one packet here, there is
no scatter-gather on this path.

RX is one frame, one datagram: there is no IP reassembly, so an inbound
datagram whose sender had to fragment it is dropped rather than
half-delivered.

Checksums are computed on TX but deliberately *not* verified on RX: over veth
the kernel offloads TX checksums, so inbound frames captured via AF_XDP carry
uncomputed or partial checksums.

The local port comes from the kernel, not from us. AF_XDP `bind` is layer 2 —
`(ifindex, queue_id)`, no port field — so nothing reserves an L4 port on our
behalf, and smoltcp refuses port 0. Instead of inventing a number we bind a
throwaway `AF_INET` socket (matching the transport, since TCP and UDP have
separate port spaces) to port 0, read back what the kernel assigned, and hold
that fd for the socket's life. The open fd *is* the reservation: the host
allocator will not give the same port to anything else, so no other socket on
the machine can present our peer an identical 4-tuple. The bind is wildcard
rather than `our_ip`, so it works even when the kernel has no address on the
interface. That socket never carries traffic — inbound is redirected before
the stack sees it, outbound bypasses the stack.

## Configuration and privileges

`Config` supplies a resolved interface index, our IPv4 and MAC, the peer MAC,
the queue id and the link MTU. `connect` inserts the full 4-tuple —
`(peer_ip, peer_port, our_ip, reserved_port)` — into the config map, and the
socket's `Drop` removes that same key.

It takes an index rather than a name deliberately. The caller already has to
resolve the name to attach XDP, and resolving it a second time inside the
socket lets the two drift: delete and recreate the interface in between and
the program is attached to one index while the socket binds to another, which
fails silently as "no packets ever arrive". One resolution, one index.

Loading the XDP program, creating the AF_XDP socket, and registering UMEM need
more than `CAP_NET_ADMIN` + `CAP_NET_RAW`. The full set, each verified against
a real veth:

- `CAP_NET_RAW` — open the `AF_XDP` socket.
- `CAP_NET_ADMIN` — attach the XDP program to the interface.
- `CAP_BPF` — load the program (`kernel.unprivileged_bpf_disabled=2` on most
  distros means an unprivileged load is refused outright).
- `CAP_PERFMON` — the program does variable pointer arithmetic on a packet
  pointer (`iph + ihl`), which the Spectre-v1 mitigation rejects without
  `bypass_spec_v1`.
- `CAP_IPC_LOCK` — the UMEM is charged against `RLIMIT_MEMLOCK`, which commonly
  has an 8 MiB hard cap. Without this, `XDP_UMEM_REG` fails `ENOBUFS`.

So either run as root, or:

```
setcap cap_bpf,cap_net_admin,cap_net_raw,cap_ipc_lock,cap_perfmon+ep <binary>
```

Note that `cargo build` rewrites the binary and drops the `security.capability`
xattr, so `setcap` has to be repeated after every build.

## Test harness

One entry point, `scripts/run.sh`: build, boot the guest, deploy, run both
suites, render flamegraphs. Everything below is what it drives.

`scripts/vm/vm.sh` boots a Fedora Cloud guest (kernel 6.19) on an isolated
libvirt bridge, because veth cannot do zero-copy and the loopback path measures
nothing. The guest NIC is `virtio-net-pci` with `mq=on,rss=on` and, crucially,
`iommu_platform=on,disable-legacy=on`: without `VIRTIO_F_ACCESS_PLATFORM` the
`XDP_ZEROCOPY` bind fails `EINVAL`. Guest CPUs 2 and 3 are isolated
(`isolcpus`), and each vCPU thread is pinned to a host core.

`scripts/suite.sh throughput|latency` runs kernel-socket and `SpeedySocket`
receivers for both transports against `scripts/bench_peer.py`, which blasts a
fixed count of fixed-size messages on request, or ping-pongs them for latency.
Throughput runs report rate, loss, CPU per message and heap allocations, all
measured in the guest; latency runs are timed entirely on the host, so the
round trip needs no clock agreement between the two.

`scripts/suite.sh profile <stack> <proto>` profiles one combination: `perf`
records in the guest where the symbols are, `inferno` renders on the host.
`cpu-clock`, not `cycles` — the guest has no vPMU.

`src/bin/bench.rs` is the reference consumer: one loop that connects, drives
the handshake, sends, and drains the rings on a pinned core.

Three things had to be true before any of it received a byte, and each failed
silently rather than loudly:

- Bind with `XDP_ZEROCOPY`, not `XDP_COPY`.
- Attach the program in native (`DRV_MODE`) XDP. A generic attachment cannot
  feed a zero-copy socket, and redirects simply vanish.
- Reduce the device to one channel. The socket is registered as
  `xsks_map[queue]`; anything steered elsewhere falls through to the kernel.
  Collapsing the RSS indirection table is *not* enough on virtio-net — the host
  tap keeps its own steering, and inbound TCP still landed on ring 1 about half
  the time.

## Status

**Done.** eBPF redirect into the `XSKMAP`; UMEM, rings and bind; `SpeedySocket`
with both transports, polling `send`/`recv`; zero-copy on virtio-net in a
pinned QEMU guest. Verified by `scripts/run.sh` passing both suites, plus unit
tests covering the hand-rolled UDP framing (round-trip, wrong-flow rejection,
non-IPv4 rejection, a full-MTU datagram).

**Not done, in rough order of interest.**

1. **Zero-copy on real hardware.** The virtio-net guest proves the path; a
   capable physical NIC is the payoff the design exists for.
2. **Hand out UMEM frames.** Let `recv` lend the consumer the frame itself
   rather than copying the payload into its slice. Needs a lifetime or
   refcount discipline so the frame returns to the FILL ring on time.
3. **Multiple sockets.** The skeleton is now borrowed, so the API permits
   several sockets on different queues sharing one program. Untested: only
   the one-socket path has been run.
4. **Message framing.** `recv` returns whatever TCP had buffered, not
   messages. A consumer that needs delimited messages reassembles them itself.

## Non-goals

- TLS. Decrypting would force a copy and break the zero-copy path.
- ARP. The peer MAC is configured.
- IPv6.
- Any application-level protocol inside the stack.
