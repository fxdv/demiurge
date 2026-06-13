use demiurge_router::{route, Backend, RoutePath, Router, StateSnapshot};
use demiurge_state::BackendSnapshot;

// [DEMI-SHORT-FASTPATH] [DEMI-WARM-DISCOUNT] — warmth override forces disaggregated path.
#[test]
fn warmth_override_forces_disaggregated_path() {
    let pf_addr = "127.0.0.1:1".parse().unwrap();
    let dc_addr = "127.0.0.1:2".parse().unwrap();
    let prefill = Backend::new("pf-hot", pf_addr, 0.01);
    let decode = Backend::new("dc0", dc_addr, 0.02);

    let mut snap = StateSnapshot::empty();
    let mut hot = BackendSnapshot::new("pf-hot", 0);
    for b in demiurge_state::default_routing_blocks(32) {
        assert!(hot.warmth.insert(b));
    }
    snap.prefill.insert("pf-hot".into(), hot);

    let router = Router::new(vec![prefill], vec![decode]).with_state(snap);
    let head = b"GET / HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 32\r\n\r\n";
    match route(&router, head).expect("route") {
        RoutePath::Disaggregated { .. } => {}
        other => panic!("expected warmth override disagg, got {other:?}"),
    }
}

// [DEMI-WARM-DISCOUNT] — warmth-aware routing prefers warm backend.
#[test]
fn warmth_aware_routing_improves_hit_ratio() {
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
