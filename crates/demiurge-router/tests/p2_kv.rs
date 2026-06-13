use demiurge_cost::kv_breakdown;
use demiurge_router::{
    on_prefill_complete, Backend, Phase, PrefillSignals, RequestId, RouteError, Router,
};

// Integration: wired KV pool rejects decode without prefill hand-off headers.
#[test]
fn disaggregated_decode_requires_handoff() {
    let pf = Backend::new("pf", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dc = Backend::new("dc", "127.0.0.1:2".parse().unwrap(), 0.02);
    let (router, ledger, _handoffs) = Router::with_kv_pool(vec![pf], vec![dc], 1_000_000, 128);

    let signals = PrefillSignals {
        request_id: RequestId::new(),
        prompt_tokens: 32,
    };

    let empty = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
    assert!(matches!(
        on_prefill_complete(&router, &signals, empty, "pf"),
        Err(RouteError::HandoffMissing)
    ));

    let reserved = kv_breakdown(32, 128).kv_reserved;
    let with_handoff = format!(
        "HTTP/1.1 200 OK\r\nx-demiurge-kv-handle: 9\r\nx-demiurge-kv-bytes: {reserved}\r\n\r\n"
    );
    let placement =
        on_prefill_complete(&router, &signals, with_handoff.as_bytes(), "pf").expect("ok");
    assert_eq!(placement.backend().label, "dc");
    assert!(ledger.fleet_reserved() > 0);
    drop(placement);
    assert_eq!(ledger.fleet_reserved(), 0);
}

#[test]
fn phi_barrier_raises_decode_cost_under_pressure() {
    let pf = Backend::new("pf", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dc = Backend::new("dc", "127.0.0.1:2".parse().unwrap(), 0.02);
    let (router, ledger, _) = Router::with_kv_pool(vec![pf], vec![dc], 1_000_000, 128);

    let cheap = router.pick(Phase::Decode).expect("decode").cost();
    let chunk = 50_000_u64;
    for id in 0..8 {
        let _ = ledger.try_reserve(id, chunk).expect("reserve");
    }
    let phi = ledger.phi_barrier();
    let dear = router
        .pick_with_phi(Phase::Decode, Some(phi))
        .expect("decode")
        .cost_with_barriers(&[phi]);
    assert!(dear.ln() > cheap.ln());
}
