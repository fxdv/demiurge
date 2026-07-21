//! Track A portable features: handoff transport, corrector shadow, fast-path telemetry.

use std::sync::Arc;
use std::time::Duration;

use demiurge_control::{eval_goodput_improvement, train_bounded_delta};
use demiurge_cost::kv_breakdown;
use demiurge_handoff::MockRdmaTransport;
use demiurge_router::{on_prefill_complete, parse_pool, PrefillSignals, RequestId, Router};

#[test]
fn mock_rdma_transport_records_fast_wall() {
    let pf = parse_pool("pf0@127.0.0.1:9101@0.01").unwrap();
    let dc = parse_pool("dc0@127.0.0.1:9102@0.01").unwrap();
    let (router, _ledger, handoffs) = Router::with_kv_pool(pf, dc, 64 * 1024 * 1024, 128);
    let router = router.with_handoff_transport(Arc::new(MockRdmaTransport::new(25)));

    let reserved = kv_breakdown(512, 128).kv_reserved;
    let head = format!(
        "HTTP/1.1 200 OK\r\nx-demiurge-kv-handle: 9\r\nx-demiurge-kv-bytes: {reserved}\r\n\r\n"
    );
    let signals = PrefillSignals {
        request_id: RequestId::new(),
        prompt_tokens: 512,
        prefill_wall: Duration::from_millis(50),
    };
    on_prefill_complete(&router, &signals, head.as_bytes(), "pf0").expect("decode");
    let m = handoffs.transfer_metrics();
    assert_eq!(m.count, 1);
    assert_eq!(m.bytes_p50, reserved);
    assert_eq!(m.wall_us_p50, 25);
}

fn handoff_head(prompt_tokens: u64, bytes_per_token: u64, handle: u64) -> String {
    let reserved = kv_breakdown(prompt_tokens, bytes_per_token).kv_reserved;
    format!(
        "HTTP/1.1 200 OK\r\nx-demiurge-kv-handle: {handle}\r\nx-demiurge-kv-bytes: {reserved}\r\n\r\n"
    )
}

#[test]
fn corrector_shadow_records_on_prefill_complete() {
    let pf = parse_pool("pf0@127.0.0.1:9201@0.01").unwrap();
    let dc = parse_pool("dc0@127.0.0.1:9202@0.01").unwrap();
    let (router, _ledger, _handoffs) = Router::with_kv_pool(pf, dc, 64 * 1024 * 1024, 128);

    let head = handoff_head(512, 128, 11);
    let signals = PrefillSignals {
        request_id: RequestId::new(),
        prompt_tokens: 512,
        prefill_wall: Duration::from_millis(8),
    };
    on_prefill_complete(&router, &signals, head.as_bytes(), "pf0").expect("decode");
    let samples = router.corrector_shadow_samples();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].prompt_tokens, 512);
    assert!(samples[0].analytic_ln.is_finite());
}

/// T6 — under-reported claim cannot undercut observed prefill wall.
#[test]
fn observed_prefill_wall_raises_effective_t_core() {
    use demiurge_router::Phase;

    let pf = parse_pool("pf0@127.0.0.1:9211@0.001").unwrap(); // claim 1ms
    let dc = parse_pool("dc0@127.0.0.1:9212@0.01").unwrap();
    let (router, _ledger, _handoffs) = Router::with_kv_pool(pf, dc, 64 * 1024 * 1024, 128);
    let pf_backend = router.pool(Phase::Prefill)[0].clone();
    let claimed_ln = pf_backend.cost().ln();

    let head = handoff_head(512, 128, 12);
    let signals = PrefillSignals {
        request_id: RequestId::new(),
        prompt_tokens: 512,
        prefill_wall: Duration::from_millis(50), // observed 50ms >> claim
    };
    on_prefill_complete(&router, &signals, head.as_bytes(), "pf0").expect("decode");

    assert!(
        pf_backend.effective_base_seconds() > 0.001,
        "ewma must lift effective base above under-claim"
    );
    assert!(
        pf_backend.cost().ln() > claimed_ln,
        "observed wall must raise routing cost vs bare claim"
    );
}

#[test]
fn fast_path_ratio_tracks_colocated_routes() {
    let pf = parse_pool("pf0@127.0.0.1:9301@0.01").unwrap();
    let dc = parse_pool("dc0@127.0.0.1:9302@0.01").unwrap();
    let router = Router::with_kv_pool(pf, dc, 64 * 1024 * 1024, 128).0;

    let short = b"GET / HTTP/1.1\r\nHost: x\r\nX-Demiurge-Tokens: 64\r\n\r\n";
    demiurge_router::route(&router, short).expect("route");
    let metrics = router.control_metrics();
    assert_eq!(metrics.colocated_routes, 1);
    assert!(metrics.fast_path_ratio > 0.9);
}

#[test]
fn corrector_shadow_train_and_eval() {
    let pf = parse_pool("pf0@127.0.0.1:9401@0.01").unwrap();
    let dc = parse_pool("dc0@127.0.0.1:9402@0.01").unwrap();
    let (router, _ledger, _handoffs) = Router::with_kv_pool(pf, dc, 64 * 1024 * 1024, 128);

    let head = handoff_head(512, 128, 3);
    for i in 0..4 {
        let signals = PrefillSignals {
            request_id: RequestId::new(),
            prompt_tokens: 512,
            prefill_wall: Duration::from_millis(10),
        };
        on_prefill_complete(&router, &signals, head.as_bytes(), "pf0").expect("decode");
        let _ = i;
    }
    let samples = router.corrector_shadow_samples();
    let delta = train_bounded_delta(&samples);
    let goodput = eval_goodput_improvement(&samples, delta);
    assert!((1.0 - demiurge_cost::ALPHA..=1.0 + demiurge_cost::ALPHA).contains(&delta));
    assert!((0.0..=1.0).contains(&goodput));
}
