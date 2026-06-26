use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use demiurge_router::{
    admit_disaggregated, dispatch_prefill, route, spawn_delay_backend, spawn_latch_prefill_backend,
    Backend, RequestId, RoutePath, Router,
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
    let worker = dispatch_prefill(prefill, head, RequestId::new(), 2048, move |_, _r| {
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

// [ALG-ROUTE] — under synthetic load, admit p99 does not track prefill duration.
#[test]
fn accept_latency_decoupled_from_prefill_duration_under_load() {
    const WORKERS: u32 = 16;
    const PER_WORKER: u32 = 50;
    const PROMPT: u64 = 2048;
    let head =
        format!("GET /long/{PROMPT} HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: {PROMPT}\r\n\r\n");

    let p99_fast = measure_admit_p99(
        head.as_bytes(),
        Duration::from_micros(500),
        WORKERS,
        PER_WORKER,
    );
    thread::sleep(Duration::from_millis(200));

    let p99_slow = measure_admit_p99(
        head.as_bytes(),
        Duration::from_millis(50),
        WORKERS,
        PER_WORKER,
    );
    thread::sleep(Duration::from_millis(100));

    let ratio = p99_slow as f64 / p99_fast.max(1) as f64;
    assert!(
        ratio < 8.0,
        "accept p99 tracked prefill duration: fast={p99_fast}µs slow={p99_slow}µs ratio={ratio:.2}"
    );
}

fn measure_admit_p99(head: &[u8], prefill_delay: Duration, workers: u32, per_worker: u32) -> u64 {
    let pf_addr = spawn_delay_backend(prefill_delay);
    let dc_addr = spawn_delay_backend(Duration::from_micros(1));
    let router = Arc::new(Router::new(
        vec![Backend::new("pf", pf_addr, 0.01)],
        vec![Backend::new("dc", dc_addr, 0.02)],
    ));

    let ok = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let latencies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::new();

    for _ in 0..workers {
        let ok = Arc::clone(&ok);
        let latencies = Arc::clone(&latencies);
        let router = Arc::clone(&router);
        let head = head.to_vec();
        handles.push(thread::spawn(move || {
            for _ in 0..per_worker {
                if let Ok(d) = admit_disaggregated(&router, &head) {
                    ok.fetch_add(1, Ordering::Relaxed);
                    latencies.lock().expect("lat").push(d.as_micros() as u64);
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }

    let mut samples = latencies.lock().expect("lat").clone();
    assert_eq!(
        ok.load(Ordering::Relaxed),
        u64::from(workers) * u64::from(per_worker),
        "all admits should succeed"
    );
    samples.sort_unstable();
    percentile(&samples, 0.99)
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
    sorted[idx]
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
