use demiurge_router::{on_prefill_complete, Backend, PrefillSignals, RoutePath, Router};
use std::time::Duration;

// [DEMI-PAIR-GREEDY] — decode pick is conditional on prefill label (transfer penalty).
#[test]
fn greedy_decode_prefers_colocated_label_over_cheaper_remote() {
    let pf_a = Backend::new("node-a", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dc_a = Backend::new("node-a", "127.0.0.1:2".parse().unwrap(), 0.0195);
    let dc_b = Backend::new("node-b", "127.0.0.1:3".parse().unwrap(), 0.019);

    let router = Router::new(vec![pf_a], vec![dc_a, dc_b]);

    let head = b"GET /long/513 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 513\r\n\r\n";
    let RoutePath::Disaggregated {
        prefill,
        request_id,
        prompt_tokens,
    } = demiurge_router::route(&router, head).expect("route")
    else {
        panic!("expected disaggregated route");
    };
    assert_eq!(prefill.label, "node-a");

    let signals = PrefillSignals {
        request_id,
        prompt_tokens,
        prefill_wall: Duration::from_micros(1),
    };
    let placement = on_prefill_complete(
        &router,
        &signals,
        b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n",
        "node-a",
    )
    .expect("decode");
    assert_eq!(
        placement.backend().label,
        "node-a",
        "independent min-cost would pick node-b; pairing applies transfer penalty"
    );

    let metrics = router.control_metrics();
    assert_eq!(metrics.predictor_p90_tokens, 513);
    assert_eq!(metrics.pairing_regret_samples, 1);
}

// [DEMI-PAIR-GREEDY] — long disagg prefill uses greedy prefill pick (warmth-aware).
#[test]
fn greedy_prefill_picks_warm_backend() {
    use demiurge_router::{route, StateSnapshot};
    use demiurge_state::BackendSnapshot;

    let addr: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
    let cold = Backend::new("pf-cold", addr, 0.02);
    let warm = Backend::new("pf-warm", addr, 0.02);
    let decode = Backend::new("dc0", addr, 0.02);

    let mut snap = StateSnapshot::empty();
    let mut hot = BackendSnapshot::new("pf-warm", 0);
    for b in demiurge_state::default_routing_blocks(2048) {
        assert!(hot.warmth.insert(b));
    }
    snap.prefill.insert("pf-warm".into(), hot);
    snap.prefill
        .insert("pf-cold".into(), BackendSnapshot::new("pf-cold", 0));

    let router = Router::new(vec![cold, warm], vec![decode]).with_state(snap);
    let head = b"GET /long/2048 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 2048\r\n\r\n";
    match route(&router, head).expect("route") {
        RoutePath::Disaggregated { prefill, .. } => assert_eq!(prefill.label, "pf-warm"),
        other => panic!("expected disagg to warm backend, got {other:?}"),
    }
}
