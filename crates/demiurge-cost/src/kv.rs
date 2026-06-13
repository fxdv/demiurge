//! KV reservation accounting and fleet Φ memory-pressure barrier.
//!
//! - [DEMI-KV-OVERHEAD] — metadata + fragmentation terms, not raw token bytes.
//! - [DEMI-BARRIER-PHI] — fleet-aggregate marginal pressure, not `N × p90`.

use crate::{
    BarrierFactor, CACHE_BLOCK_TOKENS, KV_FRAGMENTATION_SLACK, KV_METADATA_OVERHEAD_FRACTION,
};

/// Block-aligned KV footprint breakdown for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvBreakdown {
    pub prompt_tokens: u64,
    pub kv_tokens: u64,
    pub kv_payload: u64,
    pub kv_metadata: u64,
    pub kv_fragment: u64,
    pub kv_reserved: u64,
}

/// Compute overhead-aware reservation bytes for a prompt.
///
/// ```text
/// kv_tokens   = ceil(prompt / block) × block
/// kv_payload  = kv_tokens × bytes_per_token
/// kv_metadata = kv_payload × metadata_overhead_fraction
/// kv_fragment = kv_payload × fragmentation_slack
/// kv_reserved = kv_payload + kv_metadata + kv_fragment
/// ```
pub fn kv_breakdown(prompt_tokens: u64, bytes_per_token: u64) -> KvBreakdown {
    let block = CACHE_BLOCK_TOKENS.max(1);
    let kv_tokens = prompt_tokens.div_ceil(block) * block;
    let kv_payload = kv_tokens.saturating_mul(bytes_per_token);
    let kv_metadata = (kv_payload as f64 * KV_METADATA_OVERHEAD_FRACTION).ceil() as u64;
    let kv_fragment = (kv_payload as f64 * KV_FRAGMENTATION_SLACK).ceil() as u64;
    let kv_reserved = kv_payload + kv_metadata + kv_fragment;
    KvBreakdown {
        prompt_tokens,
        kv_tokens,
        kv_payload,
        kv_metadata,
        kv_fragment,
        kv_reserved,
    }
}

/// 90th percentile of a non-empty sample (nearest-rank).
pub fn percentile90(mut samples: Vec<u64>) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = ((samples.len() as f64 * 0.9).ceil() as usize).saturating_sub(1);
    samples[idx]
}

/// Fleet marginal bytes for admission/routing pressure.
///
/// Uses **one** p90 increment on top of live fleet reservation — never
/// `fleet_reserved + n × p90` ([DEMI-BARRIER-PHI]).
pub fn fleet_marginal_bytes(fleet_reserved: u64, p90_increment: u64) -> u64 {
    fleet_reserved.saturating_add(p90_increment)
}

/// Wrong pattern explicitly rejected by [DEMI-BARRIER-PHI].
pub fn fleet_marginal_bytes_wrong(fleet_reserved: u64, p90_increment: u64, n: u64) -> u64 {
    fleet_reserved.saturating_add(p90_increment.saturating_mul(n))
}

/// Φ memory-pressure barrier from fleet utilization in `[0, 1)`.
///
/// Monotonic: higher reserved/capacity → higher penalty (≥ 1).
pub fn phi_barrier(fleet_reserved: u64, capacity_bytes: u64) -> BarrierFactor {
    if capacity_bytes == 0 {
        return BarrierFactor::clamped(f64::MAX);
    }
    let util = (fleet_reserved as f64 / capacity_bytes as f64).clamp(0.0, 0.999);
    let penalty = 1.0 / (1.0 - util);
    BarrierFactor::clamped(penalty)
}

/// Φ barrier using fleet-aggregate marginal occupancy ([DEMI-BARRIER-PHI]).
pub fn phi_barrier_marginal(
    fleet_reserved: u64,
    p90_increment: u64,
    capacity_bytes: u64,
) -> BarrierFactor {
    phi_barrier(
        fleet_marginal_bytes(fleet_reserved, p90_increment),
        capacity_bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_reserved_includes_overhead() {
        let b = kv_breakdown(100, 128);
        assert_eq!(b.kv_tokens, 256);
        assert_eq!(b.kv_payload, 256 * 128);
        assert!(b.kv_metadata > 0);
        assert!(b.kv_fragment > 0);
        assert_eq!(b.kv_reserved, b.kv_payload + b.kv_metadata + b.kv_fragment);
        assert!(b.kv_reserved > b.kv_payload);
    }

    #[test]
    fn phi_uses_fleet_aggregate_not_p90_sum() {
        let fleet_reserved = 500_u64;
        let p90 = 100_u64;
        let capacity = 700_u64;
        let correct = fleet_marginal_bytes(fleet_reserved, p90);
        let wrong = fleet_marginal_bytes_wrong(fleet_reserved, p90, 10);
        assert_eq!(correct, 600);
        assert_eq!(wrong, 1500);
        assert!(correct <= capacity);
        assert!(wrong > capacity);
        let b_ok = phi_barrier_marginal(fleet_reserved, p90, capacity);
        let b_wrong = phi_barrier(wrong, capacity);
        assert!(b_ok.get() < b_wrong.get());
    }

    #[test]
    fn phi_barrier_monotonic_with_load() {
        let cap = 10_000_u64;
        let low = phi_barrier(2_000, cap).get();
        let high = phi_barrier(8_000, cap).get();
        assert!(high > low);
        assert!(low >= 1.0);
    }
}
