//! Reusable backend stubs for integration tests.
//!
//! Each helper spawns a background TCP listener that mimics a particular
//! behaviour (echo, latch, RST, large body, delay). They are `pub` so that
//! integration tests in the `tests/` directory can import them from the crate
//! root.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Prefill backend that blocks until [`LatchBackend::release`] is called.
///
/// Useful for measuring disaggregated-path latency decoupling.
#[must_use]
pub fn spawn_latch_prefill_backend() -> (SocketAddr, LatchBackend) {
    let latch = Arc::new((Mutex::new(false), Condvar::new()));
    let latch2 = Arc::clone(&latch);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let (lock, cv) = &*latch2;
            let mut started = lock.lock().expect("lock");
            while !*started {
                started = cv.wait(started).expect("wait");
            }
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nx-demiurge-prefill-done: 1\r\nx-demiurge-kv-handle: 1\r\nx-demiurge-kv-bytes: 4096\r\ncontent-length: 0\r\n\r\n",
            );
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    (addr, LatchBackend { latch })
}

/// Handle returned by [`spawn_latch_prefill_backend`].
pub struct LatchBackend {
    latch: Arc<(Mutex<bool>, Condvar)>,
}

impl LatchBackend {
    /// Unblock all waiting prefill connections.
    pub fn release(&self) {
        let (lock, cv) = &*self.latch;
        *lock.lock().expect("lock") = true;
        cv.notify_all();
    }
}

/// Backend that replies with a single-byte body identifying itself by `marker`.
///
/// Used in routing tests to assert which backend was chosen.
#[must_use]
pub fn spawn_marker_backend(marker: u8) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: 1\r\n\r\n{}",
                marker as char
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    addr
}

/// Backend that accepts then immediately resets the connection (fault injection).
#[must_use]
pub fn spawn_rst_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(s) = conn else { continue };
            drop(s);
        }
    });
    addr
}

/// Backend that returns a fixed-size HTTP body.
///
/// Used in io_uring and large-response tests.
#[must_use]
pub fn spawn_large_body_backend(body_bytes: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {body_bytes}\r\nconnection: close\r\n\r\n"
            );
            let _ = s.write_all(head.as_bytes());
            let chunk = vec![b'x'; 64 * 1024];
            let mut sent = 0usize;
            while sent < body_bytes {
                let n = chunk.len().min(body_bytes - sent);
                if s.write_all(&chunk[..n]).is_err() {
                    break;
                }
                sent += n;
            }
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    addr
}

/// Backend that sleeps for `delay` before responding (timing tests).
#[must_use]
pub fn spawn_delay_backend(delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            thread::sleep(delay);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    addr
}
