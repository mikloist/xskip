//! Hand-rolled AF_XDP socket: UMEM, the four rings, bind — plus a smoltcp
//! `Device` over the rings. No libxdp.
//!
//! UMEM frame partition: frames `[0, fill_size)` back the RX/FILL path, frames
//! `[fill_size, frame_count)` are the TX free pool. Kept disjoint so RX and TX
//! never contend for a frame.
//!
//! The `Device` runs at `Medium::Ip`: smoltcp hands us bare IP packets and we
//! add/strip the 14-byte Ethernet header ourselves (no ARP — the peer MAC is
//! configured).

#![allow(non_camel_case_types)]

use std::cell::RefCell;
use std::io;
use std::os::fd::RawFd;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use smoltcp::phy::{self, Checksum, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

const AF_XDP: libc::c_int = 44;
const SOL_XDP: libc::c_int = 283;

// setsockopt / getsockopt names (SOL_XDP)
const XDP_MMAP_OFFSETS: libc::c_int = 1;
const XDP_RX_RING: libc::c_int = 2;
const XDP_TX_RING: libc::c_int = 3;
const XDP_UMEM_REG: libc::c_int = 4;
const XDP_UMEM_FILL_RING: libc::c_int = 5;
const XDP_UMEM_COMPLETION_RING: libc::c_int = 6;

// mmap() page offsets identifying each ring.
const XDP_PGOFF_RX_RING: libc::off_t = 0;
const XDP_PGOFF_TX_RING: libc::off_t = 0x8000_0000;
const XDP_UMEM_PGOFF_FILL_RING: libc::off_t = 0x1_0000_0000;
const XDP_UMEM_PGOFF_COMPLETION_RING: libc::off_t = 0x1_8000_0000;

// bind() sxdp_flags
const XDP_COPY: u16 = 1 << 1;
const XDP_USE_NEED_WAKEUP: u16 = 1 << 3;

// ring flags field
const XDP_RING_NEED_WAKEUP: u32 = 1 << 0;

/// Ethernet header length we prepend/strip for the IP-medium device.
const ETH_HDR_LEN: usize = 14;

#[repr(C)]
struct xdp_umem_reg {
    addr: u64,
    len: u64,
    chunk_size: u32,
    headroom: u32,
    flags: u32,
    tx_metadata_len: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct xdp_ring_offset {
    producer: u64,
    consumer: u64,
    desc: u64,
    flags: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct xdp_mmap_offsets {
    rx: xdp_ring_offset,
    tx: xdp_ring_offset,
    fr: xdp_ring_offset,
    cr: xdp_ring_offset,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct xdp_desc {
    addr: u64,
    len: u32,
    options: u32,
}

#[repr(C)]
struct sockaddr_xdp {
    sxdp_family: u16,
    sxdp_flags: u16,
    sxdp_ifindex: u32,
    sxdp_queue_id: u32,
    sxdp_shared_umem_fd: u32,
}

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

pub struct XskSocket {
    fd: RawFd,
    umem: *mut u8,
    umem_len: usize,
    frame_size: u32,
    fill: Ring,
    comp: Ring,
    rx: Ring,
    tx: Ring,
    tx_free: Vec<u64>,
    huge: bool,
}

impl XskSocket {
    /// Create a copy-mode AF_XDP socket on `ifindex`/`queue_id` with its own UMEM.
    pub fn new(
        ifindex: u32,
        queue_id: u32,
        frame_size: u32,
        frame_count: u32,
        fill_size: u32,
        rx_size: u32,
        tx_size: u32,
    ) -> io::Result<XskSocket> {
        unsafe {
            let fd = libc::socket(AF_XDP, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            let umem_len = frame_size as usize * frame_count as usize;
            let (umem, huge) = match map_umem(umem_len) {
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
            setsockopt(fd, XDP_UMEM_REG, &reg)?;
            setsockopt(fd, XDP_UMEM_FILL_RING, &fill_size)?;
            setsockopt(fd, XDP_UMEM_COMPLETION_RING, &tx_size)?;
            setsockopt(fd, XDP_RX_RING, &rx_size)?;
            setsockopt(fd, XDP_TX_RING, &tx_size)?;

            let mut off = xdp_mmap_offsets::default();
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
                fill_size,
                std::mem::size_of::<u64>(),
                XDP_UMEM_PGOFF_FILL_RING,
            )?;
            let comp = Ring::map(
                fd,
                &off.cr,
                tx_size,
                std::mem::size_of::<u64>(),
                XDP_UMEM_PGOFF_COMPLETION_RING,
            )?;
            let rx = Ring::map(
                fd,
                &off.rx,
                rx_size,
                std::mem::size_of::<xdp_desc>(),
                XDP_PGOFF_RX_RING,
            )?;
            let tx = Ring::map(
                fd,
                &off.tx,
                tx_size,
                std::mem::size_of::<xdp_desc>(),
                XDP_PGOFF_TX_RING,
            )?;

            // TX free pool = the upper frames, disjoint from the RX/FILL frames.
            let mut tx_free = Vec::with_capacity((frame_count - fill_size) as usize);
            for i in fill_size..frame_count {
                tx_free.push(i as u64 * frame_size as u64);
            }

            let mut sock = XskSocket {
                fd,
                umem,
                umem_len,
                frame_size,
                fill,
                comp,
                rx,
                tx,
                tx_free,
                huge,
            };

            // Hand the kernel one frame per FILL slot to receive into.
            sock.fill_frames(fill_size);

            let sxdp = sockaddr_xdp {
                sxdp_family: AF_XDP as u16,
                sxdp_flags: XDP_COPY | XDP_USE_NEED_WAKEUP,
                sxdp_ifindex: ifindex,
                sxdp_queue_id: queue_id,
                sxdp_shared_umem_fd: 0,
            };
            if libc::bind(
                fd,
                &sxdp as *const _ as *const libc::sockaddr,
                std::mem::size_of::<sockaddr_xdp>() as libc::socklen_t,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }

            Ok(sock)
        }
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    fn umem_ptr(&self) -> *mut u8 {
        self.umem
    }

    /// Whether the UMEM is backed by 2 MiB hugepages.
    pub fn hugepages(&self) -> bool {
        self.huge
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

    /// Queue a frame `[addr, addr+len)` on the TX ring and kick the driver.
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
        self.kick_tx();
    }

    fn kick_tx(&mut self) {
        if self.tx.needs_wakeup() {
            unsafe {
                libc::sendto(self.fd, ptr::null(), 0, libc::MSG_DONTWAIT, ptr::null(), 0);
            }
        }
    }

    /// Nudge the driver to service the FILL ring if it has gone idle.
    pub fn kick_rx(&mut self) {
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

impl Drop for XskSocket {
    fn drop(&mut self) {
        unsafe {
            if !self.umem.is_null() {
                libc::munmap(self.umem as *mut libc::c_void, self.umem_len);
            }
            libc::close(self.fd);
        }
    }
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

/// Map the UMEM region, preferring 2 MiB hugepages (fewer TLB entries for the
/// hot packet path). Falls back to normal pages if the hugetlb pool is empty or
/// the length isn't a hugepage multiple. Returns `(ptr, using_hugepages)`.
unsafe fn map_umem(len: usize) -> io::Result<(*mut u8, bool)> {
    const HUGE_2MB: usize = 2 * 1024 * 1024;
    let prot = libc::PROT_READ | libc::PROT_WRITE;

    if len % HUGE_2MB == 0 {
        let p = libc::mmap(
            ptr::null_mut(),
            len,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB | libc::MAP_HUGE_2MB,
            -1,
            0,
        );
        if p != libc::MAP_FAILED {
            return Ok((p as *mut u8, true));
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
    Ok((p as *mut u8, false))
}

pub struct XskDevice {
    sock: Rc<RefCell<XskSocket>>,
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    mtu: usize,
}

impl XskDevice {
    pub fn new(
        sock: Rc<RefCell<XskSocket>>,
        src_mac: [u8; 6],
        dst_mac: [u8; 6],
        mtu: usize,
    ) -> Self {
        XskDevice {
            sock,
            src_mac,
            dst_mac,
            mtu,
        }
    }
}

impl Device for XskDevice {
    type RxToken<'a> = XskRxToken;
    type TxToken<'a> = XskTxToken;

    fn receive(&mut self, _t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (addr, len, umem) = {
            let mut s = self.sock.borrow_mut();
            s.kick_rx();
            let (addr, len) = s.rx_peek()?;
            (addr, len, s.umem_ptr())
        };
        Some((
            XskRxToken {
                sock: self.sock.clone(),
                umem,
                addr,
                len,
            },
            XskTxToken {
                sock: self.sock.clone(),
                umem,
                src_mac: self.src_mac,
                dst_mac: self.dst_mac,
            },
        ))
    }

    fn transmit(&mut self, _t: Instant) -> Option<Self::TxToken<'_>> {
        let umem = self.sock.borrow().umem_ptr();
        Some(XskTxToken {
            sock: self.sock.clone(),
            umem,
            src_mac: self.src_mac,
            dst_mac: self.dst_mac,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = self.mtu;
        // Compute checksums on TX so the peer accepts our packets, but do NOT
        // verify on RX: over veth the kernel offloads TX checksums, so inbound
        // frames captured via AF_XDP carry uncomputed/partial TCP checksums.
        c.checksum.ipv4 = Checksum::Tx;
        c.checksum.tcp = Checksum::Tx;
        c.checksum.icmpv4 = Checksum::Tx;
        c
    }
}

pub struct XskRxToken {
    sock: Rc<RefCell<XskSocket>>,
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
        self.sock.borrow_mut().recycle(self.addr);
        r
    }
}

pub struct XskTxToken {
    sock: Rc<RefCell<XskSocket>>,
    umem: *mut u8,
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
}

impl phy::TxToken for XskTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut s = self.sock.borrow_mut();
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
