//! Greedy pf→dc pairing (documented approximation). [DEMI-PAIR-GREEDY]

use std::sync::Arc;

use demiurge_cost::{warmth_discount, BarrierFactor, Cost};
use demiurge_state::{default_routing_blocks, StateSnapshot};

use crate::scored::ScoredBackend;

/// Prefill target fixed first; decode optimized conditional on pf.
pub fn select_prefill(
    candidates: &[Arc<ScoredBackend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
) -> Option<Arc<ScoredBackend>> {
    let blocks = default_routing_blocks(prompt_tokens);
    candidates
        .iter()
        .min_by(|a, b| {
            prefill_cost(a, snapshot, &blocks)
                .ln()
                .total_cmp(&prefill_cost(b, snapshot, &blocks).ln())
        })
        .cloned()
}

pub fn select_decode(
    prefill: &ScoredBackend,
    candidates: &[Arc<ScoredBackend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    transfer_penalty: f64,
) -> Option<Arc<ScoredBackend>> {
    let blocks = default_routing_blocks(prompt_tokens);
    candidates
        .iter()
        .min_by(|a, b| {
            decode_cost(a, snapshot, &blocks, prefill, transfer_penalty)
                .ln()
                .total_cmp(&decode_cost(b, snapshot, &blocks, prefill, transfer_penalty).ln())
        })
        .cloned()
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
    prefill: &ScoredBackend,
    transfer_penalty: f64,
) -> Cost {
    let mut discounts = Vec::new();
    if let Some(snap) = snapshot {
        if let Some(bs) = snap.decode.get(&backend.label) {
            if let Some(d) = warmth_discount(bs.warmth.hit_strength(blocks)) {
                discounts.push(d);
            }
        }
    }
    let transfer = if prefill.label == backend.label {
        BarrierFactor::clamped(1.0)
    } else {
        BarrierFactor::clamped(transfer_penalty.max(1.0))
    };
    backend.base_cost(&[transfer], &discounts)
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
            let ln = joint_ln(pf, dc, snapshot, &blocks, transfer_penalty);
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
    let (g_pf, g_dc) = greedy_pair(
        prefill_pool,
        decode_pool,
        snapshot,
        prompt_tokens,
        transfer_penalty,
    );
    let (o_pf, o_dc) = oracle_pair(
        prefill_pool,
        decode_pool,
        snapshot,
        prompt_tokens,
        transfer_penalty,
    );
    let blocks = default_routing_blocks(prompt_tokens);
    let greedy_c = joint_ln(&g_pf, &g_dc, snapshot, &blocks, transfer_penalty);
    let oracle_c = joint_ln(&o_pf, &o_dc, snapshot, &blocks, transfer_penalty);
    (greedy_c - oracle_c).exp() - 1.0
}

fn joint_ln(
    pf: &ScoredBackend,
    dc: &ScoredBackend,
    snapshot: Option<&StateSnapshot>,
    blocks: &[u64],
    transfer_penalty: f64,
) -> f64 {
    prefill_cost(pf, snapshot, blocks).ln()
        + decode_cost(dc, snapshot, blocks, pf, transfer_penalty).ln()
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
        let (g_pf, _) = greedy_pair(&pf, &dc, None, 2048, 1.05);
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
