use demiurge_router::{Backend, Router};

// [DEMI-DP-RCU] — live TCP path reads RCU π without blocking.
#[test]
fn router_exposes_dataplane_pi() {
    let pf = Backend::new("pf0", "127.0.0.1:1".parse().unwrap(), 0.01);
    let router = Router::new(vec![pf], vec![]);
    assert!((0.0..=1.0).contains(&router.dataplane_pi()));
}

// [DEMI-XDP-SHED] — admit bucket sheds when tokens exhausted.
#[test]
fn admit_bucket_sheds_on_live_router() {
    let pf = Backend::new("pf0", "127.0.0.1:1".parse().unwrap(), 0.01);
    let router = Router::new(vec![pf], vec![]);
    let bucket = router.admit_bucket();
    let cap = bucket.capacity();
    for _ in 0..cap {
        assert!(bucket.try_admit().is_ok());
    }
    assert!(bucket.try_admit().is_err());
    assert!(bucket.shed_total() >= 1);
}

#[test]
fn rebalancer_actuation_publishes_pi() {
    let pf = Backend::new("pf0", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dc = Backend::new("dc0", "127.0.0.1:2".parse().unwrap(), 0.02);
    let router = Router::new(vec![pf], vec![dc]).with_rebalancer_actuation(true);
    assert!(router.rebalancer_actuation());
    let head = b"GET /long/1024 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 1024\r\n\r\n";
    for _ in 0..32 {
        let _ = demiurge_router::route(&router, head);
    }
    let metrics = router.control_metrics();
    assert!(metrics.dataplane_pi >= 0.0 && metrics.dataplane_pi <= 1.0);
}
