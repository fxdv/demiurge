//! io_uring recv/send helpers for [`super::IoUringForwarder`].

use std::io;
use std::os::fd::RawFd;

use io_uring::{opcode, types, IoUring};

use super::IoUringForwarder;

pub fn copy_between(read_fd: RawFd, write_fd: RawFd, max_bytes: usize) -> io::Result<u64> {
    let mut ring = IoUring::new(8)?;
    let mut buf = vec![0u8; max_bytes.min(64 * 1024)];
    let mut total = 0u64;

    loop {
        let read_e = opcode::Read::new(types::Fd(read_fd), buf.as_mut_ptr(), buf.len() as u32)
            .build()
            .user_data(1);
        // SAFETY: buffer valid until read completion.
        unsafe {
            ring.submission()
                .push(&read_e)
                .map_err(|_| io::Error::other("io_uring submission full"))?;
        }
        ring.submit_and_wait(1)?;

        let cqe = ring
            .completion()
            .next()
            .ok_or_else(|| io::Error::other("missing read cqe"))?;
        let n = cqe.result();
        if n <= 0 {
            break;
        }
        let n = n as usize;

        let write_e = opcode::Write::new(types::Fd(write_fd), buf.as_ptr(), n as u32)
            .build()
            .user_data(2);
        unsafe {
            ring.submission()
                .push(&write_e)
                .map_err(|_| io::Error::other("io_uring submission full"))?;
        }
        ring.submit_and_wait(1)?;
        let wcqe = ring
            .completion()
            .next()
            .ok_or_else(|| io::Error::other("missing write cqe"))?;
        if wcqe.result() < 0 {
            return Err(io::Error::from_raw_os_error(-wcqe.result()));
        }
        total += n as u64;
        if n < buf.len() {
            break;
        }
    }
    Ok(total)
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
