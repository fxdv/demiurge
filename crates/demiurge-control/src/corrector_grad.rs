//! Corrector shadow → production graduation gate (Track C scaffolding). [DEMI-CORR-GRAD]

#[derive(Debug, Clone, Copy)]
pub struct GraduationGate {
    pub max_violations: u64,
    pub min_goodput_improvement: f64,
}

/// Graduate corrector to production only when clamp/C>0 violations are zero and goodput improves.
pub fn eval_corrector_graduation(
    violations: u64,
    goodput_delta: f64,
    gate: &GraduationGate,
) -> bool {
    violations <= gate.max_violations && goodput_delta >= gate.min_goodput_improvement
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrector_graduation_requires_zero_violations_and_goodput() {
        let gate = GraduationGate {
            max_violations: 0,
            min_goodput_improvement: 0.01,
        };
        assert!(eval_corrector_graduation(0, 0.02, &gate));
        assert!(!eval_corrector_graduation(1, 0.02, &gate));
        assert!(!eval_corrector_graduation(0, 0.0, &gate));
    }
}
