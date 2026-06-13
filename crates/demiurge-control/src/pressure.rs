//! Normalized pool pressure from state-plane gossip (shadow rebalancer inputs).

use demiurge_state::StateSnapshot;

#[derive(Debug, Clone, Copy, Default)]
pub struct PoolPressure {
    pub q_prefill: f64,
    pub q_decode: f64,
    pub kv_decode: f64,
    pub slo_prefill: f64,
    pub slo_decode: f64,
    pub fp_share: f64,
}

/// Export normalized `[0, 1]` pressure signals from a state snapshot.
pub fn export_pool_pressure(snapshot: &StateSnapshot, fp_share: f64) -> PoolPressure {
    let q_prefill = mean_occupancy(snapshot.prefill.values().map(|b| b.occupancy));
    let q_decode = mean_occupancy(snapshot.decode.values().map(|b| b.occupancy));
    let kv_decode = mean_f64(snapshot.decode.values().map(|b| b.kv_pressure()));
    PoolPressure {
        q_prefill,
        q_decode,
        kv_decode,
        slo_prefill: 0.0,
        slo_decode: 0.0,
        fp_share: fp_share.clamp(0.0, 1.0),
    }
}

fn mean_occupancy(values: impl Iterator<Item = f64>) -> f64 {
    mean_f64(values.map(|v| v.clamp(0.0, 1.0)))
}

fn mean_f64(values: impl Iterator<Item = f64>) -> f64 {
    let mut n = 0u64;
    let mut sum = 0.0;
    for v in values {
        sum += v;
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f64).clamp(0.0, 1.0)
    }
}
