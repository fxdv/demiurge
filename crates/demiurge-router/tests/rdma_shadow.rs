//! RDMA cost shadow — topology-aware transfer logging. [DEMI-RDMA-COST-SHADOW]

use std::sync::Arc;
use std::time::Duration;

use demiurge_control::eval_transfer_ratio_median;
use demiurge_handoff::ModeledRdmaTransport;
use demiurge_router::{
    on_prefill_complete, parse_pool_with_topology, parse_topology_map, PrefillSignals, RequestId,
    Router,
};

fn handoff_head(prompt_tokens: u64, bytes_per_token: u64, handle: u64) -> String {
    let reserved = demiurge_cost::kv_breakdown(prompt_tokens, bytes_per_token).kv_reserved;
    format!(
        "HTTP/1.1 200 OK\r\nx-demiurge-kv-handle: {handle}\r\nx-demiurge-kv-bytes: {reserved}\r\n\r\n"
    )
}

#[test]
fn rdma_cost_shadow_records_on_disagg_handoff() {
    let topo = parse_topology_map("pf0@n0/r0/cA,dc0@n1/r0/cA,dc1@n2/r1/cB").unwrap();
    let pf = parse_pool_with_topology("pf0@127.0.0.1:9501@0.01", &topo).unwrap();
    let dc =
        parse_pool_with_topology("dc0@127.0.0.1:9502@0.01,dc1@127.0.0.1:9503@0.01", &topo).unwrap();
    let (router, _ledger, _handoffs) = Router::with_kv_pool(pf, dc, 64 * 1024 * 1024, 128);
    let router = router.with_handoff_transport(Arc::new(ModeledRdmaTransport::new(topo)));

    let head = handoff_head(512, 128, 7);
    let signals = PrefillSignals {
        request_id: RequestId::new(),
        prompt_tokens: 512,
        prefill_wall: Duration::from_millis(5),
    };
    on_prefill_complete(&router, &signals, head.as_bytes(), "pf0").expect("decode");

    let samples = router.rdma_cost_shadow_samples();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].pf_label, "pf0");
    assert!(samples[0].distance > 0);
    assert!(samples[0].analytic_transfer_ln.is_finite());
    let predicted = samples[0].analytic_transfer_ln.exp();
    let observed = samples[0].observed_transfer_secs;
    assert!((observed - predicted).abs() < 1e-9);

    let ratio = eval_transfer_ratio_median(&samples);
    assert!((ratio - 1.0).abs() < 1e-3);
}

#[test]
fn rdma_cost_shadow_skips_colocated_handoff() {
    let topo = parse_topology_map("pf0@n0/r0/cA").unwrap();
    let backends = parse_pool_with_topology("pf0@127.0.0.1:9601@0.01", &topo).unwrap();
    let (router, _ledger, _handoffs) =
        Router::with_kv_pool(backends.clone(), backends, 64 * 1024 * 1024, 128);

    let head = handoff_head(512, 128, 8);
    let signals = PrefillSignals {
        request_id: RequestId::new(),
        prompt_tokens: 512,
        prefill_wall: Duration::from_millis(5),
    };
    on_prefill_complete(&router, &signals, head.as_bytes(), "pf0").expect("decode");
    assert!(router.rdma_cost_shadow_samples().is_empty());
}
