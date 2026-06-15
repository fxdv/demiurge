use std::thread;
use std::time::Duration;

use demiurge_cost::DATAPLANE_RCU_HEARTBEAT_MS;
use demiurge_dataplane::{admit_capacity_for_pi, pool_core_scale, AdmitBucket};
use demiurge_router::{route, Backend, Router};

// [DEMI-DP-RCU] — RCU heartbeat keeps snapshot fresh when actuation is idle.
#[test]
fn rcu_heartbeat_refreshes_snapshot_under_shadow_mode() {
    let pf = Backend::new("pf0", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dc = Backend::new("dc0", "127.0.0.1:2".parse().unwrap(), 0.02);
    let router = Router::new(vec![pf], vec![dc]);
    thread::sleep(Duration::from_millis(DATAPLANE_RCU_HEARTBEAT_MS + 100));
    let head = b"GET / HTTP/1.1\r\nhost: x\r\n\r\n";
    let _ = route(&router, head);
    let metrics = router.control_metrics();
    assert!(!metrics.rcu_stale, "age {}ms", metrics.dataplane_age_ms);
}

// [DEMI-DP-RCU] — live TCP path reads RCU π without blocking.
#[test]
fn router_exposes_dataplane_pi() {
    let pf = Backend::new("pf0", "127.0.0.1:1".parse().unwrap(), 0.01);
    let router = Router::new(vec![pf], vec![]);
    assert!((0.0..=1.0).contains(&router.dataplane_pi()));
}

#[test]
fn pool_core_scale_biases_prefill_under_high_pi() {
    let base = 0.02;
    let prefill_at_high = pool_core_scale(base, 0.85, true);
    let prefill_at_mid = pool_core_scale(base, 0.5, true);
    assert!(prefill_at_high < prefill_at_mid);
    let decode_at_high = pool_core_scale(base, 0.85, false);
    assert!(decode_at_high > base);
}

// [DEMI-XDP-SHED] — admit bucket sheds when tokens exhausted.
#[test]
fn admit_bucket_reseed_resets_tokens() {
    let bucket = AdmitBucket::new(8);
    for _ in 0..8 {
        assert!(bucket.try_admit().is_ok());
    }
    bucket.reseed(4);
    assert_eq!(bucket.capacity(), 4);
    assert_eq!(bucket.available(), 4);
}

#[test]
fn admit_capacity_scales_with_pi() {
    assert_eq!(admit_capacity_for_pi(4096, 0.5), 2048);
    assert_eq!(admit_capacity_for_pi(4096, 1.0), 4096);
}

#[test]
fn actuation_syncs_admit_capacity_with_pi() {
    let pf = Backend::new("pf0", "127.0.0.1:1".parse().unwrap(), 0.01);
    let dc = Backend::new("dc0", "127.0.0.1:2".parse().unwrap(), 0.02);
    let router = Router::new(vec![pf], vec![dc]).with_rebalancer_actuation(true);
    let head = b"GET /long/4096 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 4096\r\n\r\n";
    for _ in 0..64 {
        let _ = route(&router, head);
    }
    let pi = router.dataplane_pi();
    let expected = admit_capacity_for_pi(demiurge_cost::DATAPLANE_ADMIT_BURST, pi);
    assert_eq!(router.admit_bucket().capacity(), expected);
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
    assert!(!metrics.rcu_stale);
    assert_eq!(metrics.rcu_stale_alert_ms, 500);
}
