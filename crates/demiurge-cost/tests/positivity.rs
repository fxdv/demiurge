use demiurge_cost::*;
use proptest::prelude::*;

proptest! {
    // [DEMI-COST-POS] — cost is strictly positive across the whole factor
    // domain, including the floating-point underflow edge (tiny discounts) that
    // sank the old linear-space product.
    #[test]
    fn cost_strictly_positive(
        core in 1e-9f64..1e9,
        b1 in 1.0f64..1e3,
        b2 in 1.0f64..1e3,
        d1 in f64::MIN_POSITIVE..=1.0,
        d2 in f64::MIN_POSITIVE..=1.0,
        delta in (1.0 - ALPHA)..=(1.0 + ALPHA),
    ) {
        let cost = compose(
            TimeCore::new(core).unwrap(),
            &[BarrierFactor::new(b1).unwrap(), BarrierFactor::new(b2).unwrap()],
            &[Discount::new(d1).unwrap(), Discount::new(d2).unwrap()],
            Corrector::new(delta),
        );
        prop_assert!(cost.is_positive());
        prop_assert!(cost.ln().is_finite(), "ln={}", cost.ln());
    }

    // [DEMI-COST-POS] — even with a long run of near-zero discounts (which would
    // flush a linear product to 0.0) and large barriers (which would overflow
    // it to +inf), the log-space cost stays finite and positive, and ordering
    // is preserved: more discount => strictly smaller ln.
    #[test]
    fn cost_log_is_finite_at_extremes(n in 1usize..256) {
        let core = TimeCore::new(1.0).unwrap();
        let tiny: Vec<Discount> = (0..n)
            .map(|_| Discount::new(f64::MIN_POSITIVE).unwrap())
            .collect();
        let huge: Vec<BarrierFactor> = (0..n)
            .map(|_| BarrierFactor::new(1e300).unwrap())
            .collect();

        let cheap = compose(core, &[], &tiny, Corrector::identity());
        let expensive = compose(core, &huge, &[], Corrector::identity());

        prop_assert!(cheap.is_positive() && cheap.ln().is_finite());
        prop_assert!(expensive.is_positive() && expensive.ln().is_finite());
        prop_assert!(cheap.ln() < expensive.ln());
    }

    // [DEMI-CORR-CLAMP] — the corrector shifts log-cost by exactly ln(delta)
    // with delta in [1-alpha, 1+alpha], so it can adjust cost by at most a
    // bounded multiplicative envelope, no matter how extreme the raw delta.
    #[test]
    fn corrector_multiplier_bounded(
        core in 1e-9f64..1e9,
        b in 1.0f64..1e3,
        d in f64::MIN_POSITIVE..=1.0,
        raw_delta in -1e6f64..1e6,
    ) {
        let base = compose(
            TimeCore::new(core).unwrap(),
            &[BarrierFactor::new(b).unwrap()],
            &[Discount::new(d).unwrap()],
            Corrector::identity(),
        );
        let adj = compose(
            TimeCore::new(core).unwrap(),
            &[BarrierFactor::new(b).unwrap()],
            &[Discount::new(d).unwrap()],
            Corrector::new(raw_delta),
        );
        let shift = (adj.ln() - base.ln()).abs();
        let bound = (1.0 + ALPHA).ln().abs().max((1.0 - ALPHA).ln().abs());
        prop_assert!(shift <= bound + 1e-9, "shift={shift} bound={bound}");
    }
}

// [DEMI-FAIL-EXPENSIVE] — a broken signal can only ever make a target look more
// expensive (or neutral), never cheaper. A NaN latency must not undercut a
// valid fast target; an invalid discount must not grant an unearned reward.
#[test]
fn invalid_signal_never_cheapens() {
    let id = Corrector::identity();

    let valid_fast = compose(TimeCore::new(0.001).unwrap(), &[], &[], id);
    let broken_core = compose(TimeCore::clamped(f64::NAN), &[], &[], id);
    assert!(
        broken_core.ln() >= valid_fast.ln(),
        "NaN latency cheapened a target"
    );

    let core = TimeCore::new(1.0).unwrap();
    let neutral = compose(core, &[], &[], id);
    let broken_discount = compose(core, &[], &[Discount::clamped(f64::NAN)], id);
    assert_eq!(
        broken_discount.ln(),
        neutral.ln(),
        "invalid discount granted a reward"
    );

    let broken_barrier = compose(core, &[BarrierFactor::clamped(f64::NAN)], &[], id);
    assert!(
        broken_barrier.ln() >= neutral.ln(),
        "invalid barrier reduced cost"
    );
}
