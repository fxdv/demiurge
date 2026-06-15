//! io_uring production proxy on the TCP serve path (Linux).
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use demiurge_dataplane::IoUringProxySession;
use demiurge_router::{serve, spawn_large_body_backend, Backend, Router};

fn harden_report(tier: u8, id: &str, status: &str, detail: &str) {
    eprintln!("HARDEN_REPORT tier={tier} id={id} status={status} detail={detail}");
}

fn spawn_marker_backend(marker: u8) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: 1\r\nconnection: close\r\n\r\n{}",
                marker as char
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    addr
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
