//! Throughput bench: the same consume loop over an AF_XDP `SpeedySocket` or a
//! plain kernel socket, so the two are timed the same way.
//!
//! Sends one `RUSTSSI <count> <size>\n` control message to the peer, then eats
//! what the peer blasts back and prints one JSON line on stdout.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use rustssi::speedy::{
    self, Config, HugePage, Protocol, Sent, SpeedySocket, XdpMode, CONNECT_TIMEOUT,
};

/// Every heap allocation the process makes, counted at the source.
///
/// A steady-state packet loop should allocate nothing at all; this is how we
/// find out rather than assume. One relaxed add per allocation, which only
/// costs anything if allocations happen, which is the thing being measured.
static ALLOCS: AtomicU64 = AtomicU64::new(0);

struct Counting;

// No `realloc`: the default one calls `alloc`, so growth is counted anyway.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const USAGE: &str = "usage: rustssi-bench --stack <kernel|speedy> --proto <udp|tcp> \
--if <NAME> --local-ip <IPV4> --peer-ip <IPV4> --port <N> --peer-mac <MAC> --cpu <N> \
--queue <N> --count <N> --size <N> [--xdp-mode <copy|zerocopy|auto>] [--mode <consume|echo>]\n\
(--if, --peer-mac, --queue, --xdp-mode are ignored by the kernel stack;\n\
 --mode echo sends every message straight back, for the peer to time round trips)";

/// CPU seconds burned by this process so far. At a sender-bound offered load
/// both stacks report the same message rate, and this is what separates them.
fn cpu_secs() -> f64 {
    let mut u = unsafe { std::mem::zeroed::<libc::rusage>() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) } != 0 {
        return 0.0;
    }
    let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    s(u.ru_utime) + s(u.ru_stime)
}

/// Give up once the peer has been quiet this long: UDP loses the tail, so
/// there is no other way to know the blast is over.
const IDLE: Duration = Duration::from_millis(500);


/// Kernel UDP receive buffer. The 208 KiB default drops most of a blast that
/// the AF_XDP RX ring absorbs, which would measure the buffer, not the stack.
/// The kernel clamps this to `net.core.rmem_max`.
const RCVBUF: libc::c_int = 16 << 20;

/// Either transport on either stack, so one consume loop serves all four.
enum Sock<'obj> {
    Udp(UdpSocket),
    Tcp(TcpStream),
    Speedy(Box<SpeedySocket<'obj>>),
}

impl Sock<'_> {
    fn send_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Sock::Udp(s) => s.send(buf).map(drop),
            Sock::Tcp(s) => s.write_all(buf),
            Sock::Speedy(s) => {
                let mut off = 0;
                while off < buf.len() {
                    let Sent(n) = s.send(&buf[off..])?;
                    off += n;
                }
                Ok(())
            }
        }
    }

    /// `Ok(None)` means nothing arrived: the kernel sockets get there via their
    /// read timeout, the speedy socket by polling an empty ring.
    fn recv(&mut self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        let r = match self {
            Sock::Udp(s) => s.recv(buf),
            Sock::Tcp(s) => s.read(buf),
            Sock::Speedy(s) => return Ok(s.recv(buf)),
        };
        match r {
            Ok(n) => Ok(Some(n)),
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut
                    || e.kind() == io::ErrorKind::WouldBlock =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let stack = arg(&args, "stack")?;
    let proto_name = arg(&args, "proto")?;
    let proto = match proto_name {
        "udp" => Protocol::Udp,
        "tcp" => Protocol::Tcp,
        other => bail!("unknown protocol {other:?}\n{USAGE}"),
    };
    let ifname = arg(&args, "if")?.to_string();
    let local_ip: Ipv4Addr = parse(&args, "local-ip")?;
    let peer_ip: Ipv4Addr = parse(&args, "peer-ip")?;
    let port: u16 = parse(&args, "port")?;
    let peer_mac = speedy::parse_mac(arg(&args, "peer-mac")?).map_err(anyhow::Error::msg)?;
    let cpu: usize = parse(&args, "cpu")?;
    let queue: u32 = parse(&args, "queue")?;
    let count: u64 = parse(&args, "count")?;
    let size: usize = parse(&args, "size")?;
    let xdp_mode = match args.get("xdp-mode").map(String::as_str).unwrap_or("auto") {
        "copy" => XdpMode::Copy,
        "zerocopy" => XdpMode::ZeroCopy,
        "auto" => XdpMode::Auto,
        other => bail!("unknown xdp mode {other:?}\n{USAGE}"),
    };
    if count == 0 || size == 0 {
        bail!("--count and --size must be non-zero\n{USAGE}");
    }

    // Same pinning for both stacks, or the comparison is not one.
    speedy::pin_cpu(cpu);

    let peer = SocketAddrV4::new(peer_ip, port);

    // Declared here so the skeleton and the attachment outlive the socket
    // built from them; drop order is the reverse of declaration.
    let mut obj = MaybeUninit::uninit();
    let skel;
    let _xdp;

    let mut sock = match stack {
        "kernel" => match proto {
            Protocol::Udp => {
                let s = UdpSocket::bind(SocketAddrV4::new(local_ip, 0)).context("bind udp")?;
                s.connect(peer).context("connect udp")?;
                set_rcvbuf(s.as_raw_fd());
                s.set_read_timeout(Some(IDLE))?;
                Sock::Udp(s)
            }
            Protocol::Tcp => {
                let s = TcpStream::connect(peer).context("connect tcp")?;
                s.set_nodelay(true)?;
                s.set_read_timeout(Some(IDLE))?;
                Sock::Tcp(s)
            }
        },
        "speedy" => {
            let ifindex = speedy::ifindex(&ifname).with_context(|| format!("interface {ifname}"))?;
            let our_mac = speedy::read_mac(&ifname).with_context(|| format!("MAC of {ifname}"))?;
            let mtu = speedy::read_mtu(&ifname).with_context(|| format!("MTU of {ifname}"))?;
            skel = speedy::load_skel(&mut obj).context("load bpf skeleton")?;
            _xdp = speedy::attach_xdp(&skel, ifindex).context("attach xdp")?;

            let mut s = SpeedySocket::new(
                &skel,
                proto,
                Config {
                    ifindex,
                    our_ip: local_ip,
                    our_mac,
                    peer_mac,
                    queue_id: queue,
                    mtu,
                    hugepage: HugePage::Mb2,
                    xdp_mode,
                },
            )
            .context("create speedy socket")?;

            s.connect(peer).context("connect")?;
            let deadline = Instant::now() + CONNECT_TIMEOUT;
            while !s.poll_connect().context("connect")? {
                if Instant::now() >= deadline {
                    bail!("handshake to {peer} timed out");
                }
                std::hint::spin_loop();
            }
            Sock::Speedy(Box::new(s))
        }
        other => bail!("unknown stack {other:?}\n{USAGE}"),
    };

    // `--mode echo` turns this into the far end of a ping-pong: every message
    // goes straight back. The peer holds both timestamps, so no clock has to
    // agree with any other clock.
    let echo = args.get("mode").map(String::as_str).unwrap_or("consume") == "echo";
    let want_bytes = count * size as u64;
    let verb = if echo { "ECHO " } else { "" };
    sock.send_all(format!("RUSTSSI {verb}{count} {size}\n").as_bytes())
        .context("send control message")?;

    let mut buf = vec![0u8; 65536];

    let start = Instant::now();
    let cpu0 = cpu_secs();
    // Setup allocates plenty (UMEM, smoltcp buffers, the skeleton, this
    // buffer); only what the consume loop itself allocates is interesting, and
    // that should be nothing at all.
    let allocs0 = ALLOCS.load(Ordering::Relaxed);
    let mut last = start;
    let mut msgs = 0u64;
    let mut bytes = 0u64;
    let mut spins = 0u64;

    loop {
        let done = match proto {
            Protocol::Udp => msgs >= count,
            Protocol::Tcp => bytes >= want_bytes,
        };
        if done {
            break;
        }
        match sock.recv(&mut buf).context("recv")? {
            Some(0) => break, // TCP end of stream
            Some(n) => {
                if echo {
                    sock.send_all(&buf[..n]).context("echo")?;
                }
                msgs += 1;
                bytes += n as u64;
                // Reading the clock per message costs more than receiving one.
                // This only feeds the idle cutoff and the final elapsed, so a
                // batch of 64 is 50 us of slack at a million messages a second.
                if msgs & 63 == 0 {
                    last = Instant::now();
                }
            }
            None => {
                // An empty ring is the common case in a busy-poll loop, so
                // timing every one of them costs more than the receive path.
                spins += 1;
                if spins & 1023 == 0 && last.elapsed() > IDLE {
                    break;
                }
            }
        }
    }

    // A TCP read is an arbitrary slice of the stream, not a message.
    let received = match proto {
        Protocol::Udp => msgs,
        Protocol::Tcp => bytes / size as u64,
    };
    let elapsed = (last - start).as_secs_f64();
    let cpu = cpu_secs() - cpu0;
    let rate = |v: f64| if elapsed > 0.0 { v / elapsed } else { 0.0 };
    let loss = (count.saturating_sub(received)) as f64 * 100.0 / count as f64;
    // Busy-polling burns a core by design, so cost per message is the fair
    // cross-stack number, not CPU seconds.
    let us_per_msg = if received > 0 {
        cpu * 1e6 / received as f64
    } else {
        0.0
    };

    let allocs = ALLOCS.load(Ordering::Relaxed) - allocs0;

    println!(
        "{{\"stack\":\"{stack}\",\"proto\":\"{proto_name}\",\"count\":{count},\"size\":{size},\
\"received\":{received},\"bytes\":{bytes},\"elapsed_s\":{elapsed:.6},\
\"msgs_per_s\":{:.1},\"mbps\":{:.1},\"loss_pct\":{loss:.3},\
\"cpu_s\":{cpu:.6},\"cpu_us_per_msg\":{us_per_msg:.3},\
\"allocs\":{allocs}}}",
        rate(received as f64),
        rate(bytes as f64 * 8.0 / 1e6),
    );
    Ok(())
}

fn parse_args() -> Result<HashMap<String, String>> {
    let mut it = std::env::args().skip(1);
    let mut map = HashMap::new();
    while let Some(flag) = it.next() {
        let key = flag
            .strip_prefix("--")
            .with_context(|| format!("unexpected argument {flag:?}\n{USAGE}"))?;
        if key == "help" {
            bail!("{USAGE}");
        }
        let val = it
            .next()
            .with_context(|| format!("missing value for --{key}\n{USAGE}"))?;
        map.insert(key.to_string(), val);
    }
    Ok(map)
}

fn arg<'a>(map: &'a HashMap<String, String>, key: &str) -> Result<&'a str> {
    map.get(key)
        .map(String::as_str)
        .with_context(|| format!("missing --{key}\n{USAGE}"))
}

fn parse<T: std::str::FromStr>(map: &HashMap<String, String>, key: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    let raw = arg(map, key)?;
    raw.parse()
        .map_err(|e| anyhow::anyhow!("bad --{key} {raw:?}: {e}"))
}

/// Best-effort: the kernel silently clamps to `rmem_max`, and a failure here
/// only costs throughput.
fn set_rcvbuf(fd: std::os::fd::RawFd) {
    let want = RCVBUF;
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            std::ptr::addr_of!(want).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}
