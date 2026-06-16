//! RDMA transfer cost shadow — log analytic vs observed transfer (Track A shadow). [DEMI-RDMA-COST-SHADOW]

use std::sync::Mutex;

/// One shadow sample for pf→dc KV transfer (routing still uses flat penalty).
#[derive(Debug, Clone, PartialEq)]
pub struct RdmaCostShadowSample {
    pub pf_label: String,
    pub dc_label: String,
    pub distance: u64,
    pub kv_bytes: u64,
    pub analytic_transfer_ln: f64,
    pub flat_penalty_ln: f64,
    pub observed_transfer_secs: f64,
}

#[derive(Debug, Default)]
pub struct RdmaCostShadowLog {
    inner: Mutex<Vec<RdmaCostShadowSample>>,
    max_samples: usize,
}

impl RdmaCostShadowLog {
    pub fn new(max_samples: usize) -> Self {
        Self {
            inner: Mutex::new(Vec::with_capacity(max_samples.min(4096))),
            max_samples: max_samples.max(1),
        }
    }

    pub fn record(&self, sample: RdmaCostShadowSample) {
        let mut buf = self.inner.lock().expect("rdma cost shadow lock");
        if buf.len() >= self.max_samples {
            buf.remove(0);
        }
        buf.push(sample);
    }

    pub fn samples(&self) -> Vec<RdmaCostShadowSample> {
        self.inner.lock().expect("rdma cost shadow lock").clone()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("rdma cost shadow lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Median observed/predicted transfer ratio (linear seconds from ln analytic).
pub fn eval_transfer_ratio_median(samples: &[RdmaCostShadowSample]) -> f64 {
    if samples.is_empty() {
        return 1.0;
    }
    let mut ratios = Vec::new();
    for s in samples {
        if s.observed_transfer_secs <= 0.0 || !s.analytic_transfer_ln.is_finite() {
            continue;
        }
        let predicted = s.analytic_transfer_ln.exp();
        if predicted <= 0.0 || !predicted.is_finite() {
            continue;
        }
        ratios.push((s.observed_transfer_secs / predicted).clamp(1e-6, 1e6));
    }
    if ratios.is_empty() {
        return 1.0;
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ratios[ratios.len() / 2]
}
