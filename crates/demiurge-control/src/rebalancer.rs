//! Dynamic pool rebalancing (shadow mode default). [DEMI-POOL-RATIO]

use std::time::{Duration, Instant};

use demiurge_cost::{
    POOL_REBALANCE_COOLDOWN_S, POOL_REBALANCE_HYSTERESIS, POOL_WEIGHT_KV, POOL_WEIGHT_QUEUE,
    POOL_WEIGHT_SLO,
};

use crate::pressure::PoolPressure;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebalancerMode {
    Shadow,
    CanActuate,
}

#[derive(Debug)]
pub struct PoolRebalancer {
    pi: f64,
    last_change: Instant,
    mode: RebalancerMode,
}

impl Default for PoolRebalancer {
    fn default() -> Self {
        Self {
            pi: 0.5,
            last_change: Instant::now()
                .checked_sub(Duration::from_secs(POOL_REBALANCE_COOLDOWN_S + 1))
                .unwrap_or_else(Instant::now),
            mode: RebalancerMode::Shadow,
        }
    }
}

impl PoolRebalancer {
    pub fn new(mode: RebalancerMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    pub fn pi(&self) -> f64 {
        self.pi
    }

    pub fn compute_pi_star(&self, signals: &PoolPressure) -> f64 {
        let demand_prefill = POOL_WEIGHT_QUEUE * signals.q_prefill
            + POOL_WEIGHT_SLO * signals.slo_prefill * (1.0 - signals.fp_share);
        let demand_decode = POOL_WEIGHT_QUEUE * signals.q_decode
            + POOL_WEIGHT_KV * signals.kv_decode
            + POOL_WEIGHT_SLO * signals.slo_decode;
        let total = demand_prefill + demand_decode;
        if total <= 0.0 {
            return self.pi;
        }
        (demand_prefill / total).clamp(0.0, 1.0)
    }

    /// Returns new `π` when hysteresis + cooldown satisfied; shadow never actuates.
    pub fn maybe_update(&mut self, signals: &PoolPressure) -> Option<f64> {
        let pi_star = self.compute_pi_star(signals);
        if (pi_star - self.pi).abs() <= POOL_REBALANCE_HYSTERESIS {
            return None;
        }
        if self.last_change.elapsed() < Duration::from_secs(POOL_REBALANCE_COOLDOWN_S) {
            return None;
        }
        if self.mode == RebalancerMode::Shadow {
            return None;
        }
        self.pi = pi_star;
        self.last_change = Instant::now();
        Some(self.pi)
    }

    pub fn shadow_pi_star(&self, signals: &PoolPressure) -> f64 {
        self.compute_pi_star(signals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebalance_respects_hysteresis_and_cooldown() {
        let mut r = PoolRebalancer::new(RebalancerMode::CanActuate);
        let small = PoolPressure {
            q_prefill: 0.51,
            q_decode: 0.49,
            ..Default::default()
        };
        assert!(r.maybe_update(&small).is_none());
        let big = PoolPressure {
            q_prefill: 0.95,
            q_decode: 0.05,
            ..Default::default()
        };
        assert!(r.maybe_update(&big).is_some());
        assert!(r.maybe_update(&big).is_none());
    }

    #[test]
    fn shadow_mode_never_actuates() {
        let mut r = PoolRebalancer::new(RebalancerMode::Shadow);
        let heavy = PoolPressure {
            q_prefill: 0.99,
            q_decode: 0.01,
            ..Default::default()
        };
        assert!(r.maybe_update(&heavy).is_none());
        assert!(r.shadow_pi_star(&heavy) > 0.8);
    }
}
