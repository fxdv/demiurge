//! AP gossip merge for warmth and occupancy. [DEMI-STATE-AP]

use crate::snapshot::{BackendSnapshot, StatePlane, StateSnapshot};
use crate::warmth::WarmthMap;

#[derive(Debug, Clone)]
pub struct GossipUpdate {
    pub backend_label: String,
    pub is_decode: bool,
    pub warmth_blocks: Vec<u64>,
    pub occupancy: f64,
    pub kv_bytes_live: u64,
    pub epoch: u64,
}

impl StatePlane {
    pub fn apply_gossip(&self, update: GossipUpdate) {
        let mut snap = self.snapshot();
        let pool = if update.is_decode {
            &mut snap.decode
        } else {
            &mut snap.prefill
        };
        if let Some(backend) = pool.get_mut(&update.backend_label) {
            for block in update.warmth_blocks {
                backend.warmth.insert(block);
            }
            backend.occupancy = update.occupancy.clamp(0.0, 1.0);
            backend.kv_bytes_live = update.kv_bytes_live;
        }
        snap.generation = snap.generation.max(update.epoch);
        self.publish_snapshot(snap);
    }

    /// Partition heal: merge remote warmth/telemetry without CP (max generation wins).
    pub fn heal_merge(&self, remote: &StateSnapshot) {
        let mut local = self.snapshot();
        merge_pool(&mut local.prefill, &remote.prefill);
        merge_pool(&mut local.decode, &remote.decode);
        local.generation = local.generation.max(remote.generation);
        self.publish_snapshot(local);
    }
}

fn merge_pool(
    local: &mut std::collections::HashMap<String, BackendSnapshot>,
    remote: &std::collections::HashMap<String, BackendSnapshot>,
) {
    for (label, remote_b) in remote {
        local
            .entry(label.clone())
            .and_modify(|b| {
                b.warmth.merge(&remote_b.warmth);
                b.occupancy = b.occupancy.max(remote_b.occupancy);
                b.kv_bytes_live = b.kv_bytes_live.max(remote_b.kv_bytes_live);
            })
            .or_insert_with(|| remote_b.clone());
    }
}

/// Stale entries that no longer match live keys behave as misses (empty map probe).
pub fn stale_probe(_stale: &WarmthMap, blocks: &[u64]) -> f64 {
    _stale.hit_strength(blocks)
}
