//! Die-hard integration tests (Tiers 1–2). Each emits `HARDEN_REPORT` for aggregation.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use demiurge_cost::kv_breakdown;
use demiurge_dataplane::AdmitMode;
use demiurge_router::{
    on_prefill_complete, serve, spawn_latch_prefill_backend, spawn_rst_backend, Backend,
    PrefillSignals, RequestId, RouteError, Router,
};

fn harden_report(tier: u8, id: &str, status: &str, detail: &str) {
    eprintln!("HARDEN_REPORT tier={tier} id={id} status={status} detail={detail}");
}

// [DEMI-XDP-SHED] Tier 1 — second TCP client gets HTTP 503 when admit exhausted.
#[test]
fn harden_tcp_503_on_admit_exhaust() {
    let (backend_addr, latch) = spawn_latch_prefill_backend();
    let pf = vec![Backend::new("pf", backend_addr, 0.01)];
    let router = Arc::new(Router::new(pf, vec![]).with_admit_mode(AdmitMode::Userspace));
    router.sync_admit_capacity(1);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = serve(listener, router);
    });
    thread::sleep(Duration::from_millis(50));

    let front_hold = front;
    let hold = thread::spawn(move || {
        let mut c = TcpStream::connect(front_hold).unwrap();
        c.write_all(b"GET /prefill HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .unwrap();
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 256];
        let _ = c.read(&mut buf);
    });
    thread::sleep(Duration::from_millis(150));

    let mut rejected = TcpStream::connect(front).unwrap();
    rejected
        .write_all(b"GET /prefill HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .unwrap();
    rejected
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut buf = [0u8; 256];
    let n = rejected.read(&mut buf).unwrap_or(0);
    let resp = String::from_utf8_lossy(&buf[..n]);

    latch.release();
    let _ = hold.join();

    assert!(
        resp.contains("503"),
        "expected HTTP 503 on admit exhaust, got: {resp:?}"
    );
    harden_report(1, "tcp_503_on_admit_exhaust", "PASS", "second_client=503");
}

// [DEMI-XDP-SHED] Tier 1 — single admit guard path (refactor regression).
#[test]
fn harden_admit_conn_single_guard_path() {
    let pf = Backend::new("pf", "127.0.0.1:1".parse().unwrap(), 0.01);
    let router = Router::new(vec![pf], vec![]).with_admit_mode(AdmitMode::Userspace);
    router.sync_admit_capacity(1);
    assert_eq!(router.admit_bucket().available(), 1);
    assert!(router.admit_bucket().try_admit().is_ok());
    assert_eq!(router.admit_bucket().available(), 0);
    router.admit_bucket().release(1);
    harden_report(
        1,
        "admit_conn_single_guard_path",
        "PASS",
        "one_token_per_conn",
    );
}

// [DEMI-KV-HANDOFF] Tier 1 — KvAdmitRejected surfaces as HTTP 503 on the wire.
#[test]
fn harden_kv_admit_rejected_returns_503() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let pf_addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nx-demiurge-prefill-done: 1\r\nx-demiurge-kv-handle: 1\r\nx-demiurge-kv-bytes: 50000\r\ncontent-length: 0\r\n\r\n",
            );
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    let dc_addr = "127.0.0.1:2".parse().unwrap();
    let pf = Backend::new("pf", pf_addr, 0.01);
    let dc = Backend::new("dc", dc_addr, 0.02);
    let (router, _ledger, _handoffs) = Router::with_kv_pool(vec![pf], vec![dc], 5_000, 128);
    let router = Arc::new(router);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = serve(listener, router);
    });
    thread::sleep(Duration::from_millis(50));

    let head = b"GET /long/2048 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 2048\r\nconnection: close\r\n\r\n";
    let mut c = TcpStream::connect(front).unwrap();
    c.write_all(head).unwrap();
    c.shutdown(Shutdown::Write).unwrap();
    let mut resp = String::new();
    c.read_to_string(&mut resp).unwrap();

    assert!(
        resp.contains("503"),
        "expected 503 on KV admit reject, got: {resp:?}"
    );
    harden_report(2, "kv_admit_rejected_returns_503", "PASS", "wire=503");
}

// [DEMI-KV-HANDOFF] Tier 2 — duplicate handoff reservation rejected, ledger unchanged.
#[test]
fn harden_handoff_duplicate_rejects_and_releases() {
    let pf = Backend::new("pf", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dc = Backend::new("dc", "127.0.0.1:2".parse().unwrap(), 0.02);
    let (router, ledger, _handoffs) = Router::with_kv_pool(vec![pf], vec![dc], 1_000_000, 128);

    let rid = RequestId::new();
    let signals = PrefillSignals {
        request_id: rid,
        prompt_tokens: 32,
        prefill_wall: Duration::from_micros(1),
    };
    let reserved = kv_breakdown(32, 128).kv_reserved;
    let with_handoff = format!(
        "HTTP/1.1 200 OK\r\nx-demiurge-kv-handle: 9\r\nx-demiurge-kv-bytes: {reserved}\r\n\r\n"
    );
    let placement =
        on_prefill_complete(&router, &signals, with_handoff.as_bytes(), "pf").expect("first");
    assert!(ledger.fleet_reserved() > 0);
    assert!(matches!(
        on_prefill_complete(&router, &signals, with_handoff.as_bytes(), "pf"),
        Err(RouteError::KvAdmitRejected)
    ));
    drop(placement);
    assert_eq!(ledger.fleet_reserved(), 0);
    harden_report(
        2,
        "handoff_duplicate_rejects_and_releases",
        "PASS",
        "ledger=0",
    );
}

// [DEMI-XDP-SHED] Tier 2 — hybrid admit mode matrix.
#[test]
fn harden_hybrid_admit_mode_matrix() {
    assert!(AdmitMode::Userspace.uses_userspace_admit(false));
    assert!(AdmitMode::Userspace.uses_userspace_admit(true));
    assert!(!AdmitMode::KernelXdp.uses_userspace_admit(false));
    assert!(!AdmitMode::KernelXdp.uses_userspace_admit(true));
    assert!(AdmitMode::Hybrid.uses_userspace_admit(false));
    assert!(!AdmitMode::Hybrid.uses_userspace_admit(true));

    let pf = Backend::new("pf", "127.0.0.1:1".parse().unwrap(), 0.01);
    let router = Router::new(vec![pf], vec![]).with_admit_mode(AdmitMode::Hybrid);
    assert!(!router.kernel_admit_attached());
    router.sync_admit_capacity(2);
    assert!(router.admit_bucket().try_admit().is_ok());
    harden_report(2, "hybrid_admit_mode_matrix", "PASS", "fallback=userspace");
}

// [DEMI-DP-RCU] Tier 2 — step actuation raises dataplane π (mirrors LOAD-STEP-ACTUATE).
#[test]
fn harden_step_actuation_raises_dataplane_pi() {
    let pf = Backend::new("pf0", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dc = Backend::new("dc0", "127.0.0.1:2".parse().unwrap(), 0.02);
    let router = Router::new(vec![pf.clone()], vec![dc]).with_rebalancer_actuation(true);
    for _ in 0..32 {
        pf.incr_inflight();
    }
    let short = b"GET / HTTP/1.1\r\nhost: x\r\n\r\n";
    let long = b"GET /long/4096 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 4096\r\n\r\n";
    for _ in 0..40 {
        let _ = demiurge_router::route(&router, short);
    }
    for _ in 0..80 {
        let _ = demiurge_router::route(&router, long);
    }
    let pi = router.dataplane_pi();
    assert!(
        pi >= 0.55,
        "expected actuated π ≥ 0.55 after step load, got {pi}"
    );
    harden_report(
        2,
        "step_actuation_raises_dataplane_pi",
        "PASS",
        &format!("pi={pi:.3}"),
    );
}

// [DEMI-XDP-SHED] Tier 2 — backend RST mid-proxy releases admit token.
#[test]
fn harden_backend_rst_releases_admit_token() {
    let rst = spawn_rst_backend();
    let pf = vec![Backend::new("pf", rst, 0.01)];
    let router = Arc::new(Router::new(pf, vec![]).with_admit_mode(AdmitMode::Userspace));
    router.sync_admit_capacity(1);
    let admit = Arc::clone(router.admit_bucket());

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
    let mut buf = [0u8; 256];
    let _ = c.read(&mut buf);
    thread::sleep(Duration::from_millis(100));

    assert_eq!(admit.available(), 1, "admit token must return after RST");
    harden_report(2, "backend_rst_releases_admit_token", "PASS", "tokens=1");
}

// [DEMI-KV-HANDOFF] Tier 2 — over-capacity handoff rejected at placement.
#[test]
fn harden_kv_over_capacity_rejected() {
    let pf = Backend::new("pf", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dc = Backend::new("dc", "127.0.0.1:2".parse().unwrap(), 0.02);
    let (router, ledger, _handoffs) = Router::with_kv_pool(vec![pf], vec![dc], 4_000, 128);

    let signals = PrefillSignals {
        request_id: RequestId::new(),
        prompt_tokens: 32,
        prefill_wall: Duration::from_micros(1),
    };
    let with_handoff =
        b"HTTP/1.1 200 OK\r\nx-demiurge-kv-handle: 9\r\nx-demiurge-kv-bytes: 50000\r\n\r\n";
    assert!(matches!(
        on_prefill_complete(&router, &signals, with_handoff, "pf"),
        Err(RouteError::KvAdmitRejected)
    ));
    assert_eq!(ledger.fleet_reserved(), 0);
    harden_report(2, "kv_over_capacity_rejected", "PASS", "reservation=0");
}
