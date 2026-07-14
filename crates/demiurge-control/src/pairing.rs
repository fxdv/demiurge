//! Greedy pf→dc pairing (documented approximation). [DEMI-PAIR-GREEDY]

use std::sync::Arc;

use demiurge_cost::{warmth_discount, BarrierFactor, Cost};
use demiurge_state::{default_routing_blocks, StateSnapshot};

use crate::scored::ScoredBackend;

/// Cross-node KV transfer barrier when pf label != dc label (Phase 4 pairing).
pub const DEFAULT_TRANSFER_PENALTY: f64 = demiurge_cost::ROUTING_TRANSFER_PENALTY;

/// Live routing and shadow pairing share this cost surface.
pub trait PairingTarget {
    fn label(&self) -> &str;

    fn prefill_ln(&self, snapshot: Option<&StateSnapshot>, blocks: &[u64]) -> f64;

    fn decode_ln(
        &self,
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        prefill_label: &str,
        transfer_penalty: f64,
        extra_barriers: &[BarrierFactor],
    ) -> f64;
}

impl PairingTarget for ScoredBackend {
    fn label(&self) -> &str {
        &self.label
    }

    fn prefill_ln(&self, snapshot: Option<&StateSnapshot>, blocks: &[u64]) -> f64 {
        prefill_cost(self, snapshot, blocks).ln()
    }

    fn decode_ln(
        &self,
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        prefill_label: &str,
        transfer_penalty: f64,
        extra_barriers: &[BarrierFactor],
    ) -> f64 {
        decode_cost(
            self,
            snapshot,
            blocks,
            prefill_label,
            transfer_penalty,
            extra_barriers,
        )
        .ln()
    }
}

/// Prefill target fixed first; decode optimized conditional on pf.
pub fn select_prefill_target<T: PairingTarget + ?Sized>(
    candidates: &[Arc<T>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
) -> Option<Arc<T>> {
    let blocks = default_routing_blocks(prompt_tokens);
    candidates
        .iter()
        .min_by(|a, b| {
            a.prefill_ln(snapshot, &blocks)
                .total_cmp(&b.prefill_ln(snapshot, &blocks))
        })
        .cloned()
}

pub fn select_decode_target<T: PairingTarget + ?Sized>(
    prefill_label: &str,
    candidates: &[Arc<T>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    transfer_penalty: f64,
    extra_barriers: &[BarrierFactor],
) -> Option<Arc<T>> {
    let blocks = default_routing_blocks(prompt_tokens);
    candidates
        .iter()
        .min_by(|a, b| {
            a.decode_ln(
                snapshot,
                &blocks,
                prefill_label,
                transfer_penalty,
                extra_barriers,
            )
            .total_cmp(&b.decode_ln(
                snapshot,
                &blocks,
                prefill_label,
                transfer_penalty,
                extra_barriers,
            ))
        })
        .cloned()
}

pub fn select_prefill(
    candidates: &[Arc<ScoredBackend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
) -> Option<Arc<ScoredBackend>> {
    select_prefill_target(candidates, snapshot, prompt_tokens)
}

pub fn select_decode(
    prefill: &ScoredBackend,
    candidates: &[Arc<ScoredBackend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    transfer_penalty: f64,
) -> Option<Arc<ScoredBackend>> {
    select_decode_target(
        prefill.label(),
        candidates,
        snapshot,
        prompt_tokens,
        transfer_penalty,
        &[],
    )
}

fn prefill_cost(backend: &ScoredBackend, snapshot: Option<&StateSnapshot>, blocks: &[u64]) -> Cost {
    let mut discounts = Vec::new();
    if let Some(snap) = snapshot {
        if let Some(bs) = snap.prefill.get(&backend.label) {
            if let Some(d) = warmth_discount(bs.warmth.hit_strength(blocks)) {
                discounts.push(d);
            }
        }
    }
    backend.base_cost(&[], &discounts)
}

fn decode_cost(
    backend: &ScoredBackend,
    snapshot: Option<&StateSnapshot>,
    blocks: &[u64],
    prefill_label: &str,
    transfer_penalty: f64,
    extra_barriers: &[BarrierFactor],
) -> Cost {
    let mut discounts = Vec::new();
    if let Some(snap) = snapshot {
        if let Some(bs) = snap.decode.get(&backend.label) {
            if let Some(d) = warmth_discount(bs.warmth.hit_strength(blocks)) {
                discounts.push(d);
            }
        }
    }
    const MAX_BARRIERS: usize = 16;
    let mut barriers = [BarrierFactor::clamped(1.0); MAX_BARRIERS];
    let mut len = 0;
    for barrier in extra_barriers {
        if len < MAX_BARRIERS {
            barriers[len] = *barrier;
            len += 1;
        }
    }
    if prefill_label != backend.label && len < MAX_BARRIERS {
        barriers[len] = BarrierFactor::clamped(transfer_penalty.max(1.0));
        len += 1;
    }
    backend.base_cost(&barriers[..len], &discounts)
}

fn joint_ln_scored(
    pf: &ScoredBackend,
    dc: &ScoredBackend,
    snapshot: Option<&StateSnapshot>,
    blocks: &[u64],
    transfer_penalty: f64,
    extra_barriers: &[BarrierFactor],
) -> f64 {
    joint_ln_targets(pf, dc, snapshot, blocks, transfer_penalty, extra_barriers)
}

/// Oracle joint pick for pairing-regret shadow measurement.
pub fn oracle_pair(
    prefill_pool: &[Arc<ScoredBackend>],
    decode_pool: &[Arc<ScoredBackend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    transfer_penalty: f64,
) -> (Arc<ScoredBackend>, Arc<ScoredBackend>) {
    let blocks = default_routing_blocks(prompt_tokens);
    let mut best: Option<(f64, Arc<ScoredBackend>, Arc<ScoredBackend>)> = None;
    for pf in prefill_pool {
        for dc in decode_pool {
            let ln = joint_ln_scored(pf, dc, snapshot, &blocks, transfer_penalty, &[]);
            if best.as_ref().is_none_or(|(b, _, _)| ln < *b) {
                best = Some((ln, Arc::clone(pf), Arc::clone(dc)));
            }
        }
    }
    let (_, pf, dc) = best.expect("non-empty pools");
    (pf, dc)
}

pub fn greedy_pair(
    prefill_pool: &[Arc<ScoredBackend>],
    decode_pool: &[Arc<ScoredBackend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    transfer_penalty: f64,
) -> (Arc<ScoredBackend>, Arc<ScoredBackend>) {
    let pf = select_prefill(prefill_pool, snapshot, prompt_tokens).expect("prefill");
    let dc =
        select_decode(&pf, decode_pool, snapshot, prompt_tokens, transfer_penalty).expect("decode");
    (pf, dc)
}

pub fn pairing_regret(
    prefill_pool: &[Arc<ScoredBackend>],
    decode_pool: &[Arc<ScoredBackend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    transfer_penalty: f64,
) -> f64 {
    pairing_regret_targets(
        prefill_pool,
        decode_pool,
        snapshot,
        prompt_tokens,
        transfer_penalty,
    )
}

/// Shadow pairing-regret monitor for any pairing target (router backends or scored pool).
pub fn pairing_regret_targets<T: PairingTarget + ?Sized>(
    prefill_pool: &[Arc<T>],
    decode_pool: &[Arc<T>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    transfer_penalty: f64,
) -> f64 {
    if prefill_pool.is_empty() || decode_pool.is_empty() {
        return 0.0;
    }
    let pf = select_prefill_target(prefill_pool, snapshot, prompt_tokens).expect("prefill");
    let dc = select_decode_target(
        pf.label(),
        decode_pool,
        snapshot,
        prompt_tokens,
        transfer_penalty,
        &[],
    )
    .expect("decode");
    let blocks = default_routing_blocks(prompt_tokens);
    let greedy_c = joint_ln_targets(
        pf.as_ref(),
        dc.as_ref(),
        snapshot,
        &blocks,
        transfer_penalty,
        &[],
    );
    let mut oracle_c = f64::INFINITY;
    for p in prefill_pool {
        for d in decode_pool {
            oracle_c = oracle_c.min(joint_ln_targets(
                p.as_ref(),
                d.as_ref(),
                snapshot,
                &blocks,
                transfer_penalty,
                &[],
            ));
        }
    }
    (greedy_c - oracle_c).exp() - 1.0
}

fn joint_ln_targets<T: PairingTarget + ?Sized>(
    pf: &T,
    dc: &T,
    snapshot: Option<&StateSnapshot>,
    blocks: &[u64],
    transfer_penalty: f64,
    extra_barriers: &[BarrierFactor],
) -> f64 {
    pf.prefill_ln(snapshot, blocks)
        + dc.decode_ln(
            snapshot,
            blocks,
            pf.label(),
            transfer_penalty,
            extra_barriers,
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scored(label: &str, cost: f64) -> Arc<ScoredBackend> {
        ScoredBackend::new(label, "127.0.0.1:1".parse().unwrap(), cost)
    }

    #[test]
    fn greedy_pairing_prefill_first() {
        let pf = [scored("pf0", 0.01), scored("pf1", 0.05)];
        let dc = [scored("dc0", 0.02), scored("dc1", 0.03)];
        let (g_pf, _) = greedy_pair(&pf, &dc, None, 2048, DEFAULT_TRANSFER_PENALTY);
        assert_eq!(g_pf.label, "pf0");
    }

    #[test]
    fn pairing_regret_within_budget() {
        let pf = [scored("pf0", 0.01), scored("pf1", 0.02)];
        let dc = [scored("dc0", 0.02), scored("dc1", 0.025)];
        let regret = pairing_regret(&pf, &dc, None, 512, 1.1);
        assert!((0.0..0.15).contains(&regret));
    }
}
