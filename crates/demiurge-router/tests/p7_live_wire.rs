//! Live-wire hardening: cache-domain isolation on the *served* TCP path
//! (identity from trusted edge headers) and resource caps on `serve`.
//! [DEMI-S1-DOMAIN]

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use demiurge_auth::{GroupId, PrefixFingerprint, SharedPrefixGroupRegistry, TenantId};
use demiurge_router::{
    parse_cache_groups, parse_request_identity, serve, serve_with_max_conns,
    spawn_latch_prefill_backend, Backend, RequestIdentity, Router, StateSnapshot,
};
use demiurge_state::BackendSnapshot;

/// Prefill/decode stub that counts connections and answers 200 immediately.
fn spawn_counting_backend() -> (SocketAddr, Arc<AtomicU64>) {
    let hits = Arc::new(AtomicU64::new(0));
    let hits2 = Arc::clone(&hits);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            hits2.fetch_add(1, Ordering::Relaxed);
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    (addr, hits)
}

fn send_request(front: SocketAddr, head: &[u8]) -> String {
    let mut c = TcpStream::connect(front).expect("connect router");
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(head).expect("write");
    c.shutdown(Shutdown::Write).expect("shutdown");
    let mut resp = Vec::new();
    let _ = c.read_to_end(&mut resp);
    String::from_utf8_lossy(&resp).into_owned()
}

fn identity_head(tenant: u64, group: u64, fp: u64) -> Vec<u8> {
    format!(
        "GET /long/2048 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 2048\r\n\
         x-demiurge-tenant: {tenant}\r\nx-demiurge-group: {group}\r\n\
         x-demiurge-prefix-fp: {fp}\r\nconnection: close\r\n\r\n"
    )
    .into_bytes()
}

/// The full served TCP stack: two equal-cost prefill backends, one warmed
/// under the group's shared cache-domain key. A member presenting matching
/// content (via trusted edge headers) must land on the warm backend; a
/// non-member presenting byte-identical content must not. [DEMI-S1-DOMAIN]
#[test]
fn live_tcp_path_gates_warmth_by_identity() {
    let (cold_addr, cold_hits) = spawn_counting_backend();
    let (warm_addr, warm_hits) = spawn_counting_backend();
    let (dc_addr, _dc_hits) = spawn_counting_backend();

    let group = GroupId::new(7);
    let content = PrefixFingerprint::of(b"shared system prompt");
    let mut reg = SharedPrefixGroupRegistry::new();
    reg.register_template(group, [TenantId::new(1), TenantId::new(2)], content, 42);
    let shared_key = reg
        .resolve_shared_key(TenantId::new(1), group, content)
        .expect("member resolves shared key");

    let mut snap = StateSnapshot::empty();
    let mut hot = BackendSnapshot::new("pf-shared", 0);
    for b in demiurge_state::default_routing_blocks(2048) {
        assert!(hot.warmth.insert_salted(b, &shared_key));
    }
    snap.prefill.insert("pf-shared".into(), hot);
    snap.prefill
        .insert("pf-cold".into(), BackendSnapshot::new("pf-cold", 0));

    let router = Router::new(
        vec![
            Backend::new("pf-cold", cold_addr, 0.02),
            Backend::new("pf-shared", warm_addr, 0.02),
        ],
        vec![Backend::new("dc0", dc_addr, 0.02)],
    )
    .with_state(snap)
    .with_cache_registry(Arc::new(reg));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    let router = Arc::new(router);
    thread::spawn(move || {
        let _ = serve(listener, router);
    });
    thread::sleep(Duration::from_millis(50));

    // Member 2, matching content: warmth discount applies → warm backend.
    let resp = send_request(front, &identity_head(2, 7, content.raw()));
    assert!(resp.contains("200"), "member request failed: {resp:?}");
    assert_eq!(warm_hits.load(Ordering::Relaxed), 1, "member → pf-shared");
    assert_eq!(cold_hits.load(Ordering::Relaxed), 0);

    // Non-member 99, byte-identical content: no shared discount → tie breaks
    // to the first candidate (pf-cold), never the shared-domain backend.
    let resp = send_request(front, &identity_head(99, 7, content.raw()));
    assert!(resp.contains("200"), "non-member request failed: {resp:?}");
    assert_eq!(cold_hits.load(Ordering::Relaxed), 1, "non-member → pf-cold");
    assert_eq!(warm_hits.load(Ordering::Relaxed), 1, "no second warm hit");
}

// [DEMI-S1-DOMAIN] — header → identity parsing: all three headers required,
// decimal and hex accepted, garbage rejected.
#[test]
fn parse_request_identity_requires_all_headers() {
    let full = b"GET / HTTP/1.1\r\nx-demiurge-tenant: 5\r\nx-demiurge-group: 0x7\r\nx-demiurge-prefix-fp: 42\r\n\r\n";
    assert_eq!(
        parse_request_identity(full),
        Some(RequestIdentity {
            tenant: TenantId::new(5),
            group: GroupId::new(7),
            content_fp: PrefixFingerprint::new(42),
        })
    );

    let partial = b"GET / HTTP/1.1\r\nx-demiurge-tenant: 5\r\nx-demiurge-group: 7\r\n\r\n";
    assert_eq!(parse_request_identity(partial), None, "missing fp header");

    let garbage = b"GET / HTTP/1.1\r\nx-demiurge-tenant: bogus\r\nx-demiurge-group: 7\r\nx-demiurge-prefix-fp: 42\r\n\r\n";
    assert_eq!(parse_request_identity(garbage), None, "non-numeric tenant");
}

// [DEMI-S1-DOMAIN] — env spec → registry parsing round-trips membership.
#[test]
fn parse_cache_groups_spec_round_trips() {
    let reg = parse_cache_groups("7@42@0xABC@1+2, 9@1@5@3")
        .expect("valid spec")
        .expect("non-empty registry");
    assert!(reg.is_member(GroupId::new(7), TenantId::new(1)));
    assert!(reg.is_member(GroupId::new(7), TenantId::new(2)));
    assert!(!reg.is_member(GroupId::new(7), TenantId::new(3)));
    assert!(reg.is_member(GroupId::new(9), TenantId::new(3)));
    assert!(reg.matches_template(GroupId::new(7), PrefixFingerprint::new(0xABC)));

    assert!(parse_cache_groups("").expect("empty spec ok").is_none());
    assert!(parse_cache_groups("7@42@1").is_err(), "missing tenants");
    assert!(parse_cache_groups("7@42@1@").is_err(), "empty tenant list");
}

/// `serve_with_max_conns` sheds the connection over the cap with an immediate
/// 503 instead of spawning an unbounded thread — the L7 connection budget.
#[test]
fn serve_sheds_503_over_connection_cap() {
    let (backend_addr, latch) = spawn_latch_prefill_backend();
    let router = Arc::new(Router::new(
        vec![Backend::new("pf", backend_addr, 0.01)],
        vec![],
    ));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = serve_with_max_conns(listener, router, 1);
    });
    thread::sleep(Duration::from_millis(50));

    // First connection occupies the single slot (prefill latched open).
    let front_hold = front;
    let hold = thread::spawn(move || {
        let mut c = TcpStream::connect(front_hold).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(b"GET /prefill/1 HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
            .unwrap();
        let mut buf = [0u8; 256];
        let _ = c.read(&mut buf);
    });
    thread::sleep(Duration::from_millis(150));

    let resp = send_request(
        front,
        b"GET /prefill/2 HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n",
    );
    assert!(
        resp.contains("503"),
        "expected 503 over connection cap, got: {resp:?}"
    );

    latch.release();
    let _ = hold.join();

    // Slot released — the next connection proceeds normally.
    thread::sleep(Duration::from_millis(100));
    let resp = send_request(
        front,
        b"GET /prefill/3 HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n",
    );
    assert!(
        resp.contains("200"),
        "expected 200 after slot release, got: {resp:?}"
    );
}

/// A prefill backend streaming more than the response cap fails the hand-off
/// gracefully (503 to the client) instead of buffering unbounded memory.
#[test]
fn oversized_prefill_response_sheds_gracefully() {
    let big = demiurge_cost::DATAPLANE_PREFILL_RESPONSE_MAX_BYTES as usize + 64 * 1024;
    let pf_addr = demiurge_router::spawn_large_body_backend(big);
    let (dc_addr, dc_hits) = spawn_counting_backend();
    let router = Arc::new(Router::new(
        vec![Backend::new("pf", pf_addr, 0.01)],
        vec![Backend::new("dc", dc_addr, 0.02)],
    ));

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let front = listener.local_addr().unwrap();
    thread::spawn(move || {
        let _ = serve(listener, router);
    });
    thread::sleep(Duration::from_millis(50));

    let resp = send_request(
        front,
        b"GET /long/2048 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 2048\r\nconnection: close\r\n\r\n",
    );
    assert!(
        resp.contains("503"),
        "expected 503 on oversized prefill response, got: {:?}",
        &resp[..resp.len().min(120)]
    );
    assert_eq!(
        dc_hits.load(Ordering::Relaxed),
        0,
        "decode must not run after failed prefill"
    );
}
