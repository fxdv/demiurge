use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use demiurge_router::{
    dispatch_prefill, route, spawn_latch_prefill_backend, Backend, RequestId, RoutePath, Router,
};

// [ALG-ROUTE] — dispatch_prefill returns before prefill I/O completes.
#[test]
fn route_returns_before_prefill_complete() {
    let (addr, latch) = spawn_latch_prefill_backend();
    let prefill = Backend::new("latched", addr, 0.01);
    let head = b"GET /long/2048 HTTP/1.1\r\nhost: x\r\n\r\n".to_vec();

    let completed = Arc::new(AtomicBool::new(false));
    let completed2 = Arc::clone(&completed);

    let dispatch_at = Instant::now();
    let worker = dispatch_prefill(prefill, head, RequestId::new(), 2048, move |_, _| {
        completed2.store(true, Ordering::SeqCst);
    });

    assert!(
        dispatch_at.elapsed() < Duration::from_millis(100),
        "dispatch_prefill should return immediately, took {:?}",
        dispatch_at.elapsed()
    );
    assert!(
        !completed.load(Ordering::SeqCst),
        "on_complete must not run before prefill finishes"
    );

    latch.release();
    worker.join().expect("prefill worker");
    thread::sleep(Duration::from_millis(50));
    assert!(
        completed.load(Ordering::SeqCst),
        "on_complete should run after latch release"
    );
}

// [DEMI-SHORT-FASTPATH] — short prompts use colocated routing, not disaggregated.
#[test]
fn short_context_uses_colocated_path() {
    let prefill_addr = "127.0.0.1:1".parse().unwrap();
    let decode_addr = "127.0.0.1:2".parse().unwrap();
    let prefill = Backend::new("pf", prefill_addr, 0.01);
    let decode = Backend::new("dc", decode_addr, 0.02);
    let router = Router::new(vec![prefill], vec![decode]);

    let head = b"GET / HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 32\r\n\r\n";
    match route(&router, head).expect("route") {
        RoutePath::Colocated(b) => assert_eq!(b.label, "pf"),
        other => panic!("expected colocated fast path, got {other:?}"),
    }
}
