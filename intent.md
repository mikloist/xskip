# Rustssi: zerocopy Rust IRC client

Stupidly fast, overkill-by-design IRC client. Packets land in userspace via
AF_XDP and stay uncopied until the UI needs them.

## Requirements
The client speaks plaintext IRC on port 6667; TLS is out of scope because
decrypting would force a copy and break the zerocopy path. The packet data plane
comes first and must be proven before any IRC or UI work begins. Development runs
against a veth pair in a network namespace using generic (SKB) XDP, so it needs
no special NIC and stays reproducible on any box and in CI. The eBPF program is
written in C and driven from Rust through libbpf-rs.

## Pipeline
1. **eBPF/XDP filter** — redirect only traffic for target IRC address:port into
   an `XSKMAP`; everything else passes to the kernel stack.
2. **AF_XDP** — steer matched packets straight into `UMEM`.
3. **smoltcp** — userspace TCP over the `UMEM` frames; reassemble the byte stream.
4. **Parser** — parse IRC messages from the reassembled bytes, borrowing not
   copying.
5. **No copy until UI** — messages stay as borrows/refs into their buffers until
   the moment they hit the screen.
6. **UI** — ratatui TUI.
7. **Events** — single event stream multiplexing incoming messages and user
   input.

## Build order
1. netns + veth + eBPF redirect into `XSKMAP`; verify with a raw AF_XDP echo.
2. smoltcp over `UMEM`; establish a TCP connection to a local ircd.
3. Zerocopy IRC parser over the reassembled stream.
4. ratatui UI + unified event stream.

## Non-goals (for now)
- TLS / SASL.
- Real physical-NIC native-XDP deployment.
- Multi-network / multi-server sessions.
