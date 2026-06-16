//! **'sim** — trace-driven fleet load planning and calibration. [DEMI-FLEET-SIM]
//!
//! Maps production or synthetic fleet trace windows to live load-bench knobs
//! (concurrency, prefill fraction, token profile, backend delays).

use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::fleet_pilot::{replay_fleet_pilot, FleetPilotReport, TraceWindow};

#[derive(Debug, Clone)]
pub struct SimBaseKnobs {
    pub concurrency: u32,
    pub requests_per_worker: u32,
    pub base_prefill_delay_us: u64,
    pub base_decode_delay_us: u64,
    pub long_prompt_tokens: u64,
    pub long_prompt_tokens_heavy: u64,
    pub request_style: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowKnobs {
    pub prefill_fraction: f64,
    pub concurrency: u32,
    pub requests_per_worker: u32,
    pub request_style: String,
    pub long_prompt_tokens: u64,
    pub prefill_delay_us: u64,
    pub decode_delay_us: u64,
    pub delay_window_mult: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetWindowResult {
    pub ts_ms: u64,
    pub prefill_heavy: bool,
    pub held_out: bool,
    pub ok: u64,
    /// Non-200 responses (graceful + hard).
    pub errors: u64,
    /// HTTP 503 graceful shed (admit / KV pool).
    #[serde(default)]
    pub errors_graceful: u64,
    /// Wire failures, non-503 HTTP, or unexpected status.
    #[serde(default)]
    pub errors_hard: u64,
    pub p99_us: u64,
    pub dataplane_pi: f64,
    pub pi_star: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FleetSimReport {
    pub windows: usize,
    pub heldout_correlation: f64,
    pub live_pi_correlation: f64,
    pub gate_pass: bool,
    pub window_results: Vec<FleetWindowResult>,
}

#[derive(Debug, Deserialize)]
struct TraceRow {
    ts_ms: u64,
    q_prefill: f64,
    q_decode: f64,
    kv_decode: f64,
    #[serde(default)]
    slo_prefill: f64,
    #[serde(default)]
    slo_decode: f64,
    #[serde(default)]
    fp_share: f64,
    prefill_heavy: bool,
    #[serde(default)]
    held_out: bool,
}

impl From<TraceRow> for TraceWindow {
    fn from(row: TraceRow) -> Self {
        Self {
            ts_ms: row.ts_ms,
            q_prefill: row.q_prefill,
            q_decode: row.q_decode,
            kv_decode: row.kv_decode,
            slo_prefill: row.slo_prefill,
            slo_decode: row.slo_decode,
            fp_share: row.fp_share,
            prefill_heavy: row.prefill_heavy,
            held_out: row.held_out,
        }
    }
}

/// Load JSONL fleet trace (same format as fleet-pilot).
pub fn load_fleet_trace(path: &Path) -> Result<Vec<TraceWindow>, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut windows = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row: TraceRow =
            serde_json::from_str(line).map_err(|e| format!("parse trace line: {e}"))?;
        windows.push(row.into());
    }
    if windows.is_empty() {
        return Err(format!("empty trace: {}", path.display()));
    }
    Ok(windows)
}

/// Map one trace window to live load knobs.
pub fn window_knobs(w: &TraceWindow, base: &SimBaseKnobs) -> WindowKnobs {
    let prefill_fraction = if w.prefill_heavy {
        (0.70 + w.q_prefill * 0.25).clamp(0.55, 0.95)
    } else {
        (0.10 + w.q_prefill * 0.35).clamp(0.05, 0.45)
    };

    let load = (0.45 + w.q_prefill + w.q_decode).clamp(0.5, 2.0);
    let concurrency = ((f64::from(base.concurrency) * load).round() as u32).clamp(4, 64);

    let request_style = base.request_style.clone();

    let long_prompt_tokens = if w.prefill_heavy {
        base.long_prompt_tokens_heavy
    } else {
        base.long_prompt_tokens
    };

    let prefill_delay_us =
        (base.base_prefill_delay_us as f64 * (0.4 + w.q_prefill * 1.6)).max(1.0) as u64;
    let decode_delay_us =
        (base.base_decode_delay_us as f64 * (0.4 + w.q_decode * 1.6)).max(1.0) as u64;
    let delay_window_mult = (0.5 + w.q_prefill + w.q_decode * 0.5).clamp(0.5, 2.5);

    WindowKnobs {
        prefill_fraction,
        concurrency,
        requests_per_worker: base.requests_per_worker,
        request_style,
        long_prompt_tokens,
        prefill_delay_us,
        decode_delay_us,
        delay_window_mult,
    }
}

/// Per-backend delay for heterogeneous fleet tiers (L2 calibration).
pub fn tier_delay_us(tier_index: u32, tier_count: u32, base_us: u64, window_mult: f64) -> u64 {
    let skew = 1.0 + f64::from(tier_index) / f64::from(tier_count.max(1)) * 0.6;
    ((base_us as f64) * window_mult * skew).max(1.0) as u64
}

/// Apply symmetric jitter around `delay_us` (deterministic from `salt` for reproducibility).
pub fn jitter_delay_us(delay_us: u64, jitter_us: u64, salt: u64) -> u64 {
    if jitter_us == 0 {
        return delay_us;
    }
    let span = jitter_us.saturating_mul(2).saturating_add(1);
    let delta = salt.wrapping_mul(1_103_515_245).wrapping_add(12345) % span;
    delay_us.saturating_add(delta).saturating_sub(jitter_us)
}

/// Gate fleet replay: shadow π* correlates on held-out windows; live π separates heavy/light.
pub fn eval_fleet_sim_gate(
    pilot: &FleetPilotReport,
    live: &[FleetWindowResult],
    min_shadow_correlation: f64,
    min_live_correlation: Option<f64>,
    max_heavy_graceful_rate: Option<f64>,
) -> FleetSimReport {
    let held_live: Vec<_> = live.iter().filter(|w| w.held_out).collect();
    let live_values: Vec<f64> = held_live.iter().map(|w| w.dataplane_pi).collect();
    let labels: Vec<bool> = held_live.iter().map(|w| w.prefill_heavy).collect();
    let live_corr = crate::fleet_pilot::point_biserial_corr(&live_values, &labels);

    let heavy_total: u64 = live
        .iter()
        .filter(|w| w.prefill_heavy)
        .map(|w| w.ok + w.errors)
        .sum();
    let light_total: u64 = live
        .iter()
        .filter(|w| !w.prefill_heavy)
        .map(|w| w.ok + w.errors)
        .sum();
    let load_separation = heavy_total > light_total;

    let shadow_pass = pilot.gate_pass && pilot.heldout_correlation >= min_shadow_correlation;

    let heavy_graceful: u64 = live
        .iter()
        .filter(|w| w.prefill_heavy)
        .map(|w| w.errors_graceful)
        .sum();
    let heavy_shed_rate = if heavy_total > 0 {
        heavy_graceful as f64 / heavy_total as f64
    } else {
        0.0
    };
    let heavy_shed_pass = max_heavy_graceful_rate
        .map(|max| heavy_shed_rate <= max)
        .unwrap_or(true);

    let live_pass = min_live_correlation
        .map(|min| live_corr >= min)
        .unwrap_or(true);

    let gate_pass = shadow_pass && load_separation && live_pass && heavy_shed_pass;

    FleetSimReport {
        windows: live.len(),
        heldout_correlation: pilot.heldout_correlation,
        live_pi_correlation: live_corr,
        gate_pass,
        window_results: live.to_vec(),
    }
}

pub fn shadow_pilot_for_trace(windows: &[TraceWindow], min_correlation: f64) -> FleetPilotReport {
    replay_fleet_pilot(windows, min_correlation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_pilot::FleetPilotReport;

    #[test]
    fn window_knobs_prefill_heavy_raises_fraction() {
        let base = SimBaseKnobs {
            concurrency: 16,
            requests_per_worker: 20,
            base_prefill_delay_us: 100,
            base_decode_delay_us: 50,
            long_prompt_tokens: 512,
            long_prompt_tokens_heavy: 2048,
            request_style: "mixed_tokens".into(),
        };
        let heavy = TraceWindow {
            ts_ms: 0,
            q_prefill: 0.9,
            q_decode: 0.2,
            kv_decode: 0.25,
            slo_prefill: 0.1,
            slo_decode: 0.0,
            fp_share: 0.1,
            prefill_heavy: true,
            held_out: false,
        };
        let light = TraceWindow {
            prefill_heavy: false,
            q_prefill: 0.2,
            q_decode: 0.35,
            kv_decode: 0.1,
            fp_share: 0.85,
            ..heavy
        };
        let h = window_knobs(&heavy, &base);
        let l = window_knobs(&light, &base);
        assert!(h.prefill_fraction > l.prefill_fraction);
        assert_eq!(h.request_style, "mixed_tokens");
        assert_eq!(h.long_prompt_tokens, 2048);
    }

    #[test]
    fn tier_delay_increases_with_index() {
        let d0 = tier_delay_us(0, 4, 100, 1.0);
        let d3 = tier_delay_us(3, 4, 100, 1.0);
        assert!(d3 > d0);
    }

    #[test]
    fn heavy_window_shed_gate() {
        let pilot = FleetPilotReport {
            train_windows: 0,
            heldout_windows: 2,
            heldout_correlation: 0.5,
            heldout_mean_pi_heavy: 0.8,
            heldout_mean_pi_light: 0.2,
            gate_pass: true,
            replays: vec![],
        };
        let live = vec![
            FleetWindowResult {
                ts_ms: 0,
                prefill_heavy: true,
                held_out: true,
                ok: 18,
                errors: 2,
                errors_graceful: 2,
                errors_hard: 0,
                p99_us: 100,
                dataplane_pi: 0.8,
                pi_star: 0.8,
            },
            FleetWindowResult {
                ts_ms: 1,
                prefill_heavy: false,
                held_out: true,
                ok: 10,
                errors: 0,
                errors_graceful: 0,
                errors_hard: 0,
                p99_us: 50,
                dataplane_pi: 0.2,
                pi_star: 0.2,
            },
        ];
        let pass = eval_fleet_sim_gate(&pilot, &live, 0.45, None, Some(0.25));
        assert!(pass.gate_pass);
        let fail = eval_fleet_sim_gate(&pilot, &live, 0.45, None, Some(0.09));
        assert!(!fail.gate_pass);
    }
}
