//! `Socket` - an AF_XDP socket shaped like a UNIX socket.
//!
//! ```text
//! Socket::new()  UMEM + 4 rings + bind, load BPF skeleton,
//!                      xsks_map[queue] = fd, attach XDP
//! .connect(remote)     insert (ip, port) into config_map so the XDP program
//!                      starts redirecting; TCP additionally handshakes
//! .send() / .recv()    one pass over the rings on the calling thread
//! drop                 detach XDP
//! ```
//!
//! Single-threaded by construction. `send` and `recv` never block: each drives
//! the rings once on whatever thread calls them and returns what it managed to
//! move, so the caller owns the loop - pin that thread with [`pin_cpu`] if you
//! want a hot core. Nothing here blocks at all: `connect` only sends the SYN,
//! and `poll_connect` reports how the handshake is going.
//!
//! Nothing is shared, so there are no queues, no locks, and no copy
//! beyond the one the transport itself needs: TCP reads land straight in the
//! caller's buffer via smoltcp's `recv_slice`, and UDP payloads are copied once
//! out of the UMEM frame.
//!
//! UMEM frame partition: frames `[0, FILL_SIZE)` back the RX/FILL path, frames
//! `[FILL_SIZE, FRAME_COUNT)` are the TX free pool. Disjoint so RX and TX never
//! contend for a frame. Frame size comes from the interface MTU via
//! [`frame_size_for`], so the region sizes itself to the link.
//!
//! TCP runs on smoltcp at `Medium::Ip` (we add/strip the 14-byte Ethernet header
//! ourselves; no ARP, the peer MAC is configured). UDP bypasses smoltcp's stack
//! entirely and writes Ethernet/IPv4/UDP straight into a UMEM frame, using
//! `smoltcp::wire` only to emit and parse headers.

use std::cell::RefCell;
use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::fd::{AsFd, RawFd};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libbpf_rs::skel::{OpenSkel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags, OpenObject, Xdp, XdpFlags};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Checksum, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{
    HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpProtocol, Ipv4Packet, Ipv4Repr, UdpPacket,
    UdpRepr, IPV4_HEADER_LEN, UDP_HEADER_LEN,
};

pub mod skel {
    include!(concat!(env!("OUT_DIR"), "/xskip.skel.rs"));
}
use skel::*;

use libc::{
    sockaddr_xdp, xdp_desc, xdp_mmap_offsets, xdp_ring_offset, xdp_umem_reg, AF_XDP, SOL_XDP,
    XDP_COPY, XDP_MMAP_OFFSETS, XDP_PGOFF_RX_RING, XDP_PGOFF_TX_RING, XDP_RING_NEED_WAKEUP,
    XDP_RX_RING, XDP_TX_RING, XDP_UMEM_COMPLETION_RING, XDP_UMEM_FILL_RING,
    XDP_UMEM_PGOFF_COMPLETION_RING, XDP_UMEM_PGOFF_FILL_RING, XDP_UMEM_REG, XDP_USE_NEED_WAKEUP,
    XDP_ZEROCOPY,
};

/// Frames are sized from the interface MTU by [`frame_size_for`]; this is how
/// many of them the UMEM holds, so the UMEM itself scales with the link.
const FRAME_COUNT: u32 = 4096;
const FILL_SIZE: u32 = 2048;
const RX_SIZE: u32 = 2048;
const TX_SIZE: u32 = 2048;

const ETH_HDR_LEN: usize = 14;
const UDP_OVERHEAD: usize = IPV4_HEADER_LEN + UDP_HEADER_LEN;
/// `XDP_UMEM_MIN_CHUNK_SIZE`: the kernel refuses a smaller UMEM chunk.
const MIN_CHUNK_SIZE: usize = 2048;

fn frame_size_for(mtu: usize, page_size: usize) -> io::Result<u32> {
    let size = (ETH_HDR_LEN + mtu).next_power_of_two().max(MIN_CHUNK_SIZE);
    if size > page_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "mtu {mtu} needs {size}-byte UMEM frames, over the {page_size}-byte \
                 page limit for aligned chunks"
            ),
        ));
    }
    Ok(size as u32)
}

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent(pub usize);

fn checksum_caps() -> ChecksumCapabilities {
    let mut c = ChecksumCapabilities::default();
    c.ipv4 = Checksum::Tx;
    c.tcp = Checksum::Tx;
    c.udp = Checksum::Tx;
    c.icmpv4 = Checksum::Tx;
    c
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

/// UMEM backing pages. The kernel only accepts sizes its hugetlb pools are
/// built for, so this is a fixed list rather than a free-form byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Gb1 is a selectable option, not yet selected here.
pub enum HugePage {
    Off,
    Mb2,
    Gb1,
}

impl HugePage {
    fn bytes(self) -> Option<usize> {
        match self {
            HugePage::Off => None,
            HugePage::Mb2 => Some(2 << 20),
            HugePage::Gb1 => Some(1 << 30),
        }
    }

    fn label(self) -> &'static str {
        match self {
            HugePage::Off => "normal pages (hugetlb pool empty; sysctl vm.nr_hugepages)",
            HugePage::Mb2 => "2 MiB hugepages",
            HugePage::Gb1 => "1 GiB hugepages",
        }
    }
}

/// AF_XDP bind mode. `Copy` works everywhere; `ZeroCopy` needs a driver with
/// `ndo_xsk_wakeup` and fails `EOPNOTSUPP` on one without it (veth, for one).
/// `Auto` asks for zero-copy and falls back rather than refusing to bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdpMode {
    Copy,
    ZeroCopy,
    Auto,
}

impl XdpMode {
    /// Bind flags to try, in order, each with the name to report if it takes.
    fn attempts(self) -> &'static [(u16, &'static str)] {
        const ZC: (u16, &str) = (XDP_ZEROCOPY, "zero-copy");
        const CP: (u16, &str) = (XDP_COPY, "copy mode");
        match self {
            XdpMode::Copy => &[CP],
            XdpMode::ZeroCopy => &[ZC],
            XdpMode::Auto => &[ZC, CP],
        }
    }
}

/// How long `connect` may spend on a TCP handshake before a caller should
/// give up. The socket does not enforce it; the caller owns the loop.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Config {
    pub ifindex: u32,
    pub our_ip: Ipv4Addr,
    pub our_mac: [u8; 6],
    pub peer_mac: [u8; 6],
    pub queue_id: u32,
    pub mtu: usize,
    pub hugepage: HugePage,
    pub xdp_mode: XdpMode,
}

pub struct Socket<'obj> {
    skel: &'obj XskipSkel<'obj>,
    queue_id: u32,
    mtu: usize,
    our_ip: Ipv4Addr,
    port: PortReservation,
    connected: bool,
    remote: Option<SocketAddrV4>,
    plane: Plane,
}

impl<'obj> Socket<'obj> {
    /// Build the UMEM and rings, bind, and register in `xsks_map` so the
    /// already-attached XDP program can redirect to us.
    pub fn new(
        skel: &'obj XskipSkel<'obj>,
        proto: Protocol,
        cfg: Config,
    ) -> io::Result<Socket<'obj>> {
        if cfg.mtu <= UDP_OVERHEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("mtu {} leaves no room for an IPv4+UDP header", cfg.mtu),
            ));
        }

        // Frames are sized to the link, so the UMEM is too.
        let xsk = Xsk::new(
            cfg.ifindex,
            cfg.queue_id,
            cfg.mtu,
            cfg.hugepage,
            cfg.xdp_mode,
        )?;
        eprintln!(
            "-- UMEM: {} MiB, {} x {}-byte frames on {}; bound {} on queue {} --",
            xsk.umem_len >> 20,
            FRAME_COUNT,
            xsk.frame_size,
            xsk.huge.label(),
            xsk.bind_label,
            cfg.queue_id,
        );

        // One socket, one ring: a packet landing on any other RX queue finds no
        // entry here and falls through to the kernel, with nothing logged
        // anywhere. On a multi-queue NIC the receiver must cut the device to one
        // channel, or the flow is steered by hash and arrives roughly never.
        skel.maps
            .xsks_map
            .update(
                &cfg.queue_id.to_ne_bytes(),
                &(xsk.fd as u32).to_ne_bytes(),
                MapFlags::ANY,
            )
            .map_err(|e| io::Error::other(format!("insert socket fd into xsks_map: {e}")))?;

        // Take the port from the kernel's own allocator rather than inventing
        // one, so nothing else on the host can be handed the same 4-tuple.
        let port = PortReservation::take(proto)?;
        let local_port = port.port();
        let plane = match proto {
            Protocol::Tcp => Plane::Tcp(TcpPlane::new(
                xsk,
                cfg.our_ip,
                cfg.our_mac,
                cfg.peer_mac,
                cfg.mtu,
            )),
            Protocol::Udp => Plane::Udp(UdpPlane {
                xsk,
                framing: Framing {
                    src_mac: cfg.our_mac,
                    dst_mac: cfg.peer_mac,
                    local: SocketAddrV4::new(cfg.our_ip, local_port),
                    remote: None,
                },
            }),
        };

        Ok(Socket {
            skel,
            queue_id: cfg.queue_id,
            mtu: cfg.mtu,
            our_ip: cfg.our_ip,
            port,
            connected: false,
            remote: None,
            plane,
        })
    }

    /// Bind the flow into the XDP program's `config_map` so matching packets
    /// get redirected to us, and start the TCP handshake.
    ///
    /// Non-blocking like everything else here: for TCP the connection is only
    /// usable once [`Self::poll_connect`] reports `true`. UDP is usable at
    /// once, there is nothing to negotiate.
    pub fn connect(&mut self, remote: SocketAddrV4) -> io::Result<()> {
        add_flow_to_ebpf_tracking(&self.skel.maps.config_map, self.local(), remote)?;
        let local_port = self.port.port();
        match &mut self.plane {
            Plane::Tcp(t) => t.start_connect(remote, local_port)?,
            Plane::Udp(u) => u.framing.remote = Some(remote),
        }
        self.remote = Some(remote);
        self.connected = true;
        Ok(())
    }

    pub fn poll_connect(&mut self) -> io::Result<bool> {
        if !self.connected {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        }
        match &mut self.plane {
            Plane::Tcp(t) => t.poll_connect(),
            Plane::Udp(_) => Ok(true),
        }
    }

    /// Our own endpoint: the configured address plus the reserved port.
    pub fn local(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.our_ip, self.port.port())
    }

    pub fn local_port(&self) -> u16 {
        self.port.port()
    }

    fn max_udp_payload(&self) -> usize {
        self.mtu - UDP_OVERHEAD
    }

    pub fn send(&mut self, buf: &[u8]) -> io::Result<Sent> {
        if !self.connected {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        }
        if buf.is_empty() {
            return Ok(Sent(0));
        }
        let max_payload = self.max_udp_payload();
        match &mut self.plane {
            Plane::Udp(u) => {
                for chunk in buf.chunks(max_payload) {
                    u.send(chunk)?;
                }
                // One syscall for the whole buffer, however many datagrams it
                // became.
                u.xsk.kick_tx();
                Ok(Sent(buf.len()))
            }
            Plane::Tcp(t) => t.send(buf).map(Sent),
        }
    }

    /// One pass over the rings. `None` means nothing had arrived, `Some(0)` is
    /// end of stream (TCP only; UDP has no FIN).
    pub fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        debug_assert!(self.connected, "recv before connect");
        if buf.is_empty() || !self.connected {
            return None;
        }
        match &mut self.plane {
            Plane::Tcp(t) => {
                t.poll();
                let s = t.sockets.get_mut::<tcp::Socket>(t.handle);
                // Straight from smoltcp's receive buffer into the caller's.
                if s.can_recv() {
                    if let Ok(n) = s.recv_slice(buf) {
                        if n > 0 {
                            return Some(n);
                        }
                    }
                }
                if !s.is_active() {
                    return Some(0);
                }
                None
            }
            Plane::Udp(u) => u.recv_into(buf),
        }
    }

    pub fn close(&mut self) {
        if let Plane::Tcp(t) = &mut self.plane {
            t.sockets.get_mut::<tcp::Socket>(t.handle).close();
            t.poll();
        }
    }
}

impl Drop for Socket<'_> {
    fn drop(&mut self) {
        if let Some(remote) = self.remote {
            let _ = self
                .skel
                .maps
                .config_map
                .delete(&flow_key(self.local(), remote));
        }
        let _ = self.skel.maps.xsks_map.delete(&self.queue_id.to_ne_bytes());
    }
}

impl io::Read for Socket<'_> {
    /// `WouldBlock` for an empty ring, which is what `Read` has to say.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.recv(buf)
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))
    }
}

impl io::Write for Socket<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send(buf).map(|Sent(n)| n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Resolve an interface name to its index.
pub fn ifindex(ifname: &str) -> io::Result<u32> {
    let c = CString::new(ifname).unwrap_or_default();
    match unsafe { libc::if_nametoindex(c.as_ptr()) } {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface {ifname:?} not found"),
        )),
        idx => Ok(idx),
    }
}

/// Open and load the BPF skeleton into caller-owned storage. `obj` has to
/// outlive every socket built against the result.
pub fn load_skel(obj: &mut MaybeUninit<OpenObject>) -> io::Result<XskipSkel<'_>> {
    XskipSkelBuilder::default()
        .open(obj)
        .map_err(|e| io::Error::other(format!("open skeleton: {e}")))?
        .load()
        .map_err(|e| io::Error::other(format!("load skeleton (verifier): {e}")))
}

/// An attached XDP program, detached on drop.
///
/// Attachment is interface-wide, so it outlives any single socket and is the
/// caller's to hold. Tying it to a guard means no early return can leak it -
/// a stale program keeps stealing packets from the kernel stack, and leaves
/// the queue's AF_XDP pool registered so the next bind fails `EBUSY`.
pub struct XdpAttachment<'a> {
    skel: &'a XskipSkel<'a>,
    ifindex: u32,
    native: bool,
}

/// Native (driver) XDP first, generic second.
///
/// The mode is not cosmetic: an `XDP_ZEROCOPY` socket can only be fed by the
/// driver's own ZC path, so a generic attachment redirects into a void and the
/// socket receives nothing at all. Drivers without native XDP still work in
/// generic mode, in copy mode only.
pub fn attach_xdp<'a>(skel: &'a XskipSkel<'a>, ifindex: u32) -> io::Result<XdpAttachment<'a>> {
    let xdp = Xdp::new(skel.progs.xdp_redirect_flow.as_fd());
    let mut last = io::Error::from(io::ErrorKind::InvalidInput);
    for flags in [XdpFlags::DRV_MODE, XdpFlags::SKB_MODE] {
        let native = flags.bits() == XdpFlags::DRV_MODE.bits();
        match xdp.attach(ifindex as i32, flags) {
            Ok(()) => {
                eprintln!(
                    "-- XDP attached in {} mode --",
                    if native { "native" } else { "generic" }
                );
                return Ok(XdpAttachment {
                    skel,
                    ifindex,
                    native,
                });
            }
            Err(e) => last = io::Error::other(format!("attach xdp program: {e}")),
        }
    }
    Err(last)
}

impl Drop for XdpAttachment<'_> {
    fn drop(&mut self) {
        let xdp = Xdp::new(self.skel.progs.xdp_redirect_flow.as_fd());
        let flags = if self.native {
            XdpFlags::DRV_MODE
        } else {
            XdpFlags::SKB_MODE
        };
        let _ = xdp.detach(self.ifindex as i32, flags);
    }
}

struct PortReservation {
    fd: RawFd,
    port: u16,
}

impl PortReservation {
    fn take(proto: Protocol) -> io::Result<PortReservation> {
        let ty = match proto {
            Protocol::Tcp => libc::SOCK_STREAM,
            Protocol::Udp => libc::SOCK_DGRAM,
        };
        unsafe {
            let fd = libc::socket(libc::AF_INET, ty | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as libc::sa_family_t;
            addr.sin_port = 0; // let the kernel choose
            addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();
            let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;

            if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, len) < 0 {
                let e = io::Error::last_os_error();
                libc::close(fd);
                return Err(io::Error::other(format!("reserve local port: {e}")));
            }

            let mut got: libc::sockaddr_in = std::mem::zeroed();
            let mut got_len = len;
            if libc::getsockname(fd, &mut got as *mut _ as *mut libc::sockaddr, &mut got_len) < 0 {
                let e = io::Error::last_os_error();
                libc::close(fd);
                return Err(io::Error::other(format!("read reserved port: {e}")));
            }

            Ok(PortReservation {
                fd,
                port: u16::from_be(got.sin_port),
            })
        }
    }

    fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for PortReservation {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

fn flow_key(local: SocketAddrV4, remote: SocketAddrV4) -> [u8; 12] {
    let mut key = [0u8; 12];
    key[0..4].copy_from_slice(&remote.ip().octets());
    key[4..6].copy_from_slice(&remote.port().to_be_bytes());
    key[6..10].copy_from_slice(&local.ip().octets());
    key[10..12].copy_from_slice(&local.port().to_be_bytes());
    key
}

/// Insert our flow into `config_map`, which is what starts the redirect.
fn add_flow_to_ebpf_tracking<M: MapCore>(
    map: &M,
    local: SocketAddrV4,
    remote: SocketAddrV4,
) -> io::Result<()> {
    map.update(&flow_key(local, remote), &[1u8], MapFlags::ANY)
        .map_err(|e| io::Error::other(format!("insert flow into config_map: {e}")))
}

/// Pin the current thread to `cpu`. Best-effort: a failure is reported, not fatal.
pub fn pin_cpu(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0 {
            eprintln!("-- pinned to CPU {cpu} --");
        } else {
            eprintln!(
                "-- CPU pin failed (continuing): {} --",
                io::Error::last_os_error()
            );
        }
    }
}

pub fn read_mac(ifname: &str) -> io::Result<[u8; 6]> {
    let s = std::fs::read_to_string(format!("/sys/class/net/{ifname}/address"))?;
    parse_mac(s.trim()).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The interface's configured MTU, straight from the kernel. Changing it with
/// `ip link set dev <if> mtu N` changes what the socket puts on the wire.
pub fn read_mtu(ifname: &str) -> io::Result<usize> {
    let s = std::fs::read_to_string(format!("/sys/class/net/{ifname}/mtu"))?;
    s.trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("mtu of {ifname}: {e}")))
}

pub fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let mut out = [0u8; 6];
    let mut n = 0;
    for part in s.split(':') {
        if n == 6 {
            return Err(format!("bad MAC {s:?}"));
        }
        out[n] = u8::from_str_radix(part, 16).map_err(|_| format!("bad MAC {s:?}"))?;
        n += 1;
    }
    if n != 6 {
        return Err(format!("bad MAC {s:?}"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Protocol planes
// ---------------------------------------------------------------------------

enum Plane {
    Tcp(TcpPlane),
    Udp(UdpPlane),
}

struct TcpPlane {
    xsk: Rc<RefCell<Xsk>>,
    device: XskDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
    handle: SocketHandle,
    clock: Instant,
    ticks_left: u32,
}

/// Polls served from one clock reading before taking another.
const CLOCK_TICKS: u32 = 32;

impl TcpPlane {
    fn new(xsk: Xsk, our_ip: Ipv4Addr, src_mac: [u8; 6], dst_mac: [u8; 6], mtu: usize) -> TcpPlane {
        let xsk = Rc::new(RefCell::new(xsk));
        let mut device = XskDevice {
            xsk: xsk.clone(),
            src_mac,
            dst_mac,
            mtu,
        };

        let mut config = IfaceConfig::new(HardwareAddress::Ip);
        config.random_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let mut iface = Interface::new(config, &mut device, Instant::now());
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(IpAddress::from(our_ip), 24));
        });

        let mut sockets = SocketSet::new(Vec::new());
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; 65535]),
            tcp::SocketBuffer::new(vec![0u8; 65535]),
        );
        // Nagle holds a small trailing segment until the previous one is
        // acknowledged, and the peer delays that ACK ~40 ms. A message that
        // straddles the MSS therefore pays a 40 ms stall per round trip:
        // measured at 1472 B, where the echo splits into 1460 + 12. This is
        // what TCP_NODELAY turns off on a kernel socket, and the whole point
        // of the stack is latency.
        socket.set_nagle_enabled(false);
        let handle = sockets.add(socket);

        TcpPlane {
            xsk,
            device,
            iface,
            sockets,
            handle,
            clock: Instant::now(),
            ticks_left: 0,
        }
    }

    fn now(&mut self) -> Instant {
        if self.ticks_left == 0 {
            self.clock = Instant::now();
            self.ticks_left = CLOCK_TICKS;
        }
        self.ticks_left -= 1;
        self.clock
    }

    fn poll(&mut self) {
        let now = self.now();
        // One poll can emit several segments through `TxToken`; they leave
        // together on the kick below.
        self.iface.poll(now, &mut self.device, &mut self.sockets);
        let mut xsk = self.xsk.borrow_mut();
        xsk.kick_tx();
        xsk.kick_rx();
    }

    /// Hand as much of `buf` to smoltcp's send buffer as it will take right
    /// now, and return how much that was.
    ///
    /// Short by design: a shut window or a peer that stopped reading ends the
    /// loop instead of spinning in it, so the caller keeps control and can
    /// check for a signal, drain RX, or give up. `Ok(0)` means the socket took
    /// nothing - either its buffer is full or the send half is closed.
    fn send(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut sent = 0;
        while sent < buf.len() {
            let s = self.sockets.get_mut::<tcp::Socket>(self.handle);
            if !s.may_send() {
                break;
            }
            // Copies whatever the send buffer has room for. A short copy means
            // it is full, and only an ACK can free it, so looping here without
            // polling would spin: break and let the caller come back.
            match s.send_slice(&buf[sent..]) {
                Ok(0) => break,
                Ok(n) => sent += n,
                Err(e) => return Err(io::Error::other(format!("tcp send: {e:?}"))),
            }
        }
        self.poll(); // get it on the wire before returning
        Ok(sent)
    }

    /// Send the SYN. The handshake itself is driven by [`Self::poll_connect`].
    fn start_connect(&mut self, remote: SocketAddrV4, local_port: u16) -> io::Result<()> {
        let endpoint = IpEndpoint::new(IpAddress::from(*remote.ip()), remote.port());
        self.sockets
            .get_mut::<tcp::Socket>(self.handle)
            .connect(self.iface.context(), endpoint, local_port)
            .map_err(|e| io::Error::other(format!("tcp connect: {e:?}")))?;
        self.poll();
        Ok(())
    }

    /// One pass of the handshake: `true` once established, `false` while it is
    /// still in flight.
    fn poll_connect(&mut self) -> io::Result<bool> {
        self.poll();
        match self.sockets.get::<tcp::Socket>(self.handle).state() {
            tcp::State::Established => Ok(true),
            // The SYN was answered with an RST, or the socket never opened.
            tcp::State::Closed => Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "connection refused",
            )),
            _ => Ok(false),
        }
    }
}

struct UdpPlane {
    xsk: Xsk,
    framing: Framing,
}

impl UdpPlane {
    /// Returns the payload bytes accepted, like `sendto`: a full TX pool drops
    /// the datagram on the floor but still reports it sent - UDP is lossy by
    /// contract, and a drop here is indistinguishable from a drop on the wire.
    fn send(&mut self, payload: &[u8]) -> io::Result<usize> {
        if self.framing.remote.is_none() {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        }
        if let Some(addr) = self.xsk.tx_alloc() {
            let framing = self.framing;
            let n = framing.build(self.xsk.frame_mut(addr), payload);
            self.xsk.tx_submit(addr, n as u32);
        }
        Ok(payload.len())
    }

    /// Copy one datagram from the peer into `buf`, truncating like `SOCK_DGRAM`.
    fn recv_into(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.framing.remote?;
        self.xsk.kick_rx();
        let (addr, len) = self.xsk.rx_peek()?;
        let framing = self.framing;
        let n = framing.parse(self.xsk.frame(addr, len)).map(|p| {
            let n = p.len().min(buf.len());
            buf[..n].copy_from_slice(&p[..n]);
            n
        });
        self.xsk.recycle(addr);
        n.filter(|&n| n > 0)
    }
}

/// Ethernet/IPv4/UDP header construction and matching for the UDP plane.
#[derive(Clone, Copy)]
struct Framing {
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    local: SocketAddrV4,
    remote: Option<SocketAddrV4>,
}

impl Framing {
    /// Write a complete frame for `payload` into `frame`; returns its length.
    /// `frame` must hold `ETH_HDR_LEN + UDP_OVERHEAD + payload.len()` bytes.
    fn build(&self, frame: &mut [u8], payload: &[u8]) -> usize {
        let remote = self.remote.expect("build without remote");
        let total = ETH_HDR_LEN + UDP_OVERHEAD + payload.len();

        frame[0..6].copy_from_slice(&self.dst_mac);
        frame[6..12].copy_from_slice(&self.src_mac);
        frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());

        let caps = checksum_caps();
        let ip = Ipv4Repr {
            src_addr: *self.local.ip(),
            dst_addr: *remote.ip(),
            next_header: IpProtocol::Udp,
            payload_len: UDP_HEADER_LEN + payload.len(),
            hop_limit: 64,
        };
        let mut packet = Ipv4Packet::new_unchecked(&mut frame[ETH_HDR_LEN..total]);
        ip.emit(&mut packet, &caps);

        let udp = UdpRepr {
            src_port: self.local.port(),
            dst_port: remote.port(),
        };
        let mut datagram = UdpPacket::new_unchecked(packet.payload_mut());
        udp.emit(
            &mut datagram,
            &IpAddress::Ipv4(*self.local.ip()),
            &IpAddress::Ipv4(*remote.ip()),
            payload.len(),
            |b| b.copy_from_slice(payload),
            &caps,
        );
        total
    }

    fn parse<'a>(&self, frame: &'a [u8]) -> Option<&'a [u8]> {
        let remote = self.remote?;
        if frame.len() < ETH_HDR_LEN || frame[12..14] != [0x08, 0x00] {
            return None;
        }
        let caps = checksum_caps();
        let packet = Ipv4Packet::new_checked(&frame[ETH_HDR_LEN..]).ok()?;
        let ip = Ipv4Repr::parse(&packet, &caps).ok()?;
        if ip.next_header != IpProtocol::Udp
            || ip.src_addr != *remote.ip()
            || ip.dst_addr != *self.local.ip()
        {
            return None;
        }
        let datagram = UdpPacket::new_checked(packet.payload()).ok()?;
        let udp = UdpRepr::parse(
            &datagram,
            &IpAddress::Ipv4(ip.src_addr),
            &IpAddress::Ipv4(ip.dst_addr),
            &caps,
        )
        .ok()?;
        if udp.src_port != remote.port() || udp.dst_port != self.local.port() {
            return None;
        }
        Some(datagram.payload())
    }
}

// ---------------------------------------------------------------------------
// Raw AF_XDP: UMEM + the four rings
// ---------------------------------------------------------------------------

/// A single producer/consumer ring mapped out of the kernel.
struct Ring {
    producer: *mut u32,
    consumer: *mut u32,
    flags: *mut u32,
    desc: *mut u8,
    mask: u32,
    size: u32,
    map: *mut libc::c_void,
    map_len: usize,
    cached: u32, // our-side producer index (FILL, TX)
}

impl Ring {
    unsafe fn map(
        fd: RawFd,
        off: &xdp_ring_offset,
        size: u32,
        elem: usize,
        pgoff: libc::off_t,
    ) -> io::Result<Ring> {
        let map_len = off.desc as usize + size as usize * elem;
        let map = libc::mmap(
            ptr::null_mut(),
            map_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            fd,
            pgoff,
        );
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = map as *mut u8;
        Ok(Ring {
            producer: base.add(off.producer as usize) as *mut u32,
            consumer: base.add(off.consumer as usize) as *mut u32,
            flags: base.add(off.flags as usize) as *mut u32,
            desc: base.add(off.desc as usize),
            mask: size - 1,
            size,
            map,
            map_len,
            cached: 0,
        })
    }

    #[inline]
    fn producer(&self) -> &AtomicU32 {
        unsafe { AtomicU32::from_ptr(self.producer) }
    }
    #[inline]
    fn consumer(&self) -> &AtomicU32 {
        unsafe { AtomicU32::from_ptr(self.consumer) }
    }
    #[inline]
    fn needs_wakeup(&self) -> bool {
        unsafe {
            AtomicU32::from_ptr(self.flags).load(Ordering::Acquire) & XDP_RING_NEED_WAKEUP != 0
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        if !self.map.is_null() {
            unsafe { libc::munmap(self.map, self.map_len) };
        }
    }
}

/// The AF_XDP socket itself: its own UMEM plus the RX/TX/FILL/COMPLETION rings.
struct Xsk {
    fd: RawFd,
    umem: *mut u8,
    umem_len: usize,
    frame_size: u32,
    fill: Ring,
    comp: Ring,
    rx: Ring,
    tx: Ring,
    tx_free: Vec<u64>,
    /// Frames published to the TX ring that the driver has not been told
    /// about. Without it a kick in the poll loop fires on every idle spin,
    /// because `needs_wakeup` stays set whether or not we queued anything.
    tx_pending: bool,
    huge: HugePage,
    bind_label: &'static str,
}

impl Xsk {
    /// AF_XDP socket on `ifindex`/`queue_id` with a dedicated UMEM, its frames
    /// sized to hold one `mtu`-byte packet each.
    fn new(
        ifindex: u32,
        queue_id: u32,
        mtu: usize,
        hugepage: HugePage,
        mode: XdpMode,
    ) -> io::Result<Xsk> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let frame_size = frame_size_for(mtu, page_size)?;
        unsafe {
            let fd = libc::socket(AF_XDP, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            let umem_len = frame_size as usize * FRAME_COUNT as usize;
            let (umem, huge) = match map_umem(umem_len, hugepage) {
                Ok(v) => v,
                Err(e) => {
                    libc::close(fd);
                    return Err(e);
                }
            };

            let reg = xdp_umem_reg {
                addr: umem as u64,
                len: umem_len as u64,
                chunk_size: frame_size,
                headroom: 0,
                flags: 0,
                tx_metadata_len: 0,
            };
            setsockopt(fd, XDP_UMEM_REG, &reg).map_err(|e| umem_reg_error(e, umem_len))?;
            setsockopt(fd, XDP_UMEM_FILL_RING, &FILL_SIZE)?;
            setsockopt(fd, XDP_UMEM_COMPLETION_RING, &TX_SIZE)?;
            setsockopt(fd, XDP_RX_RING, &RX_SIZE)?;
            setsockopt(fd, XDP_TX_RING, &TX_SIZE)?;

            // libc's UAPI structs are plain repr(C) with no Default.
            let mut off = std::mem::zeroed::<xdp_mmap_offsets>();
            let mut len = std::mem::size_of::<xdp_mmap_offsets>() as libc::socklen_t;
            if libc::getsockopt(
                fd,
                SOL_XDP,
                XDP_MMAP_OFFSETS,
                &mut off as *mut _ as *mut libc::c_void,
                &mut len,
            ) < 0
            {
                let e = io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }

            let fill = Ring::map(
                fd,
                &off.fr,
                FILL_SIZE,
                std::mem::size_of::<u64>(),
                XDP_UMEM_PGOFF_FILL_RING as libc::off_t,
            )?;
            let comp = Ring::map(
                fd,
                &off.cr,
                TX_SIZE,
                std::mem::size_of::<u64>(),
                XDP_UMEM_PGOFF_COMPLETION_RING as libc::off_t,
            )?;
            let rx = Ring::map(
                fd,
                &off.rx,
                RX_SIZE,
                std::mem::size_of::<xdp_desc>(),
                XDP_PGOFF_RX_RING,
            )?;
            let tx = Ring::map(
                fd,
                &off.tx,
                TX_SIZE,
                std::mem::size_of::<xdp_desc>(),
                XDP_PGOFF_TX_RING,
            )?;

            // TX free pool = the upper frames, disjoint from the RX/FILL frames.
            let mut tx_free = Vec::with_capacity((FRAME_COUNT - FILL_SIZE) as usize);
            for i in FILL_SIZE..FRAME_COUNT {
                tx_free.push(i as u64 * frame_size as u64);
            }

            let mut xsk = Xsk {
                fd,
                umem,
                umem_len,
                frame_size,
                fill,
                comp,
                rx,
                tx,
                tx_free,
                tx_pending: false,
                huge,
                bind_label: "",
            };

            // Hand the kernel one frame per FILL slot to receive into.
            xsk.fill_frames(FILL_SIZE);

            let mut last = io::Error::from(io::ErrorKind::InvalidInput);
            for &(flag, label) in mode.attempts() {
                let sxdp = sockaddr_xdp {
                    sxdp_family: AF_XDP as u16,
                    sxdp_flags: flag | XDP_USE_NEED_WAKEUP,
                    sxdp_ifindex: ifindex,
                    sxdp_queue_id: queue_id,
                    sxdp_shared_umem_fd: 0,
                };
                if libc::bind(
                    fd,
                    &sxdp as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<sockaddr_xdp>() as libc::socklen_t,
                ) == 0
                {
                    xsk.bind_label = label;
                    return Ok(xsk);
                }
                last = io::Error::last_os_error();
            }
            Err(last)
        }
    }

    /// Whole frame at `addr`, for building a packet in place.
    fn frame_mut(&mut self, addr: u64) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(self.umem.add(addr as usize), self.frame_size as usize)
        }
    }

    fn frame(&self, addr: u64, len: u32) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.umem.add(addr as usize), len as usize) }
    }

    /// Produce up to `n` UMEM frame addresses into the FILL ring (bootstrap only).
    fn fill_frames(&mut self, n: u32) {
        let cons = self.fill.consumer().load(Ordering::Acquire);
        let free = self.fill.size - (self.fill.cached.wrapping_sub(cons));
        let n = n.min(free);
        for _ in 0..n {
            let idx = (self.fill.cached & self.fill.mask) as usize;
            let addr = (self.fill.cached as u64 * self.frame_size as u64) % self.umem_len as u64;
            unsafe { *(self.fill.desc as *mut u64).add(idx) = addr };
            self.fill.cached = self.fill.cached.wrapping_add(1);
        }
        if n > 0 {
            self.fill
                .producer()
                .store(self.fill.cached, Ordering::Release);
        }
    }

    /// Frame-aligned base of a UMEM address (frame_size is a power of two).
    fn frame_base(&self, addr: u64) -> u64 {
        addr & !(self.frame_size as u64 - 1)
    }

    /// Recycle one consumed RX frame back to the FILL ring.
    fn recycle(&mut self, addr: u64) {
        let base = self.frame_base(addr);
        let idx = (self.fill.cached & self.fill.mask) as usize;
        unsafe { *(self.fill.desc as *mut u64).add(idx) = base };
        self.fill.cached = self.fill.cached.wrapping_add(1);
        self.fill
            .producer()
            .store(self.fill.cached, Ordering::Release);
    }

    /// Pop one received frame descriptor `(addr, len)`, advancing the RX consumer.
    fn rx_peek(&mut self) -> Option<(u64, u32)> {
        let prod = self.rx.producer().load(Ordering::Acquire);
        let cons = self.rx.consumer().load(Ordering::Acquire);
        if cons == prod {
            return None;
        }
        let idx = (cons & self.rx.mask) as usize;
        let desc = unsafe { *(self.rx.desc as *const xdp_desc).add(idx) };
        self.rx
            .consumer()
            .store(cons.wrapping_add(1), Ordering::Release);
        Some((desc.addr, desc.len))
    }

    /// Reclaim transmitted frames from the COMPLETION ring back into the free pool.
    fn reclaim_comp(&mut self) {
        let prod = self.comp.producer().load(Ordering::Acquire);
        let mut cons = self.comp.consumer().load(Ordering::Acquire);
        while cons != prod {
            let idx = (cons & self.comp.mask) as usize;
            let addr = unsafe { *(self.comp.desc as *const u64).add(idx) };
            let base = self.frame_base(addr);
            self.tx_free.push(base);
            cons = cons.wrapping_add(1);
        }
        self.comp.consumer().store(cons, Ordering::Release);
    }

    /// Grab a free TX frame offset, reclaiming completions if the pool is empty.
    fn tx_alloc(&mut self) -> Option<u64> {
        if self.tx_free.is_empty() {
            self.reclaim_comp();
        }
        self.tx_free.pop()
    }

    /// Queue a frame `[addr, addr+len)` on the TX ring.
    ///
    /// Does not kick: AF_XDP transmits nothing until a syscall says so, but
    /// one syscall covers every descriptor published so far, so the caller
    /// kicks once at the end of a batch rather than once per frame.
    fn tx_submit(&mut self, addr: u64, len: u32) {
        let idx = (self.tx.cached & self.tx.mask) as usize;
        unsafe {
            let d = (self.tx.desc as *mut xdp_desc).add(idx);
            (*d).addr = addr;
            (*d).len = len;
            (*d).options = 0;
        }
        self.tx.cached = self.tx.cached.wrapping_add(1);
        self.tx.producer().store(self.tx.cached, Ordering::Release);
        self.tx_pending = true;
    }

    /// Hand the queued frames to the driver. Mandatory after `tx_submit`:
    /// nothing leaves the ring on its own. A no-op when nothing was queued,
    /// which is most calls in a polling loop.
    fn kick_tx(&mut self) {
        if !self.tx_pending {
            return;
        }
        self.tx_pending = false;
        if self.tx.needs_wakeup() {
            unsafe {
                libc::sendto(self.fd, ptr::null(), 0, libc::MSG_DONTWAIT, ptr::null(), 0);
            }
        }
    }

    /// Nudge the driver to service the FILL ring if it has gone idle.
    fn kick_rx(&mut self) {
        if self.fill.needs_wakeup() {
            unsafe {
                libc::recvfrom(
                    self.fd,
                    ptr::null_mut(),
                    0,
                    libc::MSG_DONTWAIT,
                    ptr::null_mut(),
                    ptr::null_mut(),
                );
            }
        }
    }
}

impl Drop for Xsk {
    fn drop(&mut self) {
        unsafe {
            if !self.umem.is_null() {
                libc::munmap(self.umem as *mut libc::c_void, self.umem_len);
            }
            libc::close(self.fd);
        }
    }
}

unsafe fn umem_reg_error(e: io::Error, umem_len: usize) -> io::Error {
    if e.raw_os_error() != Some(libc::ENOBUFS) {
        return e;
    }
    let mut lim = std::mem::zeroed::<libc::rlimit>();
    if libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) == 0 && lim.rlim_cur != libc::RLIM_INFINITY {
        return io::Error::other(format!(
            "register {} KiB UMEM: {e} (RLIMIT_MEMLOCK is {} KiB; \
             grant cap_ipc_lock or raise the memlock limit)",
            umem_len / 1024,
            lim.rlim_cur / 1024,
        ));
    }
    e
}

unsafe fn setsockopt<T>(fd: RawFd, name: libc::c_int, val: &T) -> io::Result<()> {
    let r = libc::setsockopt(
        fd,
        SOL_XDP,
        name,
        val as *const T as *const libc::c_void,
        std::mem::size_of::<T>() as libc::socklen_t,
    );
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

unsafe fn map_umem(len: usize, hugepage: HugePage) -> io::Result<(*mut u8, HugePage)> {
    let prot = libc::PROT_READ | libc::PROT_WRITE;

    if let Some(size) = hugepage.bytes().filter(|s| len.is_multiple_of(*s)) {
        // MAP_HUGE_* is just log2(size) parked above MAP_HUGE_SHIFT.
        let encoded = (size.trailing_zeros() as libc::c_int) << libc::MAP_HUGE_SHIFT;
        let p = libc::mmap(
            ptr::null_mut(),
            len,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB | encoded,
            -1,
            0,
        );
        if p != libc::MAP_FAILED {
            return Ok((p as *mut u8, hugepage));
        }
    }

    let p = libc::mmap(
        ptr::null_mut(),
        len,
        prot,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
        -1,
        0,
    );
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok((p as *mut u8, HugePage::Off))
}

// ---------------------------------------------------------------------------
// smoltcp phy::Device over the rings (TCP plane only)
// ---------------------------------------------------------------------------

struct XskDevice {
    xsk: Rc<RefCell<Xsk>>,
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    mtu: usize,
}

impl Device for XskDevice {
    type RxToken<'a> = XskRxToken;
    type TxToken<'a> = XskTxToken;

    fn receive(&mut self, _t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (addr, len, umem) = {
            let mut s = self.xsk.borrow_mut();
            s.kick_rx();
            let (addr, len) = s.rx_peek()?;
            (addr, len, s.umem)
        };
        Some((
            XskRxToken {
                xsk: self.xsk.clone(),
                umem,
                addr,
                len,
            },
            XskTxToken {
                xsk: self.xsk.clone(),
                umem,
                src_mac: self.src_mac,
                dst_mac: self.dst_mac,
            },
        ))
    }

    fn transmit(&mut self, _t: Instant) -> Option<Self::TxToken<'_>> {
        let umem = self.xsk.borrow().umem;
        Some(XskTxToken {
            xsk: self.xsk.clone(),
            umem,
            src_mac: self.src_mac,
            dst_mac: self.dst_mac,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = self.mtu;
        c.checksum = checksum_caps();
        c
    }
}

struct XskRxToken {
    xsk: Rc<RefCell<Xsk>>,
    umem: *mut u8,
    addr: u64,
    len: u32,
}

impl phy::RxToken for XskRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let start = self.addr as usize + ETH_HDR_LEN;
        let ip_len = (self.len as usize).saturating_sub(ETH_HDR_LEN);
        // Borrow straight out of UMEM: smoltcp copies this into its socket
        // buffer (the one boundary copy), then the frame is recycled to FILL.
        let ip = unsafe { std::slice::from_raw_parts(self.umem.add(start), ip_len) };
        let r = f(ip);
        self.xsk.borrow_mut().recycle(self.addr);
        r
    }
}

struct XskTxToken {
    xsk: Rc<RefCell<Xsk>>,
    umem: *mut u8,
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
}

impl phy::TxToken for XskTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut s = self.xsk.borrow_mut();
        let addr = s.tx_alloc().expect("TX frame pool exhausted");
        let base = addr as usize;
        unsafe {
            // Ethernet header: dst, src, EtherType IPv4.
            let hdr = self.umem.add(base);
            ptr::copy_nonoverlapping(self.dst_mac.as_ptr(), hdr, 6);
            ptr::copy_nonoverlapping(self.src_mac.as_ptr(), hdr.add(6), 6);
            *hdr.add(12) = 0x08;
            *hdr.add(13) = 0x00;
        }
        let ip = unsafe { std::slice::from_raw_parts_mut(self.umem.add(base + ETH_HDR_LEN), len) };
        let r = f(ip);
        s.tx_submit(addr, (ETH_HDR_LEN + len) as u32);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `frame_size_for` hands back for a standard 1500-byte link.
    const FRAME_SIZE: u32 = 2048;

    #[test]
    fn frame_size_follows_the_mtu() {
        // Rounded up to a power of two, never under the kernel's 2 KiB floor.
        assert_eq!(frame_size_for(1500, 4096).unwrap(), 2048);
        assert_eq!(frame_size_for(1400, 4096).unwrap(), 2048);
        assert_eq!(frame_size_for(576, 4096).unwrap(), 2048);
        // 3000 + 14 needs the next chunk up.
        assert_eq!(frame_size_for(3000, 4096).unwrap(), 4096);
        // Exactly one page of payload already overflows: the header goes in too.
        assert!(frame_size_for(4096, 4096).is_err());
        assert!(frame_size_for(9000, 4096).is_err());
        // A bigger page is a bigger ceiling.
        assert_eq!(frame_size_for(9000, 65536).unwrap(), 16384);
    }

    fn framing() -> Framing {
        Framing {
            src_mac: [0x02, 0, 0, 0, 0, 1],
            dst_mac: [0x02, 0, 0, 0, 0, 2],
            local: SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 49321),
            remote: Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 6667)),
        }
    }

    /// The peer sees our src/dst swapped.
    fn mirror(f: &Framing) -> Framing {
        Framing {
            src_mac: f.dst_mac,
            dst_mac: f.src_mac,
            local: f.remote.unwrap(),
            remote: Some(f.local),
        }
    }

    #[test]
    fn udp_frame_round_trip() {
        let f = framing();
        let payload = b"payload over udp\r\n";
        let mut frame = [0u8; FRAME_SIZE as usize];
        let n = f.build(&mut frame, payload);
        assert_eq!(n, ETH_HDR_LEN + UDP_OVERHEAD + payload.len());
        assert_eq!(&frame[0..6], &f.dst_mac);
        assert_eq!(&frame[6..12], &f.src_mac);
        assert_eq!(mirror(&f).parse(&frame[..n]), Some(&payload[..]));
    }

    /// The largest datagram `send` builds on a 1500-byte link still frames to
    /// exactly one MTU and parses back whole.
    #[test]
    fn udp_frame_at_max_payload() {
        const MTU: usize = 1500;
        let f = framing();
        let payload = vec![0xabu8; MTU - UDP_OVERHEAD];
        let mut frame = [0u8; FRAME_SIZE as usize];
        let n = f.build(&mut frame, &payload);
        assert_eq!(n, ETH_HDR_LEN + MTU);
        assert_eq!(mirror(&f).parse(&frame[..n]), Some(&payload[..]));
    }

    #[test]
    fn udp_frame_rejects_other_flows() {
        let f = framing();
        let mut frame = [0u8; FRAME_SIZE as usize];
        let n = f.build(&mut frame, b"x");
        let mut peer = mirror(&f);

        // Wrong peer port, wrong peer address, and not-yet-connected all drop.
        peer.remote = Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 1234));
        assert_eq!(peer.parse(&frame[..n]), None);
        peer.remote = Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 9), 49321));
        assert_eq!(peer.parse(&frame[..n]), None);
        peer.remote = None;
        assert_eq!(peer.parse(&frame[..n]), None);
    }

    #[test]
    fn udp_frame_rejects_non_ipv4_ethertype() {
        let f = framing();
        let mut frame = [0u8; FRAME_SIZE as usize];
        let n = f.build(&mut frame, b"x");
        frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
        assert_eq!(mirror(&f).parse(&frame[..n]), None);
    }

    #[test]
    fn mac_parsing() {
        assert_eq!(
            parse_mac("00:11:22:33:44:55").unwrap(),
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55]
        );
        assert!(parse_mac("00:11:22:33:44").is_err());
        assert!(parse_mac("00:11:22:33:44:55:66").is_err());
        assert!(parse_mac("zz:11:22:33:44:55").is_err());
    }

    fn try_bind(port: u16) -> io::Result<()> {
        std::net::TcpListener::bind(("0.0.0.0", port)).map(drop)
    }

    #[test]
    fn reserved_ports_are_held_then_released() {
        let a = PortReservation::take(Protocol::Tcp).unwrap();
        let b = PortReservation::take(Protocol::Tcp).unwrap();
        assert_ne!(a.port(), 0);
        assert_ne!(a.port(), b.port(), "two reservations must differ");

        assert_eq!(
            try_bind(a.port()).unwrap_err().kind(),
            io::ErrorKind::AddrInUse,
            "port {} was not actually reserved",
            a.port()
        );

        let freed = a.port();
        drop(a);
        try_bind(freed).expect("drop must release the port");
    }

    /// The key has to match `struct flow` in the BPF program byte for byte.
    /// Get the order or endianness wrong and nothing errors - the lookup just
    /// never matches and no packet is ever redirected.
    #[test]
    fn flow_key_matches_the_bpf_struct_layout() {
        let local = SocketAddrV4::new(Ipv4Addr::new(10, 99, 0, 1), 49160);
        let remote = SocketAddrV4::new(Ipv4Addr::new(10, 99, 0, 2), 6667);

        assert_eq!(
            flow_key(local, remote),
            [
                10, 99, 0, 2, // remote addr, network order
                0x1a, 0x0b, // remote port 6667, big endian
                10, 99, 0, 1, // local addr
                0xc0, 0x08, // local port 49160
            ]
        );

        // Direction is load-bearing: only peer->us is keyed, so a hairpinned
        // copy of our own output cannot match and be redirected back at us.
        assert_ne!(flow_key(local, remote), flow_key(remote, local));
    }
}
