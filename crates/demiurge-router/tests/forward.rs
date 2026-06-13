use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use demiurge_router::{select, serve, Backend, Router};

/// A trivial backend that replies with a fixed one-byte body identifying itself.
fn spawn_marker_backend(marker: u8) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf); // consume the forwarded head
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

// [DEMI-ROUTE-MINCOST] — select returns the cheapest backend, and in-flight
// load shifts the decision: enough load on the cheap one flips the choice.
#[test]
fn selects_min_cost_backend() {
    let cheap = Backend::new("cheap", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dear = Backend::new("dear", "127.0.0.1:2".parse().unwrap(), 1.00);
    let pool = vec![Arc::clone(&cheap), Arc::clone(&dear)];

    // With no load, the lower base service time wins.
    assert_eq!(select(&pool).unwrap().label, "cheap");

    // 200 in-flight * 0.01s base => ~2.0s effective, dearer than dear's 1.0s.
    for _ in 0..200 {
        cheap.incr_inflight();
    }
    assert_eq!(select(&pool).unwrap().label, "dear");

    assert!(select(&[]).is_none());
}

// End-to-end: forwarder proxies to the cheaper backend. [DEMI-ROUTE-MINCOST]
#[test]
fn forwards_to_cheapest_backend() {
    let cheap_addr = spawn_marker_backend(b'A');
    let dear_addr = spawn_marker_backend(b'B');

    let prefill = vec![
        Backend::new("cheap", cheap_addr, 0.01),
        Backend::new("dear", dear_addr, 1.00),
    ];
    let router = Arc::new(Router::new(prefill, vec![]));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = serve(listener, router);
    });
    thread::sleep(Duration::from_millis(50));

    let mut c = TcpStream::connect(front).unwrap();
    c.write_all(b"GET /prefill HTTP/1.1\r\nhost: x\r\n\r\n")
        .unwrap();
    c.shutdown(Shutdown::Write).unwrap();
    let mut resp = String::new();
    c.read_to_string(&mut resp).unwrap();

    assert!(
        resp.ends_with('A'),
        "expected response from cheap backend, got: {resp:?}"
    );
}
