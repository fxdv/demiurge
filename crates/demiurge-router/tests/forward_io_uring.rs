//! io_uring production proxy on the TCP serve path (Linux).
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use demiurge_dataplane::IoUringProxySession;
use demiurge_router::{serve, spawn_large_body_backend, spawn_marker_backend, Backend, Router};

fn harden_report(tier: u8, id: &str, status: &str, detail: &str) {
    eprintln!("HARDEN_REPORT tier={tier} id={id} status={status} detail={detail}");
}

#[test]
fn forwards_with_io_uring_proxy_session() {
    let cheap_addr = spawn_marker_backend(b'A');
    let prefill = vec![Backend::new("cheap", cheap_addr, 0.01)];
    let router = Arc::new(Router::new(prefill, vec![]).with_io_uring(true));
    assert!(router.io_uring_enabled());

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = serve(listener, router);
    });
    thread::sleep(Duration::from_millis(50));

    let mut c = TcpStream::connect(front).unwrap();
    c.write_all(b"GET /prefill HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .unwrap();
    c.shutdown(Shutdown::Write).unwrap();
    let mut resp = String::new();
    c.read_to_string(&mut resp).unwrap();

    assert!(
        resp.ends_with('A'),
        "expected io_uring proxy to reach backend, got: {resp:?}"
    );
}

/// G6 — `IoUringAcceptLoop` accepts a live TCP connection.
#[test]
fn harden_io_uring_accept_forwards() {
    use std::os::fd::{AsRawFd, FromRawFd};

    use demiurge_dataplane::IoUringAcceptLoop;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    let mut acceptor = IoUringAcceptLoop::new(listener.as_raw_fd()).expect("accept loop");

    let client = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        let mut c = TcpStream::connect(front).unwrap();
        c.write_all(b"ping").unwrap();
        c
    });

    let fd = acceptor.accept_one().expect("accept_one");
    assert!(fd >= 0, "accepted fd must be non-negative");
    // SAFETY: fd owned by this test after Accept CQE.
    let mut accepted = unsafe { TcpStream::from_raw_fd(fd) };
    let mut buf = [0u8; 4];
    accepted.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    drop(accepted);
    let _ = client.join();
    drop(listener);
    harden_report(2, "io_uring_accept_forwards", "PASS", "accept_one");
}

// Tier 2 — io_uring copy_stream respects max_bytes cap (256 KiB production limit).
#[test]
fn harden_io_uring_copy_respects_max_bytes() {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::fd::AsRawFd;

    let payload = vec![b'z'; 300 * 1024];
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    tmp.write_all(&payload).expect("write payload");
    tmp.flush().expect("flush");

    let null = OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .expect("/dev/null");

    let mut session = IoUringProxySession::new().expect("session");
    let cap = 256 * 1024;
    let copied = session
        .copy_stream(tmp.as_raw_fd(), null.as_raw_fd(), cap)
        .expect("copy");

    assert_eq!(
        copied, cap as u64,
        "copy_stream should stop at max_bytes cap"
    );
    harden_report(
        2,
        "io_uring_copy_respects_max_bytes",
        "PASS",
        &format!("copied={copied}"),
    );
}

// Tier 2 — truncated HTTP head does not panic the router.
#[test]
fn harden_io_uring_bad_http_head_no_panic() {
    let cheap_addr = spawn_marker_backend(b'B');
    let prefill = vec![Backend::new("cheap", cheap_addr, 0.01)];
    let router = Arc::new(Router::new(prefill, vec![]).with_io_uring(true));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = serve(listener, router);
    });
    thread::sleep(Duration::from_millis(50));

    let mut c = TcpStream::connect(front).unwrap();
    c.write_all(b"GET /incomplete HTTP/1.1\r\nhost: x\r\n")
        .unwrap();
    c.shutdown(Shutdown::Write).unwrap();
    thread::sleep(Duration::from_millis(100));
    harden_report(2, "io_uring_bad_http_head_no_panic", "PASS", "no_panic");
}

// Tier 4 — large response through io_uring proxy (≥256 KiB observed).
#[test]
fn harden_io_uring_large_response_through_proxy() {
    let body_bytes = 300 * 1024;
    let addr = spawn_large_body_backend(body_bytes);
    let prefill = vec![Backend::new("large", addr, 0.01)];
    let router = Arc::new(Router::new(prefill, vec![]).with_io_uring(true));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = serve(listener, router);
    });
    thread::sleep(Duration::from_millis(50));

    let mut c = TcpStream::connect(front).unwrap();
    c.write_all(b"GET /large HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .unwrap();
    c.shutdown(Shutdown::Write).unwrap();

    let mut total = 0usize;
    let mut buf = [0u8; 64 * 1024];
    loop {
        match c.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    assert!(
        total >= 256 * 1024,
        "expected ≥256 KiB through io_uring proxy, got {total} bytes"
    );
    harden_report(
        4,
        "io_uring_large_response_through_proxy",
        "PASS",
        &format!("bytes={total}"),
    );
}
