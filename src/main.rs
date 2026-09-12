//! AF_XDP pipe: connect a `SpeedySocket` to a peer, then stream stdin -> socket
//! and socket -> stdout.
//!
//! The socket is single-threaded and polling, so this drives it from one loop
//! on a pinned core. stdin gets its own thread only because it is a blocking fd
//! that has nothing to do with the packet path.

mod speedy;

use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use speedy::{Config, HugePage, Protocol, Sent, SpeedySocket};

/// Cleared by SIGINT/SIGTERM. Every loop in this file watches it — the socket
/// itself has no notion of running, it just never blocks.
static RUNNING: AtomicBool = AtomicBool::new(true);

const DEFAULT_CPU: usize = 2;

/// How long a UDP pipe waits for a straggling reply before calling it done.
// ponytail: fixed idle window; promote to a flag if a slow peer ever needs more.
const UDP_IDLE: Duration = Duration::from_millis(250);

/// How long to drive a TCP handshake before giving up on the peer.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

extern "C" fn on_signal(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let usage =
        "usage: rustssi <tcp|udp> <ifname> <our-ipv4> <server-ipv4> <port> <server-mac> [cpu]";

    let proto = match a.next().context(usage)?.as_str() {
        "tcp" => Protocol::Tcp,
        "udp" => Protocol::Udp,
        other => bail!("unknown protocol {other:?}\n{usage}"),
    };
    let ifname = a.next().context(usage)?;
    let our_ip: Ipv4Addr = a.next().context(usage)?.parse().context("bad our-ipv4")?;
    let server_ip: Ipv4Addr = a.next().context(usage)?.parse().context("bad server-ipv4")?;
    let port: u16 = a.next().context(usage)?.parse().context("bad port")?;
    let peer_mac = speedy::parse_mac(&a.next().context(usage)?).map_err(anyhow::Error::msg)?;
    let cpu = match a.next() {
        Some(s) => s.parse().context("bad cpu")?,
        None => DEFAULT_CPU,
    };

    // SIGTERM too: without it a `timeout`/`kill` skips Drop, leaving the XDP
    // program attached and the AF_XDP pool held on the queue, so the next run
    // fails to bind with EBUSY.
    for sig in [libc::SIGINT, libc::SIGTERM] {
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }

    speedy::pin_cpu(cpu);

    // The XDP program and its maps belong to the interface, not to any one
    // socket, so main owns them and every socket borrows them.
    // Resolve the interface once: the same index must be used to attach the
    // program and to bind the socket, or they end up on different netdevs.
    let ifindex = speedy::ifindex(&ifname).with_context(|| format!("interface {ifname}"))?;
    let our_mac = speedy::read_mac(&ifname).with_context(|| format!("MAC of {ifname}"))?;
    let mtu = speedy::read_mtu(&ifname).with_context(|| format!("MTU of {ifname}"))?;
    let mut obj = MaybeUninit::uninit();
    let skel = speedy::load_skel(&mut obj).context("load bpf skeleton")?;
    let _xdp = speedy::attach_xdp(&skel, ifindex).context("attach xdp")?;

    let mut sock = SpeedySocket::new(
        &skel,
        proto,
        Config {
            ifindex,
            our_mac,
            our_ip,
            peer_mac,
            queue_id: 0,
            mtu,
            hugepage: HugePage::Mb2,
        },
    )
    .context("create speedy socket")?;

    sock.connect(SocketAddrV4::new(server_ip, port))
        .context("connect")?;
    // The socket never blocks, so the handshake deadline and the signal check
    // live here, where the flag does.
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while !sock.poll_connect().context("connect")? {
        if !RUNNING.load(Ordering::SeqCst) {
            bail!("interrupted during handshake");
        }
        if Instant::now() >= deadline {
            bail!("handshake to {server_ip}:{port} timed out");
        }
        std::hint::spin_loop();
    }
    eprintln!(
        "-- connected {}:{} -> {}:{} --",
        our_ip,
        sock.local_port(),
        server_ip,
        port
    );

    // stdin on its own thread: it is a blocking fd, and reading it must not
    // stall the ring loop below.
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut chunk = [0u8; 16 * 1024];
        while let Ok(n) = stdin.read(&mut chunk) {
            if n == 0 || stdin_tx.send(chunk[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut out = io::stdout().lock();
    let mut buf = vec![0u8; 65536];
    let mut stdin_done = false;
    let mut closed = false;
    let mut last_rx = Instant::now();
    // Unsent tail: TCP `send` is short when the peer's window is shut, so what
    // it would not take waits here and the loop keeps draining RX meanwhile.
    let mut pending: Vec<u8> = Vec::new();

    while RUNNING.load(Ordering::SeqCst) {
        match stdin_rx.try_recv() {
            Ok(data) => pending.extend_from_slice(&data),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => stdin_done = true,
        }

        if !pending.is_empty() {
            let Sent(n) = sock.send(&pending).context("send")?;
            pending.drain(..n);
        } else if stdin_done && !closed {
            closed = true;
            sock.close(); // TCP FIN; no-op for UDP
        }

        // One pass over the rings, then loop so stdin keeps flowing.
        match sock.recv(&mut buf) {
            Ok(0) => break, // TCP FIN from the peer
            Ok(n) => {
                out.write_all(&buf[..n])?;
                out.flush()?;
                last_rx = Instant::now();
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // UDP has no FIN. Once stdin is drained and the peer has been
                // quiet for UDP_IDLE there is nothing left to wait for; TCP
                // keeps waiting for a real FIN.
                if proto == Protocol::Udp && closed && last_rx.elapsed() > UDP_IDLE {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => break,
            Err(e) => return Err(e.into()),
        }
        std::hint::spin_loop();
    }

    out.flush()?;
    Ok(())
}
