//! Fleet pilot shadow replay — π* vs prefill-heavy windows on held-out trace. [DEMI-FLEET-SHADOW]

use crate::pressure::PoolPressure;
use crate::rebalancer::{PoolRebalancer, RebalancerMode};

/// One time-window row from a production or synthetic trace file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceWindow {
    pub ts_ms: u64,
    pub q_prefill: f64,
    pub q_decode: f64,
    pub kv_decode: f64,
    pub slo_prefill: f64,
    pub slo_decode: f64,
    pub fp_share: f64,
    pub prefill_heavy: bool,
    /// `true` = held-out evaluation split; `false` = train/calibration.
    pub held_out: bool,
}

impl TraceWindow {
    pub fn pressure(&self) -> PoolPressure {
        PoolPressure {
            q_prefill: self.q_prefill.clamp(0.0, 1.0),
            q_decode: self.q_decode.clamp(0.0, 1.0),
            kv_decode: self.kv_decode.clamp(0.0, 1.0),
            slo_prefill: self.slo_prefill.clamp(0.0, 1.0),
            slo_decode: self.slo_decode.clamp(0.0, 1.0),
            fp_share: self.fp_share.clamp(0.0, 1.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowReplay {
    pub ts_ms: u64,
    pub pi_star: f64,
    pub prefill_heavy: bool,
    pub held_out: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FleetPilotReport {
    pub train_windows: usize,
    pub heldout_windows: usize,
    pub heldout_correlation: f64,
    pub heldout_mean_pi_heavy: f64,
    pub heldout_mean_pi_light: f64,
    pub gate_pass: bool,
    pub replays: Vec<WindowReplay>,
}

/// Point-biserial correlation between continuous π* and binary prefill-heavy label.
pub fn point_biserial_corr(values: &[f64], labels: &[bool]) -> f64 {
    if values.len() != labels.len() || values.len() < 2 {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean_all = values.iter().sum::<f64>() / n;
    let mut n1 = 0u64;
    let mut n0 = 0u64;
    let mut sum1 = 0.0;
    let mut sum0 = 0.0;
    let mut sq = 0.0;
    for (&v, &lab) in values.iter().zip(labels.iter()) {
        sq += (v - mean_all).powi(2);
        if lab {
            n1 += 1;
            sum1 += v;
        } else {
            n0 += 1;
            sum0 += v;
        }
    }
    if n1 == 0 || n0 == 0 {
        return 0.0;
    }
    let mean1 = sum1 / n1 as f64;
    let mean0 = sum0 / n0 as f64;
    let std = (sq / n).sqrt();
    if std <= 1e-12 {
        return 0.0;
    }
    let p = n1 as f64 / n;
    let q = 1.0 - p;
    ((mean1 - mean0) / std) * (p * q).sqrt()
}

/// Shadow replay: compute π* per window; gate on held-out correlation + separation.
pub fn replay_fleet_pilot(windows: &[TraceWindow], min_correlation: f64) -> FleetPilotReport {
    let rebalancer = PoolRebalancer::new(RebalancerMode::Shadow);
    let mut replays = Vec::with_capacity(windows.len());
    let mut train = 0usize;
    let mut heldout = 0usize;

    for w in windows {
        if w.held_out {
            heldout += 1;
        } else {
            train += 1;
        }
        let pi_star = rebalancer.shadow_pi_star(&w.pressure());
        replays.push(WindowReplay {
            ts_ms: w.ts_ms,
            pi_star,
            prefill_heavy: w.prefill_heavy,
            held_out: w.held_out,
        });
    }

    let held: Vec<_> = replays.iter().filter(|r| r.held_out).collect();
    let values: Vec<f64> = held.iter().map(|r| r.pi_star).collect();
    let labels: Vec<bool> = held.iter().map(|r| r.prefill_heavy).collect();
    let corr = point_biserial_corr(&values, &labels);

    let (mut sum_heavy, mut n_heavy) = (0.0, 0u64);
    let (mut sum_light, mut n_light) = (0.0, 0u64);
    for r in &held {
        if r.prefill_heavy {
            sum_heavy += r.pi_star;
            n_heavy += 1;
        } else {
            sum_light += r.pi_star;
            n_light += 1;
        }
    }
    let mean_heavy = if n_heavy > 0 {
        sum_heavy / n_heavy as f64
    } else {
        0.0
    };
    let mean_light = if n_light > 0 {
        sum_light / n_light as f64
    } else {
        0.0
    };

    let separation = mean_heavy > mean_light + 0.05;
    let gate_pass = corr >= min_correlation && separation && heldout > 0;

    FleetPilotReport {
        train_windows: train,
        heldout_windows: heldout,
        heldout_correlation: corr,
        heldout_mean_pi_heavy: mean_heavy,
        heldout_mean_pi_light: mean_light,
        gate_pass,
        replays,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heavy(ts: u64, held_out: bool) -> TraceWindow {
        TraceWindow {
            ts_ms: ts,
            q_prefill: 0.92,
            q_decode: 0.18,
            kv_decode: 0.25,
            slo_prefill: 0.15,
            slo_decode: 0.05,
            fp_share: 0.12,
            prefill_heavy: true,
            held_out,
        }
    }

    fn light(ts: u64, held_out: bool) -> TraceWindow {
        TraceWindow {
            ts_ms: ts,
            q_prefill: 0.18,
            q_decode: 0.32,
            kv_decode: 0.12,
            slo_prefill: 0.0,
            slo_decode: 0.05,
            fp_share: 0.82,
            prefill_heavy: false,
            held_out,
        }
    }

    #[test]
    fn fleet_pilot_heldout_correlates() {
        let windows = vec![
            heavy(0, false),
            light(60_000, false),
            heavy(120_000, false),
            light(180_000, true),
            heavy(240_000, true),
            light(300_000, true),
            heavy(360_000, true),
            light(420_000, true),
        ];
        let report = replay_fleet_pilot(&windows, 0.45);
        assert!(report.heldout_correlation > 0.45);
        assert!(report.heldout_mean_pi_heavy > report.heldout_mean_pi_light);
        assert!(report.gate_pass);
    }
}
