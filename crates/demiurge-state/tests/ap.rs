use demiurge_state::{
    default_routing_blocks, stale_probe, GossipUpdate, StatePlane, StateSnapshot, WarmthMap,
};

// [DEMI-STATE-AP] — stale warmth causes miss only, never crash.
#[test]
fn stale_warmth_miss_only() {
    let stale = WarmthMap::default();
    let blocks = default_routing_blocks(512);
    assert_eq!(stale_probe(&stale, &blocks), 0.0);

    let mut hot = WarmthMap::default();
    for b in &blocks {
        assert!(hot.insert(*b));
    }
    assert!(stale_probe(&hot, &blocks) > 0.0);
}

// [DEMI-STATE-AP] — gossip partition heals without control plane.
#[test]
fn gossip_partition_heals_without_control_plane() {
    let plane = StatePlane::new(&["pf0".into(), "pf1".into()], &["dc0".into()]);
    plane.apply_gossip(GossipUpdate {
        backend_label: "pf0".into(),
        is_decode: false,
        warmth_blocks: vec![0, 256],
        occupancy: 0.2,
        kv_bytes_live: 0,
        epoch: 1,
    });

    let mut remote = StateSnapshot::empty();
    remote.generation = 2;
    remote
        .prefill
        .insert("pf1".into(), demiurge_state::BackendSnapshot::new("pf1", 0));
    if let Some(b) = remote.prefill.get_mut("pf1") {
        assert!(b.warmth.insert(512));
        b.occupancy = 0.9;
    }

    plane.heal_merge(&remote);
    let snap = plane.snapshot();
    assert!(snap.prefill.get("pf0").unwrap().warmth.contains(0));
    assert!(snap.prefill.get("pf1").unwrap().warmth.contains(512));
    assert_eq!(snap.generation, 2);
}
