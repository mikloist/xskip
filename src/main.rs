//! Phase 4: two-thread client.
//!
//! A pinned data-plane thread (AF_XDP + smoltcp + IRC parsing) and a ratatui UI
//! thread, joined only by two lock-free SPSC rings — decoded lines up, user
//! commands down. No async runtime.

mod irc;
mod net;
mod ui;
mod xsk;

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use rtrb::RingBuffer;

const CHANNEL: &str = "#test";
const RING_CAP: usize = 1024;

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let usage = "usage: rustssi <ifname> <our-ipv4> <server-ipv4> <port> <server-mac> [nick] [channel]";
    let ifname = a.next().context(usage)?;
    let our_ip: Ipv4Addr = a.next().context(usage)?.parse().context("bad our-ipv4")?;
    let server_ip: Ipv4Addr = a.next().context(usage)?.parse().context("bad server-ipv4")?;
    let port: u16 = a.next().context(usage)?.parse().context("bad port")?;
    let server_mac = net::parse_mac(&a.next().context(usage)?)?;
    let nick = a.next().unwrap_or_else(|| "rustssi".to_string());
    let channel = a.next().unwrap_or_else(|| CHANNEL.to_string());

    let cfg = net::NetConfig {
        ifname,
        our_ip,
        server_ip,
        port,
        server_mac,
        nick: nick.clone(),
        channel: channel.clone(),
    };

    // Two SPSC rings: net -> ui (display lines), ui -> net (commands).
    let (net_to_ui_tx, net_to_ui_rx) = RingBuffer::<String>::new(RING_CAP);
    let (ui_to_net_tx, ui_to_net_rx) = RingBuffer::<String>::new(RING_CAP);

    let running = Arc::new(AtomicBool::new(true));

    let net_running = running.clone();
    let net_thread = thread::Builder::new()
        .name("dataplane".into())
        .spawn(move || net::run(cfg, net_to_ui_tx, ui_to_net_rx, net_running))
        .context("spawn data-plane thread")?;

    // UI runs on the main thread (ratatui prefers it).
    let ui_res = ui::run(ui_to_net_tx, net_to_ui_rx, running.clone(), nick, channel);
    running.store(false, Ordering::SeqCst);

    let net_res = net_thread.join().expect("data-plane thread panicked");

    ui_res?;
    if let Err(e) = net_res {
        eprintln!("data-plane error: {e:#}");
    }
    Ok(())
}
