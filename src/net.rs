//! Pinned data-plane thread: AF_XDP + smoltcp + IRC parsing.
//!
//! Owns everything from the wire up through parsing. Decoded messages are
//! turned into owned display `String`s and pushed to the UI over an SPSC ring
//! (the one boundary copy); user commands flow back over a second SPSC ring and
//! are written into smoltcp's TX. No terminal I/O happens here — the UI owns
//! the screen — so status is reported as ring messages, never `println!`.

use std::cell::RefCell;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::net::Ipv4Addr;
use std::os::fd::AsFd;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use libbpf_rs::skel::{OpenSkel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags, Xdp, XdpFlags};
use rtrb::{Consumer, Producer};
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint};

use crate::irc;

mod skel {
    include!(concat!(env!("OUT_DIR"), "/rustssi.skel.rs"));
}
use skel::*;

const FRAME_SIZE: u32 = 4096;
const FRAME_COUNT: u32 = 4096;
const FILL_SIZE: u32 = 2048;
const RX_SIZE: u32 = 2048;
const TX_SIZE: u32 = 2048;
const QUEUE_ID: u32 = 0;
const LOCAL_PORT: u16 = 49321;
const MTU: usize = 1500;
/// CPU the data-plane thread pins itself to.
const DATAPLANE_CPU: usize = 2;

pub struct NetConfig {
    pub ifname: String,
    pub our_ip: Ipv4Addr,
    pub server_ip: Ipv4Addr,
    pub port: u16,
    pub server_mac: [u8; 6],
    pub nick: String,
    pub channel: String,
}

/// Entry point for the data-plane thread. Reports errors to the UI ring and
/// clears `running` on exit so the UI tears down too.
pub fn run(
    cfg: NetConfig,
    mut to_ui: Producer<String>,
    mut from_ui: Consumer<String>,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let r = run_inner(&cfg, &mut to_ui, &mut from_ui, &running);
    if let Err(e) = &r {
        let _ = to_ui.push(format!("-- data-plane error: {e:#} --"));
    }
    running.store(false, Ordering::SeqCst);
    r
}

fn run_inner(
    cfg: &NetConfig,
    to_ui: &mut Producer<String>,
    from_ui: &mut Consumer<String>,
    running: &AtomicBool,
) -> Result<()> {
    pin_cpu(DATAPLANE_CPU, to_ui);

    // Best-effort: raising RLIMIT_MEMLOCK needs CAP_SYS_RESOURCE, which the
    // setcap-based unprivileged path deliberately omits. Kernel 5.11+ accounts
    // BPF memory against memcg, so a failure here is fine to ignore.
    let inf = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &inf) } != 0 {
        let _ = to_ui.push(format!(
            "-- note: could not raise RLIMIT_MEMLOCK ({}); continuing --",
            std::io::Error::last_os_error()
        ));
    }

    let ifindex = {
        let c = CString::new(cfg.ifname.as_str()).unwrap();
        let idx = unsafe { libc::if_nametoindex(c.as_ptr()) };
        if idx == 0 {
            bail!("interface {:?} not found", cfg.ifname);
        }
        idx
    };
    let our_mac = read_mac(&cfg.ifname)?;

    let mut open_obj = MaybeUninit::uninit();
    let open = RustssiSkelBuilder::default()
        .open(&mut open_obj)
        .context("open skeleton")?;
    let skel = open.load().context("load skeleton (verifier)")?;

    let mut key = [0u8; 6];
    key[0..4].copy_from_slice(&cfg.server_ip.octets());
    key[4..6].copy_from_slice(&cfg.port.to_be_bytes());
    skel.maps
        .config_map
        .update(&key, &[1u8], MapFlags::ANY)
        .context("insert endpoint into config_map")?;

    let sock = crate::xsk::XskSocket::new(
        ifindex, QUEUE_ID, FRAME_SIZE, FRAME_COUNT, FILL_SIZE, RX_SIZE, TX_SIZE,
    )
    .context("create AF_XDP socket")?;
    let fd = sock.fd();
    let _ = to_ui.push(if sock.hugepages() {
        "-- UMEM: 16 MiB on 2 MiB hugepages --".to_string()
    } else {
        "-- UMEM: 16 MiB on 4 KiB pages (hugetlb pool empty; sysctl vm.nr_hugepages) --".to_string()
    });
    skel.maps
        .xsks_map
        .update(&QUEUE_ID.to_ne_bytes(), &(fd as u32).to_ne_bytes(), MapFlags::ANY)
        .context("insert socket fd into xsks_map")?;

    let xdp = Xdp::new(skel.progs.xdp_redirect_irc.as_fd());
    xdp.attach(ifindex as i32, XdpFlags::SKB_MODE)
        .context("attach xdp program")?;

    let sock = Rc::new(RefCell::new(sock));
    let mut device = crate::xsk::XskDevice::new(sock.clone(), our_mac, cfg.server_mac, MTU);

    let seed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    let mut config = Config::new(HardwareAddress::Ip);
    config.random_seed = seed;
    let mut iface = Interface::new(config, &mut device, Instant::now());
    iface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(IpAddress::from(cfg.our_ip), 24)).unwrap();
    });

    let tcp_socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0u8; 65535]),
        tcp::SocketBuffer::new(vec![0u8; 65535]),
    );
    let mut sockets = SocketSet::new(Vec::new());
    let handle = sockets.add(tcp_socket);
    {
        let s = sockets.get_mut::<tcp::Socket>(handle);
        let remote = IpEndpoint::new(IpAddress::from(cfg.server_ip), cfg.port);
        s.connect(iface.context(), remote, LOCAL_PORT).context("tcp connect")?;
    }
    let _ = to_ui.push(format!(
        "-- connecting {} -> {}:{} as {} --",
        cfg.our_ip, cfg.server_ip, cfg.port, cfg.nick
    ));

    let mut sent_registration = false;
    let mut registered = false;
    let mut joined = false;
    let mut rx_acc: Vec<u8> = Vec::new();

    while running.load(Ordering::SeqCst) {
        iface.poll(Instant::now(), &mut device, &mut sockets);
        let s = sockets.get_mut::<tcp::Socket>(handle);

        if s.may_send() && !sent_registration {
            let reg = format!("NICK {nick}\r\nUSER {nick} 0 * :{nick}\r\n", nick = cfg.nick);
            s.send_slice(reg.as_bytes()).ok();
            sent_registration = true;
        }

        if s.may_recv() {
            let _ = s.recv(|data| {
                rx_acc.extend_from_slice(data);
                (data.len(), ())
            });
        }

        let mut replies: Vec<Vec<u8>> = Vec::new();
        while let Some(pos) = rx_acc.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = rx_acc.drain(..=pos).collect();
            let raw = trim_crlf(&line);
            let Some(msg) = irc::Message::parse(raw) else {
                continue;
            };
            let _ = to_ui.push(display_line(&msg));
            if msg.is(b"PING") {
                let mut r = b"PONG".to_vec();
                if let Some(tok) = msg.last_param() {
                    r.extend_from_slice(b" :");
                    r.extend_from_slice(tok);
                }
                replies.push(r);
            } else if msg.command() == b"001" && !registered {
                registered = true;
                let _ = to_ui.push("-- registered --".to_string());
            }
        }

        // Auto-join once registered.
        if registered && !joined {
            let s = sockets.get_mut::<tcp::Socket>(handle);
            if s.may_send() {
                s.send_slice(format!("JOIN {}\r\n", cfg.channel).as_bytes()).ok();
                joined = true;
            }
        }

        // User commands from the UI: full IRC lines, we add CRLF.
        while let Ok(cmd) = from_ui.pop() {
            let s = sockets.get_mut::<tcp::Socket>(handle);
            if s.may_send() {
                let mut line = cmd.into_bytes();
                line.extend_from_slice(b"\r\n");
                s.send_slice(&line).ok();
            }
        }

        for mut r in replies {
            let s = sockets.get_mut::<tcp::Socket>(handle);
            if s.may_send() {
                r.extend_from_slice(b"\r\n");
                s.send_slice(&r).ok();
            }
        }

        sock.borrow_mut().kick_rx();
        let delay_ms = iface
            .poll_delay(Instant::now(), &sockets)
            .map(|d| d.total_millis() as libc::c_int)
            .unwrap_or(50)
            .clamp(0, 50);
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut pfd, 1, delay_ms) };
    }

    xdp.detach(ifindex as i32, XdpFlags::SKB_MODE)
        .context("detach xdp program")?;
    Ok(())
}

/// Pin the current thread to `cpu`. Best-effort: a failure is reported but not fatal.
fn pin_cpu(cpu: usize, to_ui: &mut Producer<String>) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        let r = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if r == 0 {
            let _ = to_ui.push(format!("-- data-plane pinned to CPU {cpu} --"));
        } else {
            let _ = to_ui.push(format!("-- CPU pin failed (continuing): {} --", std::io::Error::last_os_error()));
        }
    }
}

/// Format a parsed message into an owned display line (the boundary copy).
fn display_line(m: &irc::Message) -> String {
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if m.is(b"PRIVMSG") || m.is(b"NOTICE") {
        let from = m.prefix().map(nick_of).unwrap_or_default();
        let target = m.params().first().map(|b| text(b)).unwrap_or_default();
        let body = m.last_param().map(|b| text(b)).unwrap_or_default();
        let tag = if m.is(b"NOTICE") { "notice " } else { "" };
        format!("{tag}[{target}] <{from}> {body}")
    } else if m.is(b"JOIN") {
        let who = m.prefix().map(nick_of).unwrap_or_default();
        let chan = m.last_param().map(|b| text(b)).unwrap_or_default();
        format!("* {who} joined {chan}")
    } else {
        let params: Vec<String> = m.params().iter().map(|b| text(b)).collect();
        format!("{} {}", text(m.command()), params.join(" "))
    }
}

fn nick_of(prefix: &[u8]) -> String {
    let s = String::from_utf8_lossy(prefix);
    s.split(['!', '@']).next().unwrap_or("").to_string()
}

fn trim_crlf(mut s: &[u8]) -> &[u8] {
    while matches!(s.last(), Some(b'\r' | b'\n')) {
        s = &s[..s.len() - 1];
    }
    s
}

pub fn read_mac(ifname: &str) -> Result<[u8; 6]> {
    let s = std::fs::read_to_string(format!("/sys/class/net/{ifname}/address"))
        .with_context(|| format!("read MAC of {ifname}"))?;
    parse_mac(s.trim())
}

pub fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        bail!("bad MAC {s:?}");
    }
    let mut m = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        m[i] = u8::from_str_radix(p, 16).context("bad MAC hex")?;
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::{display_line, parse_mac};
    use crate::irc::Message;

    #[test]
    fn formats_privmsg_as_chat() {
        let m = Message::parse(b":alice!u@h PRIVMSG #test :hi all").unwrap();
        assert_eq!(display_line(&m), "[#test] <alice> hi all");
    }

    #[test]
    fn formats_join() {
        let m = Message::parse(b":bob!u@h JOIN #test").unwrap();
        assert_eq!(display_line(&m), "* bob joined #test");
    }

    #[test]
    fn formats_numeric_generically() {
        let m = Message::parse(b":srv 366 me #test :End of NAMES list").unwrap();
        assert_eq!(display_line(&m), "366 me #test End of NAMES list");
    }

    #[test]
    fn parse_mac_roundtrip() {
        assert_eq!(parse_mac("ca:c5:b2:64:89:9e").unwrap(), [0xca, 0xc5, 0xb2, 0x64, 0x89, 0x9e]);
        assert!(parse_mac("zz:zz").is_err());
    }
}
