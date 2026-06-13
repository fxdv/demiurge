//! Length predictor for aggregate reservation. [DEMI-PAIR-GREEDY]

use std::collections::VecDeque;

use demiurge_cost::kv_breakdown;

const MAX_SAMPLES: usize = 512;

#[derive(Debug, Default)]
pub struct LengthPredictor {
    samples: VecDeque<u64>,
}

impl LengthPredictor {
    pub fn record(&mut self, prompt_tokens: u64) {
        if self.samples.len() >= MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(prompt_tokens);
    }

    pub fn p50(&self) -> u64 {
        percentile_at(&self.samples, 0.50)
    }

    pub fn p90(&self) -> u64 {
        percentile_at(&self.samples, 0.90)
    }

    pub fn p99(&self) -> u64 {
        percentile_at(&self.samples, 0.99)
    }

    /// Fleet-aggregate reservation bytes at p90 prompt length.
    pub fn reserve_bytes_p90(&self, bytes_per_token: u64) -> u64 {
        kv_breakdown(self.p90(), bytes_per_token).kv_reserved
    }
}

fn percentile_at(samples: &VecDeque<u64>, p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut v: Vec<u64> = samples.iter().copied().collect();
    v.sort_unstable();
    let idx = ((v.len() as f64 * p) as usize).min(v.len() - 1);
    v[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predictor_percentiles_ordered() {
        let mut p = LengthPredictor::default();
        for i in 1..=100 {
            p.record(i * 10);
        }
        assert!(p.p50() <= p.p90());
        assert!(p.p90() <= p.p99());
    }
}
