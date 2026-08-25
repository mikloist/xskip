//! Hand-rolled AF_XDP socket: UMEM, the four rings, bind. No libxdp.
//!
//! Phase 1 uses only the RX path (plus FILL to hand the kernel empty frames,
//! and COMPLETION which the UMEM registration expects). TX arrives in phase 2.

#![allow(non_camel_case_types)]

use std::io;
use std::os::fd::RawFd;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

const AF_XDP: libc::c_int = 44;
const SOL_XDP: libc::c_int = 283;

// setsockopt / getsockopt names (SOL_XDP)
const XDP_MMAP_OFFSETS: libc::c_int = 1;
const XDP_RX_RING: libc::c_int = 2;
const XDP_UMEM_REG: libc::c_int = 4;
const XDP_UMEM_FILL_RING: libc::c_int = 5;
const XDP_UMEM_COMPLETION_RING: libc::c_int = 6;

// mmap() page offsets identifying each ring.
const XDP_PGOFF_RX_RING: libc::off_t = 0;
const XDP_UMEM_PGOFF_FILL_RING: libc::off_t = 0x1_0000_0000;
const XDP_UMEM_PGOFF_COMPLETION_RING: libc::off_t = 0x1_8000_0000;

// bind() sxdp_flags
const XDP_COPY: u16 = 1 << 1;
const XDP_USE_NEED_WAKEUP: u16 = 1 << 3;

// ring flags field
const XDP_RING_NEED_WAKEUP: u32 = 1 << 0;

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
    cached: u32, // our side's cached index (producer for FILL, consumer for RX)
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
        unsafe { AtomicU32::from_ptr(self.flags).load(Ordering::Acquire) & XDP_RING_NEED_WAKEUP != 0 }
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
    _comp: Ring,
    rx: Ring,
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
    ) -> io::Result<XskSocket> {
        unsafe {
            let fd = libc::socket(AF_XDP, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            let umem_len = frame_size as usize * frame_count as usize;
            let umem = libc::mmap(
                ptr::null_mut(),
                umem_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if umem == libc::MAP_FAILED {
                let e = io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            let umem = umem as *mut u8;

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
            setsockopt(fd, XDP_UMEM_COMPLETION_RING, &fill_size)?;
            setsockopt(fd, XDP_RX_RING, &rx_size)?;

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
                fill_size,
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

            let mut sock = XskSocket {
                fd,
                umem,
                umem_len,
                frame_size,
                fill,
                _comp: comp,
                rx,
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
                let e = io::Error::last_os_error();
                return Err(e);
            }

            Ok(sock)
        }
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// Produce up to `n` UMEM frame addresses into the FILL ring.
    fn fill_frames(&mut self, n: u32) {
        let cons = self.fill.consumer().load(Ordering::Acquire);
        let free = self.fill.size - (self.fill.cached.wrapping_sub(cons));
        let n = n.min(free);
        for _ in 0..n {
            let idx = (self.fill.cached & self.fill.mask) as usize;
            let addr =
                (self.fill.cached as u64 * self.frame_size as u64) % self.umem_len as u64;
            unsafe { *(self.fill.desc as *mut u64).add(idx) = addr };
            self.fill.cached = self.fill.cached.wrapping_add(1);
        }
        if n > 0 {
            self.fill
                .producer()
                .store(self.fill.cached, Ordering::Release);
        }
    }

    /// Recycle a single frame address back to the kernel via the FILL ring.
    fn recycle(&mut self, addr: u64) {
        let base = addr & !(self.frame_size as u64 - 1);
        let idx = (self.fill.cached & self.fill.mask) as usize;
        unsafe { *(self.fill.desc as *mut u64).add(idx) = base };
        self.fill.cached = self.fill.cached.wrapping_add(1);
        self.fill
            .producer()
            .store(self.fill.cached, Ordering::Release);
    }

    /// Block up to `timeout` for RX, then drain every ready frame through `f`.
    /// `Duration::ZERO` polls without blocking (busy-poll). Returns the number
    /// of frames delivered.
    pub fn poll_rx<F: FnMut(&[u8])>(
        &mut self,
        timeout: std::time::Duration,
        mut f: F,
    ) -> io::Result<usize> {
        if self.fill.needs_wakeup() {
            unsafe {
                libc::recvfrom(self.fd, ptr::null_mut(), 0, libc::MSG_DONTWAIT, ptr::null_mut(), ptr::null_mut());
            }
        }

        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // poll(2) takes a millisecond c_int; saturate rather than wrap.
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(0);
            }
            return Err(e);
        }

        let prod = self.rx.producer().load(Ordering::Acquire);
        let mut cons = self.rx.consumer().load(Ordering::Acquire);
        let mut count = 0usize;
        while cons != prod {
            let idx = (cons & self.rx.mask) as usize;
            let desc = unsafe { *(self.rx.desc as *const xdp_desc).add(idx) };
            let start = desc.addr as usize;
            let frame =
                unsafe { std::slice::from_raw_parts(self.umem.add(start), desc.len as usize) };
            f(frame);
            self.recycle(desc.addr);
            cons = cons.wrapping_add(1);
            count += 1;
        }
        if count > 0 {
            self.rx.consumer().store(cons, Ordering::Release);
        }
        Ok(count)
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
