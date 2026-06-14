//! Fleet pilot shadow replay + corrector shadow eval (Track A).

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use demiurge_control::{
    eval_goodput_improvement, replay_fleet_pilot, train_bounded_delta, TraceWindow,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct FleetPilotConfig {
    trace_path: String,
    min_heldout_correlation: f64,
    min_corrector_goodput: f64,
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

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn load_config() -> Result<FleetPilotConfig, Box<dyn Error>> {
    let raw = fs::read_to_string(repo_root().join("design/fleet-pilot.toml"))?;
    Ok(toml::from_str(&raw)?)
}

fn load_trace(path: &Path) -> Result<Vec<TraceWindow>, Box<dyn Error>> {
    let path = if path.is_relative() {
        repo_root().join(path)
    } else {
        path.to_path_buf()
    };
    let mut windows = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row: TraceRow = serde_json::from_str(line)?;
        windows.push(row.into());
    }
    Ok(windows)
}

pub fn fleet_pilot() -> Result<(), Box<dyn Error>> {
    let cfg = load_config()?;
    let trace_path = Path::new(&cfg.trace_path);
    let windows = load_trace(trace_path)?;
    if windows.is_empty() {
        return Err(format!("empty trace: {}", trace_path.display()).into());
    }

    let report = replay_fleet_pilot(&windows, cfg.min_heldout_correlation);
    println!(
        "fleet-pilot: train={} heldout={} corr={:.3} pi_heavy={:.3} pi_light={:.3} gate={}",
        report.train_windows,
        report.heldout_windows,
        report.heldout_correlation,
        report.heldout_mean_pi_heavy,
        report.heldout_mean_pi_light,
        if report.gate_pass { "PASS" } else { "FAIL" }
    );

    if !report.gate_pass {
        return Err("fleet-pilot held-out π* correlation gate failed".into());
    }

    // Corrector shadow eval uses synthetic calibration samples (offline Track A gate).
    let shadow_samples = synthetic_corrector_samples();
    let delta = train_bounded_delta(&shadow_samples);
    let goodput = eval_goodput_improvement(&shadow_samples, delta);
    println!(
        "corrector-shadow: delta={:.4} goodput_improvement={:.1}% gate={}",
        delta,
        goodput * 100.0,
        if goodput >= cfg.min_corrector_goodput {
            "PASS"
        } else {
            "FAIL"
        }
    );
    if goodput < cfg.min_corrector_goodput {
        return Err("corrector shadow goodput gate failed".into());
    }

    let out_dir = repo_root().join("target/fleet-pilot");
    fs::create_dir_all(&out_dir)?;
    fs::write(
        out_dir.join("latest.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "trace": cfg.trace_path,
            "fleet_pilot": {
                "train_windows": report.train_windows,
                "heldout_windows": report.heldout_windows,
                "heldout_correlation": report.heldout_correlation,
                "heldout_mean_pi_heavy": report.heldout_mean_pi_heavy,
                "heldout_mean_pi_light": report.heldout_mean_pi_light,
                "gate_pass": report.gate_pass,
            },
            "corrector_shadow": {
                "delta": delta,
                "goodput_improvement": goodput,
            },
        }))?,
    )?;
    Ok(())
}

fn synthetic_corrector_samples() -> Vec<demiurge_control::CorrectorShadowSample> {
    use demiurge_control::CorrectorShadowSample;
    vec![
        CorrectorShadowSample {
            prompt_tokens: 256,
            analytic_ln: -1.2,
            observed_us: 280_000,
            pool_pi: 0.5,
            backend_label: "dc0".into(),
        },
        CorrectorShadowSample {
            prompt_tokens: 512,
            analytic_ln: -0.9,
            observed_us: 520_000,
            pool_pi: 0.5,
            backend_label: "dc1".into(),
        },
        CorrectorShadowSample {
            prompt_tokens: 768,
            analytic_ln: -0.6,
            observed_us: 890_000,
            pool_pi: 0.55,
            backend_label: "dc0".into(),
        },
        CorrectorShadowSample {
            prompt_tokens: 1024,
            analytic_ln: -0.4,
            observed_us: 1_200_000,
            pool_pi: 0.55,
            backend_label: "dc1".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_trace_passes_gate() {
        let cfg = load_config().expect("config");
        let windows = load_trace(Path::new(&cfg.trace_path)).expect("trace");
        let report = replay_fleet_pilot(&windows, cfg.min_heldout_correlation);
        assert!(report.gate_pass, "corr={}", report.heldout_correlation);
    }
}
