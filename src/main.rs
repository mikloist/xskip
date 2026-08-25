//! Phase 1: prove the data plane.
//!
//! Loads the XDP redirector, tracks a server `(IPv4, port)` endpoint in the BPF
//! config map, opens a copy-mode AF_XDP socket, wires its fd into the XSKMAP,
//! attaches the program in generic (SKB) mode, and prints every frame the
//! kernel redirects into UMEM. Matched traffic shows up here; unmatched traffic
//! never does (it stays on the kernel path).

mod xsk;

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::net::Ipv4Addr;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use libbpf_rs::skel::{OpenSkel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags, Xdp, XdpFlags};

mod skel {
    include!(concat!(env!("OUT_DIR"), "/rustssi.skel.rs"));
}
use skel::*;

const FRAME_SIZE: u32 = 4096;
const FRAME_COUNT: u32 = 4096;
const FILL_SIZE: u32 = 2048;
const RX_SIZE: u32 = 2048;
const QUEUE_ID: u32 = 0;

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn on_signal(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let ifname = args.next().context("usage: rustssi <ifname> <server-ipv4> <port>")?;
    let ip: Ipv4Addr = args
        .next()
        .context("missing server ipv4")?
        .parse()
        .context("bad server ipv4")?;
    let port: u16 = args
        .next()
        .context("missing port")?
        .parse()
        .context("bad port")?;

    // AF_XDP UMEM registration is charged against RLIMIT_MEMLOCK.
    let inf = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &inf) } != 0 {
        bail!("setrlimit(MEMLOCK): {}", std::io::Error::last_os_error());
    }

    let ifindex = {
        let c = CString::new(ifname.as_str()).unwrap();
        let idx = unsafe { libc::if_nametoindex(c.as_ptr()) };
        if idx == 0 {
            bail!("interface {ifname:?} not found");
        }
        idx
    };

    // Load the BPF object (creates the maps and validates the program).
    let mut open_obj = MaybeUninit::uninit();
    let open = RustssiSkelBuilder::default()
        .open(&mut open_obj)
        .context("open skeleton")?;
    let skel = open.load().context("load skeleton (verifier)")?;

    // Track the server endpoint. Key layout is network byte order to match the
    // packet bytes the BPF program reads.
    let mut key = [0u8; 6];
    key[0..4].copy_from_slice(&ip.octets());
    key[4..6].copy_from_slice(&port.to_be_bytes());
    skel.maps
        .config_map
        .update(&key, &[1u8], MapFlags::ANY)
        .context("insert endpoint into config_map")?;

    // Open the AF_XDP socket and register its fd for this RX queue *before*
    // attaching the program, so the first redirected packet has a target.
    let mut sock = xsk::XskSocket::new(ifindex, QUEUE_ID, FRAME_SIZE, FRAME_COUNT, FILL_SIZE, RX_SIZE)
        .context("create AF_XDP socket")?;
    skel.maps
        .xsks_map
        .update(
            &QUEUE_ID.to_ne_bytes(),
            &(sock.fd() as u32).to_ne_bytes(),
            MapFlags::ANY,
        )
        .context("insert socket fd into xsks_map")?;

    // Attach in generic/SKB mode (veth has no native/zero-copy AF_XDP path).
    let xdp = Xdp::new(skel.progs.xdp_redirect_irc.as_fd());
    xdp.attach(ifindex as i32, XdpFlags::SKB_MODE)
        .context("attach xdp program")?;

    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
    }

    println!(
        "rustssi phase1: iface={ifname} (ifindex={ifindex}) queue={QUEUE_ID} tracking {ip}:{port}"
    );
    println!("waiting for redirected frames (Ctrl-C to stop)...");

    let mut total = 0usize;
    while RUNNING.load(Ordering::SeqCst) {
        let n = sock.poll_rx(std::time::Duration::from_millis(500), |frame| {
            total += 1;
            print_frame(total, frame);
        })?;
        let _ = n;
    }

    xdp.detach(ifindex as i32, XdpFlags::SKB_MODE)
        .context("detach xdp program")?;
    println!("\nstopped. {total} frame(s) redirected into UMEM.");
    Ok(())
}

fn print_frame(n: usize, frame: &[u8]) {
    if frame.len() < 34 || u16::from_be_bytes([frame[12], frame[13]]) != 0x0800 {
        println!("#{n} non-IPv4 frame len={}", frame.len());
        return;
    }
    let ihl = (frame[14] & 0x0f) as usize * 4;
    let proto = frame[23];
    let src = Ipv4Addr::new(frame[26], frame[27], frame[28], frame[29]);
    let dst = Ipv4Addr::new(frame[30], frame[31], frame[32], frame[33]);
    let l4 = 14 + ihl;
    let (sp, dp) = if frame.len() >= l4 + 4 {
        (
            u16::from_be_bytes([frame[l4], frame[l4 + 1]]),
            u16::from_be_bytes([frame[l4 + 2], frame[l4 + 3]]),
        )
    } else {
        (0, 0)
    };
    let pname = match proto {
        6 => "TCP",
        17 => "UDP",
        other => return println!("#{n} ip proto={other} {src} -> {dst} len={}", frame.len()),
    };
    println!("#{n} MATCH {pname} {src}:{sp} -> {dst}:{dp} len={}", frame.len());
}
