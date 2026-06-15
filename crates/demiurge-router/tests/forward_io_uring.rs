//! io_uring production proxy on the TCP serve path (Linux).
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use demiurge_router::{serve, Backend, Router};

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
