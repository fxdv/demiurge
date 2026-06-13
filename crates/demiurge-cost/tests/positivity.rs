use demiurge_cost::*;
use proptest::prelude::*;

proptest! {
    // [DEMI-COST-POS] — cost is strictly positive across the whole factor domain.
    #[test]
    fn cost_strictly_positive(
        core in 1e-6f64..1e6,
        b1 in 1.0f64..100.0,
        b2 in 1.0f64..100.0,
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
        prop_assert!(cost.is_positive(), "cost={}", cost.get());
    }

    // [DEMI-CORR-CLAMP] — the corrector adjusts cost by at most a factor of
    // (1±α), no matter how extreme the raw delta a learner proposes.
    #[test]
    fn corrector_multiplier_bounded(
        core in 1e-6f64..1e6,
        b in 1.0f64..100.0,
        d in f64::MIN_POSITIVE..=1.0,
        raw_delta in -10.0f64..10.0,
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
        let ratio_ln = (adj.get() / base.get()).ln().abs();
        let bound = (1.0 + ALPHA).ln().abs().max((1.0 - ALPHA).ln().abs());
        prop_assert!(ratio_ln <= bound + 1e-9, "ratio_ln={ratio_ln} bound={bound}");
    }
}
