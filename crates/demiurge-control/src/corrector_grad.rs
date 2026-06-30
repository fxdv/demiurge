//! Corrector shadow → canary → production graduation state machine. [DEMI-CORR-GRAD]
//!
//! Promotion is one-way-gated and self-demoting: any `DEMI-CORR-CLAMP`
//! saturation or graduation-gate violation rolls the controller straight
//! back to `Shadow`, including out of `Production`. There is no "stuck"
//! state — every evaluated window either holds, promotes one stage, or
//! rolls all the way back to the safe (non-actuating) state.

use demiurge_cost::ALPHA;

use crate::corrector_shadow::{
    eval_goodput_improvement, train_bounded_delta, CorrectorShadowSample,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraduationStage {
    /// Logs `(features, ln C, t_obs)`; the trained delta never actuates.
    Shadow,
    /// Delta actuates on a bounded canary slice of traffic.
    Canary,
    /// Delta actuates fleet-wide; still continuously monitored.
    Production,
}

#[derive(Debug, Clone, Copy)]
pub struct GraduationGate {
    pub max_violations: u64,
    pub min_goodput_improvement: f64,
}

/// True once `delta` has reached (or would be pinned to) the
/// `DEMI-CORR-CLAMP` boundary `[1-alpha, 1+alpha]` — the structural signal
/// that the corrector is no longer making a free analytic choice and must
/// not be trusted to actuate further.
#[must_use]
pub fn is_clamp_saturated(delta: f64) -> bool {
    !delta.is_finite() || delta <= 1.0 - ALPHA || delta >= 1.0 + ALPHA
}

/// Graduate corrector to production only when clamp/C>0 violations are zero and goodput improves.
#[must_use]
pub fn eval_corrector_graduation(
    violations: u64,
    goodput_delta: f64,
    gate: &GraduationGate,
) -> bool {
    violations <= gate.max_violations && goodput_delta >= gate.min_goodput_improvement
}

/// Outcome of one graduation evaluation: the resulting stage plus whether
/// this step was a rollback (distinct from a hold, for alerting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraduationStep {
    pub stage: GraduationStage,
    pub rolled_back: bool,
}

/// Shadow/canary/production graduation controller. [DEMI-CORR-GRAD]
///
/// One window of evaluation = one call to [`Self::evaluate`]. Promotion
/// requires the trained delta to clear [`is_clamp_saturated`] *and*
/// [`eval_corrector_graduation`] against caller-supplied violations (e.g.
/// observed `DEMI-COST-POS` breaches); failing either demotes to `Shadow`
/// immediately, even from `Production`.
#[derive(Debug, Clone, Copy)]
pub struct GraduationController {
    stage: GraduationStage,
}

impl Default for GraduationController {
    fn default() -> Self {
        Self::new()
    }
}

impl GraduationController {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stage: GraduationStage::Shadow,
        }
    }

    #[must_use]
    pub const fn stage(self) -> GraduationStage {
        self.stage
    }

    /// Evaluate one window of shadow/canary/production `samples` against
    /// `gate`. `violations` is the count of invariant breaches the caller
    /// observed during the window (e.g. would-be `DEMI-COST-POS` failures);
    /// this function never invents them, only gates on them.
    pub fn evaluate(
        &mut self,
        samples: &[CorrectorShadowSample],
        violations: u64,
        gate: &GraduationGate,
    ) -> GraduationStep {
        let delta = train_bounded_delta(samples);
        let goodput = eval_goodput_improvement(samples, delta);
        let saturated = is_clamp_saturated(delta);
        let healthy = !saturated && eval_corrector_graduation(violations, goodput, gate);

        let next = match (self.stage, healthy) {
            (GraduationStage::Shadow, true) => GraduationStage::Canary,
            (GraduationStage::Canary, true) => GraduationStage::Production,
            (GraduationStage::Production, true) => GraduationStage::Production,
            (_, false) => GraduationStage::Shadow,
        };
        let rolled_back = !healthy && self.stage != GraduationStage::Shadow;
        self.stage = next;
        GraduationStep {
            stage: next,
            rolled_back,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> GraduationGate {
        GraduationGate {
            max_violations: 0,
            min_goodput_improvement: 0.01,
        }
    }

    fn clean_sample(analytic_ln: f64, observed_us: u64) -> CorrectorShadowSample {
        CorrectorShadowSample {
            prompt_tokens: 512,
            analytic_ln,
            observed_us,
            pool_pi: 0.5,
            backend_label: "dc0".into(),
        }
    }

    /// Predicted ~1.0s, observed consistently ~1.05s: delta ≈ 1.05, within
    /// [0.8, 1.2], and correcting reduces error vs identity.
    fn healthy_samples() -> Vec<CorrectorShadowSample> {
        vec![
            clean_sample(0.0, 1_050_000),
            clean_sample(0.0, 1_040_000),
            clean_sample(0.0, 1_060_000),
        ]
    }

    /// Observed ~10x predicted: median ratio is far outside [0.8, 1.2], so
    /// `train_bounded_delta` clamps to the `DEMI-CORR-CLAMP` boundary.
    fn saturating_samples() -> Vec<CorrectorShadowSample> {
        vec![
            clean_sample(0.0, 10_000_000),
            clean_sample(0.0, 10_200_000),
            clean_sample(0.0, 9_800_000),
        ]
    }

    #[test]
    fn corrector_graduation_requires_zero_violations_and_goodput() {
        let g = gate();
        assert!(eval_corrector_graduation(0, 0.02, &g));
        assert!(!eval_corrector_graduation(1, 0.02, &g));
        assert!(!eval_corrector_graduation(0, 0.0, &g));
    }

    #[test]
    fn is_clamp_saturated_covers_boundary_and_invalid_values() {
        assert!(!is_clamp_saturated(1.0));
        assert!(!is_clamp_saturated(0.81));
        assert!(!is_clamp_saturated(1.19));
        assert!(is_clamp_saturated(0.8));
        assert!(is_clamp_saturated(1.2));
        assert!(is_clamp_saturated(5.0));
        assert!(is_clamp_saturated(f64::NAN));
        assert!(is_clamp_saturated(f64::INFINITY));
    }

    #[test]
    fn healthy_window_promotes_shadow_to_canary() {
        let mut ctrl = GraduationController::new();
        assert_eq!(ctrl.stage(), GraduationStage::Shadow);
        let step = ctrl.evaluate(&healthy_samples(), 0, &gate());
        assert_eq!(step.stage, GraduationStage::Canary);
        assert!(!step.rolled_back);
    }

    #[test]
    fn two_healthy_windows_reach_production() {
        let mut ctrl = GraduationController::new();
        ctrl.evaluate(&healthy_samples(), 0, &gate());
        let step = ctrl.evaluate(&healthy_samples(), 0, &gate());
        assert_eq!(step.stage, GraduationStage::Production);
        assert!(!step.rolled_back);
    }

    #[test]
    fn production_holds_under_continued_health() {
        let mut ctrl = GraduationController::new();
        ctrl.evaluate(&healthy_samples(), 0, &gate());
        ctrl.evaluate(&healthy_samples(), 0, &gate());
        assert_eq!(ctrl.stage(), GraduationStage::Production);
        let step = ctrl.evaluate(&healthy_samples(), 0, &gate());
        assert_eq!(step.stage, GraduationStage::Production);
        assert!(!step.rolled_back);
    }

    #[test]
    fn violation_during_canary_rolls_back_to_shadow() {
        let mut ctrl = GraduationController::new();
        ctrl.evaluate(&healthy_samples(), 0, &gate());
        assert_eq!(ctrl.stage(), GraduationStage::Canary);
        let step = ctrl.evaluate(&healthy_samples(), 1, &gate());
        assert_eq!(step.stage, GraduationStage::Shadow);
        assert!(step.rolled_back);
    }

    #[test]
    fn violation_during_production_rolls_back_to_shadow() {
        let mut ctrl = GraduationController::new();
        ctrl.evaluate(&healthy_samples(), 0, &gate());
        ctrl.evaluate(&healthy_samples(), 0, &gate());
        assert_eq!(ctrl.stage(), GraduationStage::Production);
        let step = ctrl.evaluate(&healthy_samples(), 1, &gate());
        assert_eq!(step.stage, GraduationStage::Shadow);
        assert!(step.rolled_back);
    }

    #[test]
    fn saturated_delta_blocks_promotion_even_with_zero_violations() {
        let mut ctrl = GraduationController::new();
        let step = ctrl.evaluate(&saturating_samples(), 0, &gate());
        assert_eq!(step.stage, GraduationStage::Shadow);
        assert!(
            !step.rolled_back,
            "never promoted, so this is a hold, not a rollback"
        );
    }

    #[test]
    fn saturation_in_canary_rolls_back_even_though_goodput_looks_fine() {
        let mut ctrl = GraduationController::new();
        ctrl.evaluate(&healthy_samples(), 0, &gate());
        assert_eq!(ctrl.stage(), GraduationStage::Canary);
        let step = ctrl.evaluate(&saturating_samples(), 0, &gate());
        assert_eq!(step.stage, GraduationStage::Shadow);
        assert!(step.rolled_back);
    }

    #[test]
    fn hold_at_shadow_is_not_reported_as_rollback() {
        let mut ctrl = GraduationController::new();
        let step = ctrl.evaluate(&[], 0, &gate());
        assert_eq!(step.stage, GraduationStage::Shadow);
        assert!(!step.rolled_back);
    }
}
