//! io_uring recv/send helpers for [`super::IoUringForwarder`].

use std::io;
use std::os::fd::RawFd;
use std::ptr;

use io_uring::{opcode, types, IoUring};

use super::IoUringForwarder;

const IOV_MAX: usize = 64 * 1024;

/// io_uring-owned accept loop over a listening TCP fd (threat-model G6).
pub struct IoUringAcceptLoop {
    ring: IoUring,
    listen_fd: RawFd,
}

impl IoUringAcceptLoop {
    pub fn new(listen_fd: RawFd) -> io::Result<Self> {
        Ok(Self {
            ring: IoUring::new(64)?,
            listen_fd,
        })
    }

    /// Block until one connection is accepted; returns the client fd.
    pub fn accept_one(&mut self) -> io::Result<RawFd> {
        let accept =
            opcode::Accept::new(types::Fd(self.listen_fd), ptr::null_mut(), ptr::null_mut())
                .build()
                .user_data(1);
        // SAFETY: null addr/addrlen is valid for accept(2) when peer address
        // is unused; listen_fd remains open for the lifetime of this loop.
        unsafe {
            self.ring
                .submission()
                .push(&accept)
                .map_err(|_| io::Error::other("io_uring submission full"))?;
        }
        self.ring.submit_and_wait(1)?;
        let cqe = self
            .ring
            .completion()
            .next()
            .ok_or_else(|| io::Error::other("missing io_uring accept cqe"))?;
        let fd = cqe.result();
        if fd < 0 {
            return Err(io::Error::from_raw_os_error(-fd));
        }
        Ok(fd)
    }
}

/// Reused io_uring ring for production TCP proxy (one session per connection worker).
pub struct IoUringProxySession {
    ring: IoUring,
    buf: Vec<u8>,
}

impl IoUringProxySession {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            ring: IoUring::new(16)?,
            buf: vec![0u8; IOV_MAX],
        })
    }

    fn submit_read(&mut self, fd: RawFd, len: usize) -> io::Result<()> {
        let len = len.min(self.buf.len()) as u32;
        let read_e = opcode::Read::new(types::Fd(fd), self.buf.as_mut_ptr(), len)
            .build()
            .user_data(1);
        // SAFETY: buffer valid until read completion.
        unsafe {
            self.ring
                .submission()
                .push(&read_e)
                .map_err(|_| io::Error::other("io_uring submission full"))?;
        }
        Ok(())
    }

    fn submit_write(&mut self, fd: RawFd, len: usize) -> io::Result<()> {
        let len = len.min(self.buf.len()) as u32;
        let write_e = opcode::Write::new(types::Fd(fd), self.buf.as_ptr(), len)
            .build()
            .user_data(2);
        unsafe {
            self.ring
                .submission()
                .push(&write_e)
                .map_err(|_| io::Error::other("io_uring submission full"))?;
        }
        Ok(())
    }

    fn wait_cqe(&mut self) -> io::Result<i32> {
        self.ring.submit_and_wait(1)?;
        let cqe = self
            .ring
            .completion()
            .next()
            .ok_or_else(|| io::Error::other("missing io_uring cqe"))?;
        Ok(cqe.result())
    }

    /// Read until `\r\n\r\n` or `max` bytes (production HTTP head recv).
    pub fn read_http_head(&mut self, fd: RawFd, max: usize) -> io::Result<Vec<u8>> {
        let mut acc = Vec::with_capacity(1024.min(max));
        while acc.len() < max {
            self.submit_read(fd, (max - acc.len()).min(self.buf.len()))?;
            let n = self.wait_cqe()?;
            if n <= 0 {
                break;
            }
            acc.extend_from_slice(&self.buf[..n as usize]);
            if acc.ends_with(b"\r\n\r\n") {
                return Ok(acc);
            }
        }
        Ok(acc)
    }

    /// Copy from `read_fd` to `write_fd` up to `max_bytes` using this session's ring.
    pub fn copy_stream(
        &mut self,
        read_fd: RawFd,
        write_fd: RawFd,
        max_bytes: usize,
    ) -> io::Result<u64> {
        let mut total = 0u64;
        while total < max_bytes as u64 {
            let chunk = (max_bytes - total as usize).min(self.buf.len());
            self.submit_read(read_fd, chunk)?;
            let n = self.wait_cqe()?;
            if n <= 0 {
                break;
            }
            let n = n as usize;

            self.submit_write(write_fd, n)?;
            let w = self.wait_cqe()?;
            if w < 0 {
                return Err(io::Error::from_raw_os_error(-w));
            }
            total += n as u64;
        }
        Ok(total)
    }
}

pub fn copy_between(read_fd: RawFd, write_fd: RawFd, max_bytes: usize) -> io::Result<u64> {
    let mut session = IoUringProxySession::new()?;
    session.copy_stream(read_fd, write_fd, max_bytes)
}

pub fn bench_forward_nop(fwd: &IoUringForwarder, ring: &mut IoUring) -> io::Result<()> {
    std::hint::black_box(fwd.forward_decision());
    let nop = opcode::Nop::new().build().user_data(42);
    unsafe {
        ring.submission()
            .push(&nop)
            .map_err(|_| io::Error::other("io_uring submission full"))?;
    }
    ring.submit_and_wait(1)?;
    let _ = ring.completion().next();
    Ok(())
}
