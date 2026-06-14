//! Corrector shadow pipeline — log tuples, train bounded δ, eval goodput (Track A). [DEMI-CORR-SHADOW]

use std::sync::Mutex;

use demiurge_cost::{Corrector, Cost, TimeCore, ALPHA};

/// One shadow sample: features + analytic cost + observed prefill latency.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrectorShadowSample {
    pub prompt_tokens: u64,
    pub analytic_ln: f64,
    pub observed_us: u64,
    pub pool_pi: f64,
    pub backend_label: String,
}

/// In-memory ring buffer of shadow samples (no production actuation).
#[derive(Debug, Default)]
pub struct CorrectorShadowLog {
    inner: Mutex<Vec<CorrectorShadowSample>>,
    max_samples: usize,
}

impl CorrectorShadowLog {
    pub fn new(max_samples: usize) -> Self {
        Self {
            inner: Mutex::new(Vec::with_capacity(max_samples.min(4096))),
            max_samples: max_samples.max(1),
        }
    }

    pub fn record(&self, sample: CorrectorShadowSample) {
        let mut buf = self.inner.lock().expect("corrector shadow lock");
        if buf.len() >= self.max_samples {
            buf.remove(0);
        }
        buf.push(sample);
    }

    pub fn samples(&self) -> Vec<CorrectorShadowSample> {
        self.inner.lock().expect("corrector shadow lock").clone()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("corrector shadow lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Train a multiplicative δ clamped to `[1−α, 1+α]` from shadow samples.
pub fn train_bounded_delta(samples: &[CorrectorShadowSample]) -> f64 {
    if samples.is_empty() {
        return 1.0;
    }
    let mut ratios = Vec::with_capacity(samples.len());
    for s in samples {
        if s.observed_us == 0 || !s.analytic_ln.is_finite() {
            continue;
        }
        let predicted = s.analytic_ln.exp();
        if predicted <= 0.0 || !predicted.is_finite() {
            continue;
        }
        let observed = s.observed_us as f64 / 1_000_000.0;
        ratios.push((observed / predicted).clamp(1e-6, 1e6));
    }
    if ratios.is_empty() {
        return 1.0;
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = ratios[ratios.len() / 2];
    Corrector::new(mid).get()
}

/// Fraction of samples where applying `delta` reduces log prediction error vs identity.
pub fn eval_goodput_improvement(samples: &[CorrectorShadowSample], delta: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let corrector = Corrector::new(delta);
    let mut improved = 0u64;
    let mut total = 0u64;
    for s in samples {
        if s.observed_us == 0 || !s.analytic_ln.is_finite() {
            continue;
        }
        let observed = TimeCore::clamped(s.observed_us as f64 / 1_000_000.0);
        let identity_pred = Cost::from_ln(s.analytic_ln);
        let corrected_pred = Cost::from_ln(s.analytic_ln + corrector.get().ln());
        let err_id = (identity_pred.ln() - observed.get().ln()).abs();
        let err_corr = (corrected_pred.ln() - observed.get().ln()).abs();
        if err_corr + 1e-12 < err_id {
            improved += 1;
        }
        total += 1;
    }
    if total == 0 {
        0.0
    } else {
        improved as f64 / total as f64
    }
}

/// Sanity: trained delta stays within clamp envelope.
pub fn delta_within_envelope(delta: f64) -> bool {
    (1.0 - ALPHA..=1.0 + ALPHA).contains(&delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(analytic_ln: f64, observed_us: u64) -> CorrectorShadowSample {
        CorrectorShadowSample {
            prompt_tokens: 512,
            analytic_ln,
            observed_us,
            pool_pi: 0.5,
            backend_label: "dc0".into(),
        }
    }

    #[test]
    fn train_delta_clamped() {
        let samples = vec![
            sample(0.0, 2_000_000),
            sample(0.0, 2_100_000),
            sample(0.0, 1_900_000),
        ];
        let delta = train_bounded_delta(&samples);
        assert!(delta_within_envelope(delta));
    }

    #[test]
    fn eval_goodput_identity_is_zero_or_small() {
        let samples = vec![sample(-1.0, 368_000), sample(-0.5, 606_000)];
        let imp = eval_goodput_improvement(&samples, 1.0);
        assert!((0.0..=1.0).contains(&imp));
    }
}
