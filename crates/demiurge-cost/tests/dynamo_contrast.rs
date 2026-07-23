//! Competitive contrast: Dynamo-style linear block cost vs Demiurge fail-expensive.
//!
//! NVIDIA Dynamo's KV router (Routing Concepts) ranks workers with a linear
//! block score roughly:
//!
//! ```text
//! cost = prefill_load_scale * max(active_prefill + incoming − overlap_credits, 0)
//!      + active_decode + incoming_decode
//! ```
//!
//! That is a strong TTFT/load heuristic, but it is not a fail-expensive algebra:
//! unbounded or non-finite overlap credits can collapse or poison ranking.
//! These tests pin that contrast in CI so the moat stays measurable.
//! [DEMI-FAIL-EXPENSIVE] [DEMI-COST-POS] [DEMI-WARM-DISCOUNT]

use demiurge_cost::*;
use proptest::prelude::*;

/// Foil — Dynamo KV-router block cost shape (not used in production).
fn dynamo_block_cost(
    prefill_load_scale: f64,
    active_prefill: f64,
    incoming_prompt: f64,
    overlap_credits: f64,
    active_decode: f64,
    incoming_decode: f64,
) -> f64 {
    let adjusted = (active_prefill + incoming_prompt - overlap_credits).max(0.0);
    prefill_load_scale * adjusted + active_decode + incoming_decode
}

fn argmin2(a: f64, b: f64) -> Option<usize> {
    if !a.is_finite() && !b.is_finite() {
        return None;
    }
    if !a.is_finite() {
        return Some(1);
    }
    if !b.is_finite() {
        return Some(0);
    }
    Some(usize::from(b < a))
}

/// Forged / unbounded overlap credits can zero the prefill term and undercut a
/// healthy worker that has less cache but honest load.
#[test]
fn dynamo_unbounded_overlap_can_undercut_healthy_worker() {
    let scale = 1.0;
    // Healthy: modest overlap, light decode load.
    let healthy = dynamo_block_cost(scale, 0.0, 100.0, 20.0, 10.0, 5.0);
    // Poisoned: claims more overlap than the prompt has → prefill term vanishes.
    let poisoned = dynamo_block_cost(scale, 0.0, 100.0, 10_000.0, 40.0, 5.0);
    assert!(
        poisoned < healthy,
        "unbounded overlap must be able to undercut healthy (poisoned={poisoned} healthy={healthy})"
    );
    assert_eq!(argmin2(healthy, poisoned), Some(1));
}

/// Non-finite overlap credits are dangerous in the linear foil: Rust's
/// `f64::max` returns the non-NaN argument, so `max(NaN, 0)` becomes `0` and
/// the prefill term vanishes (fail-*open* to free prefill). Demiurge clamps
/// invalid warmth to neutral and never undercuts a valid hit.
#[test]
fn dynamo_nan_overlap_poisons_rank_demiurge_stays_finite() {
    let scale = 1.0;
    let healthy = dynamo_block_cost(scale, 0.0, 80.0, 10.0, 8.0, 4.0);
    let nan_cost = dynamo_block_cost(scale, 0.0, 80.0, f64::NAN, 8.0, 4.0);
    assert!(
        nan_cost.is_finite(),
        "foil uses max(·,0): NaN overlap collapses to finite free-prefill"
    );
    assert!(
        (nan_cost - 12.0).abs() < 1e-9,
        "NaN overlap must erase prefill (got {nan_cost})"
    );
    assert!(
        nan_cost < healthy,
        "NaN overlap undercuts honest credit (nan={nan_cost} healthy={healthy})"
    );
    assert_eq!(argmin2(healthy, nan_cost), Some(1));

    let id = Corrector::identity();
    let demi_healthy = compose(
        TimeCore::new(1.0).unwrap(),
        &[BarrierFactor::new(1.0 + 8.0).unwrap()],
        &[warmth_discount(0.25).unwrap()],
        id,
    );
    let demi_broken_warmth = compose(
        TimeCore::new(1.0).unwrap(),
        &[BarrierFactor::new(1.0 + 8.0).unwrap()],
        &[Discount::clamped(f64::NAN)],
        id,
    );
    assert!(demi_healthy.ln().is_finite());
    assert!(demi_broken_warmth.ln().is_finite());
    assert!(
        demi_broken_warmth.ln() >= demi_healthy.ln(),
        "invalid warmth must not cheapen a valid hit"
    );
}

/// Demiurge ρ_max caps warmth; a "full hit" cannot erase the time core the way
/// unbounded Dynamo overlap credits can erase the prefill term.
#[test]
fn demiurge_rho_max_caps_warmth_unlike_unbounded_overlap() {
    let core = TimeCore::new(2.0).unwrap();
    let id = Corrector::identity();
    let no_warmth = compose(core, &[], &[], id);
    let full_hit = compose(core, &[], &[warmth_discount(1.0).unwrap()], id);
    let expected_factor = 1.0 - WARMTH_MAX_DISCOUNT;
    assert!((warmth_discount(1.0).unwrap().get() - expected_factor).abs() < 1e-12);
    // Full warmth multiplies cost by (1-ρ_max), never by ~0.
    let ratio = (full_hit.ln() - no_warmth.ln()).exp();
    assert!(
        (ratio - expected_factor).abs() < 1e-9,
        "full hit must apply exactly ρ_max envelope (ratio={ratio})"
    );
    assert!(full_hit.is_positive());
    assert!(full_hit.get() > expected_factor); // core=2 → cost = 2*(1-ρ_max) > 1-ρ_max
}

proptest! {
    /// [DEMI-WARM-DISCOUNT] — for any finite strength, the discount factor stays
    /// in (1-ρ_max, 1] (or None on miss); never below the envelope.
    #[test]
    fn warmth_respects_rho_max_envelope(strength in -1e3f64..1e3) {
        match warmth_discount(strength) {
            None => prop_assert!(!strength.is_finite() || strength <= 0.0),
            Some(d) => {
                let v = d.get();
                prop_assert!(v.is_finite());
                prop_assert!(v > 0.0 && v <= 1.0);
                prop_assert!(
                    v + 1e-12 >= 1.0 - WARMTH_MAX_DISCOUNT,
                    "warmth factor {v} below 1-ρ_max"
                );
            }
        }
    }

    /// [DEMI-FAIL-EXPENSIVE] — service_cost with broken core/barrier/discount
    /// never undercuts the same inputs with healthy substitutes.
    #[test]
    fn service_cost_broken_signals_never_undercut_healthy(
        core in 1e-6f64..1e3,
        inflight in 0usize..64,
        warmth in 0.0f64..=1.0,
    ) {
        let discounts: Vec<Discount> = warmth_discount(warmth).into_iter().collect();
        let healthy = service_cost(core, inflight, &[], &discounts);

        let broken_core = service_cost(f64::NAN, inflight, &[], &discounts);
        prop_assert!(broken_core.ln() >= healthy.ln());

        let broken_phi = service_cost(
            core,
            inflight,
            &[BarrierFactor::clamped(f64::NAN)],
            &discounts,
        );
        prop_assert!(broken_phi.ln() >= healthy.ln());

        let broken_discount = service_cost(core, inflight, &[], &[Discount::clamped(f64::NAN)]);
        // Invalid discount is neutral — may match a miss, never undercut a hit.
        if warmth > 0.0 {
            prop_assert!(broken_discount.ln() > healthy.ln() - 1e-12);
        } else {
            prop_assert!((broken_discount.ln() - healthy.ln()).abs() < 1e-9);
        }
        prop_assert!(healthy.ln().is_finite() && broken_core.ln().is_finite());
    }

    /// Foil property: for any credit ≥ incoming+active prefill, Dynamo prefill
    /// term is zero — load on the decode side alone decides rank. Demiurge
    /// warmth cannot zero the time core.
    #[test]
    fn dynamo_overlap_can_zero_prefill_demiurge_cannot(
        incoming in 1.0f64..500.0,
        decode in 1.0f64..100.0,
        scale in 0.5f64..2.0,
    ) {
        let zeroed = dynamo_block_cost(scale, 0.0, incoming, incoming + 1.0, decode, 0.0);
        prop_assert!(
            (zeroed - decode).abs() < 1e-9,
            "Dynamo foil must be able to erase prefill via overlap (got {zeroed})"
        );

        let demi = service_cost(incoming, decode as usize, &[], &[warmth_discount(1.0).unwrap()]);
        let demi_bare = service_cost(incoming, decode as usize, &[], &[]);
        prop_assert!(demi.is_positive() && demi.ln().is_finite());
        // Full warmth shrinks cost by ρ_max only — still tracks the core.
        let shrink = (demi.ln() - demi_bare.ln()).exp();
        prop_assert!((shrink - (1.0 - WARMTH_MAX_DISCOUNT)).abs() < 1e-9);
    }
}
