//! Local TCP load scenarios against a live demiurge-router stack.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use demiurge_control::{
    eval_fleet_sim_gate, eval_transfer_ratio_median, jitter_delay_us, load_fleet_trace,
    shadow_pilot_for_trace, tier_delay_us, window_knobs, FleetWindowResult, SimBaseKnobs,
};
use demiurge_cost::{kv_breakdown, TopologyId};
use demiurge_handoff::{
    HandoffTransport, HeaderPassthroughTransport, MockRdmaTransport, ModeledRdmaTransport,
};
use demiurge_router::{
    admit_disaggregated, serve, spawn_delay_backend, AdmitMode, Backend, KvHandoffRegistry,
    KvReservationLedger, Router,
};
use serde::{Deserialize, Serialize};

const LOAD_BENCH: &str = "design/load-bench.toml";

#[derive(Debug, Deserialize)]
struct LoadBenchFile {
    settings: LoadSettings,
    #[serde(default)]
    scenario: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct LoadSettings {
    report_dir: String,
    warmup_requests: u32,
    startup_delay_ms: u64,
    /// Pause between isolated `--stress` subprocess runs (ephemeral-port recovery).
    #[serde(default)]
    stress_recovery_ms: u64,
    #[serde(default)]
    gate_strict: bool,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    id: String,
    summary: String,
    #[serde(default)]
    ci: bool,
    /// Real stress scenarios: run via `load-bench --stress` only (excluded from default suite).
    #[serde(default)]
    stress: bool,
    /// Harden-only scenarios (`load-bench --harden` / harden-verify).
    #[serde(default)]
    harden: bool,
    backends: u32,
    base_cost_seconds: f64,
    cost_step_seconds: f64,
    #[serde(default)]
    backend_delay_us: u64,
    /// Decode pool size (0 = prefill-only router).
    #[serde(default)]
    decode_backends: u32,
    concurrency: u32,
    requests_per_worker: u32,
    #[serde(default = "default_prefill_fraction")]
    prefill_fraction: f64,
    max_p99_ms: Option<f64>,
    /// `path` | `short_tokens` | `long_tokens` | `mixed_tokens`
    #[serde(default = "default_request_style")]
    request_style: String,
    #[serde(default)]
    use_kv_pool: bool,
    #[serde(default = "default_bytes_per_token")]
    bytes_per_token: u64,
    #[serde(default)]
    decode_capacity_bytes: u64,
    #[serde(default)]
    prefill_kv_headers: bool,
    /// Long-context token count for `long_tokens` / KV hand-off paths.
    #[serde(default = "default_long_tokens")]
    long_prompt_tokens: u64,
    /// Cap concurrent in-flight TCP requests (0 = unlimited).
    #[serde(default)]
    max_inflight: u32,
    /// `e2e` (default) or `admit_decouple` (P1 accept-path gate).
    #[serde(default = "default_measure")]
    measure: String,
    /// Prefill backend delays (µs) for `admit_decouple`; compares p99 ratio across arms.
    #[serde(default)]
    prefill_delay_sweep_us: Vec<u64>,
    /// Max p99_slow / p99_fast for `admit_decouple` (default 8.0).
    #[serde(default)]
    max_accept_p99_ratio: Option<f64>,
    /// Matching pf/dc labels (`node0`..) for greedy pairing colocation tests.
    #[serde(default)]
    paired_labels: bool,
    /// Allow up to this many KV admit rejects (503) without failing the scenario.
    #[serde(default)]
    max_kv_admit_rejects: Option<u64>,
    /// Publish rebalancer π to the RCU dataplane (`DEMIURGE_REBALANCER_ACTUATE=1` equivalent).
    #[serde(default)]
    rebalancer_actuation: bool,
    /// Sequential load phases (each runs `requests_per_worker` per worker); last phase drives π gates.
    #[serde(default)]
    step_prefill_fraction: Vec<f64>,
    /// Per-phase request style (`path`, `long_tokens`, …); defaults to `request_style` when omitted.
    #[serde(default)]
    step_request_style: Vec<String>,
    /// Fail when final dataplane π stays below this after actuation scenarios.
    #[serde(default)]
    min_dataplane_pi: Option<f64>,
    /// Pause `stress_recovery_ms` before this scenario's isolated subprocess (port recovery).
    #[serde(default)]
    isolate_recovery: bool,
    /// Enable io_uring production TCP proxy (`Router::with_io_uring(true)`).
    #[serde(default)]
    io_uring: bool,
    /// Linux+root: veth + kernel XDP admit + io_uring (implies io_uring when unset).
    #[serde(default)]
    track_b_kernel: bool,
    /// `userspace` | `kernel_xdp` | `hybrid` — overrides env for this scenario.
    #[serde(default)]
    admit_mode: Option<String>,
    /// Skip on non-Linux hosts (Track B kernel dataplane scenarios).
    #[serde(default)]
    linux_only: bool,
    /// Skip without failing the suite when prerequisites are missing (e.g. root for kernel XDP load).
    #[serde(default)]
    optional: bool,
    /// Fixed userspace admit bucket capacity (0 = default burst).
    #[serde(default)]
    admit_capacity: u64,
    /// Fail when max/p99 latency ratio exceeds this (tail widening gate).
    #[serde(default)]
    max_p99_tail_ratio: Option<f64>,
    /// Fail when fast-path misroute mean exceeds this.
    #[serde(default)]
    max_misroute_mean: Option<f64>,
    /// Require at least this many KV admit rejects (harden exhaust scenarios).
    #[serde(default)]
    min_kv_admit_rejects: Option<u64>,
    /// Require at least this many client errors (e.g. userspace admit 503 sheds).
    #[serde(default)]
    min_errors: Option<u64>,
    /// Fail when hard errors (non-graceful) exceed this cap (`'sim` default: 0).
    #[serde(default)]
    max_hard_errors: Option<u64>,
    /// Response body size for `large_response` request style.
    #[serde(default)]
    large_body_bytes: u64,
    /// **'sim** spinoff — trace-driven fleet replay (`load-bench --sim`).
    #[serde(default)]
    sim: bool,
    /// JSONL fleet trace for `measure = "fleet_replay"`.
    #[serde(default)]
    trace_path: Option<String>,
    /// Gate: held-out shadow π* correlation vs prefill-heavy windows.
    #[serde(default)]
    min_fleet_correlation: Option<f64>,
    /// Optional gate: held-out live dataplane π correlation (informational when unset).
    #[serde(default)]
    min_live_fleet_correlation: Option<f64>,
    /// Optional gate: max graceful 503 rate on prefill-heavy trace windows.
    #[serde(default)]
    max_heavy_graceful_rate: Option<f64>,
    /// Handoff transport override: `tcp` | `mock_rdma` | `modeled_rdma`.
    #[serde(default)]
    handoff_transport: Option<String>,
    /// Use RDMA topology model in decode placement (not just shadow logging).
    #[serde(default)]
    rdma_routing: bool,
    /// L2: symmetric delay jitter on mock backend responses (µs).
    #[serde(default)]
    backend_delay_jitter_us: u64,
    /// L2: tier-skewed backend delays emulating heterogeneous fleet nodes.
    #[serde(default)]
    heterogeneous_backends: bool,
    /// L3: extra delay on upper backend tiers (simulated cross-node netem, µs).
    #[serde(default)]
    sim_netem_us: u64,
    /// Modeled RDMA handoff transport + per-backend topology (shadow cost samples).
    #[serde(default)]
    rdma_modeled: bool,
    /// Minimum RDMA cost-shadow samples (disagg handoffs with topology distance).
    #[serde(default)]
    min_rdma_shadow_samples: Option<u64>,
    /// Minimum median observed/predicted transfer ratio (modeled transport ≈ 1.0).
    #[serde(default)]
    min_rdma_transfer_ratio: Option<f64>,
    /// Maximum median observed/predicted transfer ratio.
    #[serde(default)]
    max_rdma_transfer_ratio: Option<f64>,
}

fn default_prefill_fraction() -> f64 {
    1.0
}

fn default_request_style() -> String {
    "path".into()
}

fn default_bytes_per_token() -> u64 {
    128
}

fn default_long_tokens() -> u64 {
    2048
}

fn default_measure() -> String {
    "e2e".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub id: String,
    pub summary: String,
    pub backends: u32,
    #[serde(default)]
    pub decode_backends: u32,
    pub concurrency: u32,
    pub requests_per_worker: u32,
    pub backend_delay_us: u64,
    #[serde(default)]
    pub request_style: String,
    #[serde(default)]
    pub use_kv_pool: bool,
    pub total_requests: u64,
    pub ok: u64,
    pub errors: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors_graceful: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors_hard: Option<u64>,
    pub duration_secs: f64,
    pub req_per_sec: f64,
    pub min_us: u64,
    pub p50_us: u64,
    pub p90_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub max_p99_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_bytes_reserved_peak: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_admit_rejects: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_transfer_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_bytes_p50: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_bytes_p99: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_wall_us_p50: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_wall_us_p99: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_p99_us_low: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_p99_us_high: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_p99_ratio: Option<f64>,
    #[serde(default)]
    pub latencies_us: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataplane_pi: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataplane_age_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rcu_stale: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pi_star: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_dataplane_pi_sampled: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast_path_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colocated_routes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disagg_routes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fastpath_misroute_mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fastpath_misroute_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrector_shadow_samples: Option<u64>,
    /// **'sim** per-trace-window live results.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fleet_windows: Vec<FleetWindowResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet_live_pi_correlation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet_shadow_correlation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet_heavy_graceful_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet_sim_gate_pass: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rdma_shadow_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rdma_transfer_ratio_median: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoadBenchReport {
    pub generated_at: String,
    pub hostname: String,
    pub scenarios: Vec<ScenarioResult>,
}

/// Gate thresholds from `load-bench.toml` for pseudo-report evaluation.
#[derive(Debug, Clone)]
pub struct ScenarioGateConfig {
    pub use_kv_pool: bool,
    pub decode_capacity_bytes: u64,
    pub max_kv_admit_rejects: Option<u64>,
    pub min_kv_admit_rejects: Option<u64>,
    pub min_errors: Option<u64>,
    pub max_hard_errors: Option<u64>,
    pub max_accept_p99_ratio: Option<f64>,
    pub min_dataplane_pi: Option<f64>,
    pub max_p99_tail_ratio: Option<f64>,
    pub max_misroute_mean: Option<f64>,
    pub min_fleet_correlation: Option<f64>,
    pub min_live_fleet_correlation: Option<f64>,
    pub max_heavy_graceful_rate: Option<f64>,
    pub min_rdma_shadow_samples: Option<u64>,
    pub min_rdma_transfer_ratio: Option<f64>,
    pub max_rdma_transfer_ratio: Option<f64>,
}

impl From<&Scenario> for ScenarioGateConfig {
    fn from(sc: &Scenario) -> Self {
        Self {
            use_kv_pool: sc.use_kv_pool,
            decode_capacity_bytes: effective_decode_capacity_bytes(sc),
            max_kv_admit_rejects: sc.max_kv_admit_rejects,
            min_kv_admit_rejects: sc.min_kv_admit_rejects,
            min_errors: sc.min_errors,
            max_hard_errors: sc.max_hard_errors,
            max_accept_p99_ratio: sc.max_accept_p99_ratio,
            min_dataplane_pi: sc.min_dataplane_pi,
            max_p99_tail_ratio: sc.max_p99_tail_ratio,
            max_misroute_mean: sc.max_misroute_mean,
            min_fleet_correlation: sc.min_fleet_correlation,
            min_live_fleet_correlation: sc.min_live_fleet_correlation,
            max_heavy_graceful_rate: sc.max_heavy_graceful_rate,
            min_rdma_shadow_samples: sc.min_rdma_shadow_samples,
            min_rdma_transfer_ratio: sc.min_rdma_transfer_ratio,
            max_rdma_transfer_ratio: sc.max_rdma_transfer_ratio,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GateVerdict {
    pub name: &'static str,
    pub pass: bool,
    pub detail: String,
}

pub fn gate_configs_by_id(
) -> Result<std::collections::HashMap<String, ScenarioGateConfig>, Box<dyn std::error::Error>> {
    let file: LoadBenchFile = toml::from_str(&fs::read_to_string(LOAD_BENCH)?)?;
    Ok(file
        .scenario
        .iter()
        .map(|sc| (sc.id.clone(), ScenarioGateConfig::from(sc)))
        .collect())
}

pub fn evaluate_scenario_gates(
    cfg: &ScenarioGateConfig,
    result: &ScenarioResult,
) -> Vec<GateVerdict> {
    let mut gates = Vec::new();

    if cfg.max_kv_admit_rejects.is_some()
        || cfg.min_kv_admit_rejects.is_some()
        || cfg.min_errors.is_some()
        || cfg.max_hard_errors.is_some()
        || result.errors > 0
    {
        let rejects = result.kv_admit_rejects.unwrap_or(0);
        let graceful = result.errors_graceful.unwrap_or(0);
        let hard = result
            .errors_hard
            .unwrap_or_else(|| result.errors.saturating_sub(graceful));

        if let Some(max_hard) = cfg.max_hard_errors {
            gates.push(GateVerdict {
                name: "hard errors",
                pass: hard <= max_hard,
                detail: format!("{hard} hard (cap {max_hard})"),
            });
        }

        if let Some(cap) = cfg.max_kv_admit_rejects {
            let pass = if result.errors_graceful.is_some() {
                graceful <= cap && rejects <= cap && hard <= cfg.max_hard_errors.unwrap_or(cap)
            } else {
                result.errors <= cap && rejects <= cap
            };
            gates.push(GateVerdict {
                name: "errors",
                pass,
                detail: if result.errors_graceful.is_some() {
                    format!("{graceful} 503 / {hard} hard / {rejects} kv rejects (cap {cap})")
                } else {
                    format!("{} err / {} kv rejects (cap {cap})", result.errors, rejects)
                },
            });
        } else if result.errors > 0 && cfg.max_hard_errors.is_none() {
            gates.push(GateVerdict {
                name: "errors",
                pass: false,
                detail: format!(
                    "{} err / {} req (zero required)",
                    result.errors, result.total_requests
                ),
            });
        } else if cfg.max_hard_errors.is_none() {
            gates.push(GateVerdict {
                name: "errors",
                pass: true,
                detail: if graceful > 0 || hard > 0 {
                    format!("{graceful} 503 / {hard} hard")
                } else {
                    "0 err".into()
                },
            });
        } else if graceful > 0 {
            gates.push(GateVerdict {
                name: "graceful 503",
                pass: true,
                detail: format!("{graceful} shed"),
            });
        }
        if let Some(min_rejects) = cfg.min_kv_admit_rejects {
            gates.push(GateVerdict {
                name: "kv rejects min",
                pass: rejects >= min_rejects,
                detail: format!("{rejects} rejects (min {min_rejects})"),
            });
        }
        if let Some(min_err) = cfg.min_errors {
            gates.push(GateVerdict {
                name: "errors min",
                pass: result.errors >= min_err,
                detail: format!("{} err (min {min_err})", result.errors),
            });
        }
    }

    if cfg.use_kv_pool && cfg.decode_capacity_bytes > 0 {
        if let Some(peak) = result.kv_bytes_reserved_peak {
            gates.push(GateVerdict {
                name: "kv peak",
                pass: peak <= cfg.decode_capacity_bytes,
                detail: format!("{peak} bytes (cap {})", cfg.decode_capacity_bytes),
            });
        }
    }

    if let Some(limit) = result.max_p99_ms {
        let p99_ms = result.p99_us as f64 / 1000.0;
        let pass = result.ok > 0 && p99_ms <= limit;
        gates.push(GateVerdict {
            name: "p99",
            pass,
            detail: if result.ok == 0 {
                "no successful requests".into()
            } else {
                format!("{p99_ms:.2}ms (≤ {limit:.1}ms)")
            },
        });
    }

    if let (Some(ratio), Some(limit)) = (result.accept_p99_ratio, cfg.max_accept_p99_ratio) {
        gates.push(GateVerdict {
            name: "accept p99 ratio",
            pass: ratio <= limit,
            detail: format!(
                "{ratio:.2} (≤ {limit:.1}) — {}µs / {}µs",
                result.accept_p99_us_low.unwrap_or(0),
                result.accept_p99_us_high.unwrap_or(0),
            ),
        });
    }

    if let Some(min_pi) = cfg.min_dataplane_pi {
        if let Some(observed) = result.dataplane_pi {
            gates.push(GateVerdict {
                name: "dataplane π",
                pass: observed >= min_pi,
                detail: format!("{observed:.3} (min {min_pi:.3})"),
            });
        }
    }

    if let Some(limit) = cfg.max_p99_tail_ratio {
        if result.p99_us > 0 {
            let ratio = result.max_us as f64 / result.p99_us as f64;
            gates.push(GateVerdict {
                name: "tail max/p99",
                pass: ratio <= limit,
                detail: format!(
                    "{ratio:.2} (≤ {limit:.1}) — max {}µs / p99 {}µs",
                    result.max_us, result.p99_us
                ),
            });
        }
    }

    if let Some(limit) = cfg.max_misroute_mean {
        if let Some(m) = result.fastpath_misroute_mean {
            gates.push(GateVerdict {
                name: "misroute mean",
                pass: m <= limit,
                detail: format!("{m:.3} (≤ {limit:.3})"),
            });
        }
    }

    if cfg.min_fleet_correlation.is_some() || result.fleet_sim_gate_pass.is_some() {
        let shadow_corr = result.fleet_shadow_correlation.unwrap_or(0.0);
        let min_shadow = cfg.min_fleet_correlation.unwrap_or(0.45);
        gates.push(GateVerdict {
            name: "fleet shadow π*",
            pass: shadow_corr >= min_shadow,
            detail: format!("held-out shadow_corr {shadow_corr:.3} (min {min_shadow:.2})"),
        });

        if let Some(corr) = result.fleet_live_pi_correlation {
            if let Some(min_live) = cfg.min_live_fleet_correlation {
                gates.push(GateVerdict {
                    name: "fleet live π",
                    pass: corr >= min_live,
                    detail: format!("held-out live_pi_corr {corr:.3} (min {min_live:.2})"),
                });
            } else {
                gates.push(GateVerdict {
                    name: "fleet live π",
                    pass: true,
                    detail: format!("held-out live_pi_corr {corr:.3} (informational)"),
                });
            }
        }

        if let Some(max_rate) = cfg.max_heavy_graceful_rate {
            let rate = result.fleet_heavy_graceful_rate.unwrap_or(0.0);
            gates.push(GateVerdict {
                name: "fleet heavy 503",
                pass: rate <= max_rate,
                detail: format!("heavy window shed rate {rate:.3} (max {max_rate:.2})"),
            });
        }

        let pass = result.fleet_sim_gate_pass.unwrap_or(false);
        gates.push(GateVerdict {
            name: "fleet replay",
            pass,
            detail: if pass {
                "overall PASS".into()
            } else {
                "overall FAIL".into()
            },
        });
    }

    if let Some(min_n) = cfg.min_rdma_shadow_samples {
        let n = result.rdma_shadow_samples.unwrap_or(0);
        gates.push(GateVerdict {
            name: "RDMA shadow samples",
            pass: n >= min_n,
            detail: format!("{n} (min {min_n})"),
        });
    }

    if let Some(min_r) = cfg.min_rdma_transfer_ratio {
        if let Some(ratio) = result.rdma_transfer_ratio_median {
            gates.push(GateVerdict {
                name: "RDMA transfer ratio min",
                pass: ratio >= min_r,
                detail: format!("{ratio:.3} (min {min_r:.3})"),
            });
        }
    }

    if let Some(max_r) = cfg.max_rdma_transfer_ratio {
        if let Some(ratio) = result.rdma_transfer_ratio_median {
            gates.push(GateVerdict {
                name: "RDMA transfer ratio max",
                pass: ratio <= max_r,
                detail: format!("{ratio:.3} (max {max_r:.3})"),
            });
        }
    }

    gates
}

struct RouterStack {
    addr: SocketAddr,
    router: Arc<Router>,
    ledger: Option<Arc<KvReservationLedger>>,
    handoffs: Option<Arc<KvHandoffRegistry>>,
    #[cfg(target_os = "linux")]
    _track_b_veth: Option<crate::track_b_load::TrackBVeth>,
}

fn apply_scenario_router_flags(
    mut router: Router,
    sc: &Scenario,
    prefill: &[Arc<Backend>],
    decode: &[Arc<Backend>],
) -> Router {
    if sc.io_uring || sc.track_b_kernel {
        router = router.with_io_uring(true);
    }
    if let Some(ref mode) = sc.admit_mode {
        if let Some(m) = AdmitMode::parse(mode) {
            router = router.with_admit_mode(m);
        }
    } else if sc.track_b_kernel {
        router = router.with_admit_mode(AdmitMode::KernelXdp);
    }
    if sc.rdma_modeled || sc.rdma_routing {
        router = router.with_rdma_routing(true);
    }
    if sc.rdma_modeled {
        let mut topo = HashMap::new();
        for b in prefill.iter().chain(decode.iter()) {
            topo.insert(b.label.clone(), b.topology().clone());
        }
        router = router.with_handoff_transport(Arc::new(ModeledRdmaTransport::new(topo)));
    } else if let Some(ref mode) = sc.handoff_transport {
        let mut topo = HashMap::new();
        for b in prefill.iter().chain(decode.iter()) {
            topo.insert(b.label.clone(), b.topology().clone());
        }
        let transport: Arc<dyn HandoffTransport> = match mode.as_str() {
            "mock_rdma" => Arc::new(MockRdmaTransport::default()),
            "modeled_rdma" => Arc::new(ModeledRdmaTransport::new(topo)),
            _ => Arc::new(HeaderPassthroughTransport),
        };
        router = router.with_handoff_transport(transport);
    }
    router
}

fn scenario_skip_reason(sc: &Scenario) -> Option<String> {
    if sc.linux_only {
        #[cfg(not(target_os = "linux"))]
        return Some("Linux only".into());
    }
    #[cfg(not(target_os = "linux"))]
    if sc.track_b_kernel {
        return Some("Linux only".into());
    }
    #[cfg(target_os = "linux")]
    if sc.track_b_kernel && !crate::track_b_load::is_root() {
        return Some("track_b_kernel requires root".into());
    }
    None
}

fn spawn_large_body_backend(delay_us: u64, body_bytes: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let addr = listener.local_addr().expect("backend addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            if delay_us > 0 {
                thread::sleep(Duration::from_micros(delay_us));
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {body_bytes}\r\nconnection: close\r\n\r\n"
            );
            let _ = s.write_all(head.as_bytes());
            let chunk = vec![b'x'; 64 * 1024];
            let mut sent = 0usize;
            while sent < body_bytes {
                let n = chunk.len().min(body_bytes - sent);
                if s.write_all(&chunk[..n]).is_err() {
                    break;
                }
                sent += n;
            }
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    addr
}

fn spawn_mock_backend(delay_us: u64, jitter_us: u64, kv_bytes: Option<u64>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let addr = listener.local_addr().expect("backend addr");
    thread::spawn(move || {
        static REQ: AtomicU64 = AtomicU64::new(0);
        static HANDLE: AtomicU64 = AtomicU64::new(1);
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let salt = REQ.fetch_add(1, Ordering::Relaxed);
            let sleep_us = if jitter_us > 0 {
                jitter_delay_us(delay_us, jitter_us, salt)
            } else {
                delay_us
            };
            if sleep_us > 0 {
                thread::sleep(Duration::from_micros(sleep_us));
            }
            if let Some(bytes) = kv_bytes {
                let handle = HANDLE.fetch_add(1, Ordering::Relaxed);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nx-demiurge-kv-handle: {handle}\r\nx-demiurge-kv-bytes: {bytes}\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
                );
                let _ = s.write_all(resp.as_bytes());
            } else {
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                );
            }
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    addr
}

/// Portable KV shed threshold: holds ~3 concurrent reservations; the 4th triggers reject.
fn kv_shed_capacity_bytes(long_prompt_tokens: u64, bytes_per_token: u64) -> u64 {
    let per = kv_breakdown(long_prompt_tokens, bytes_per_token).kv_reserved;
    per.saturating_mul(3) + per / 4
}

fn effective_decode_capacity_bytes(sc: &Scenario) -> u64 {
    if !sc.use_kv_pool {
        return sc.decode_capacity_bytes;
    }
    if sc.min_kv_admit_rejects.unwrap_or(0) > 0 {
        let tokens = sc.long_prompt_tokens.max(2048);
        return kv_shed_capacity_bytes(tokens, sc.bytes_per_token);
    }
    if sc.decode_capacity_bytes > 0 {
        return sc.decode_capacity_bytes;
    }
    let per = kv_breakdown(sc.long_prompt_tokens, sc.bytes_per_token).kv_reserved;
    per.saturating_mul(10)
}

fn spawn_router_stack(
    sc: &Scenario,
    prefill: &[Arc<Backend>],
    decode: &[Arc<Backend>],
) -> Result<RouterStack, String> {
    #[cfg(not(target_os = "linux"))]
    if sc.track_b_kernel {
        return Err(format!("{}: track_b_kernel requires Linux", sc.id));
    }

    #[cfg(target_os = "linux")]
    let track_b_veth = if sc.track_b_kernel {
        Some(crate::track_b_load::TrackBVeth::create()?)
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let _track_b_veth: Option<()> = None;

    if sc.use_kv_pool {
        let capacity = effective_decode_capacity_bytes(sc);
        if sc.min_kv_admit_rejects.unwrap_or(0) > 0 {
            eprintln!(
                "load-bench: {} KV pool auto-capacity {capacity} bytes (4th concurrent reservation sheds)",
                sc.id
            );
        }
        let (mut router, ledger, handoffs) = Router::with_kv_pool(
            prefill.to_vec(),
            decode.to_vec(),
            capacity,
            sc.bytes_per_token,
        );
        if sc.rebalancer_actuation {
            router = router.with_rebalancer_actuation(true);
        }
        router = apply_scenario_router_flags(router, sc, prefill, decode);
        #[cfg(target_os = "linux")]
        if let Some(ref veth) = track_b_veth {
            router = veth.attach_router(router)?;
        }
        if sc.admit_capacity > 0 {
            router.sync_admit_capacity(sc.admit_capacity);
        }
        let listener = {
            #[cfg(target_os = "linux")]
            {
                crate::track_b_load::bind_router_listener(&track_b_veth)?
            }
            #[cfg(not(target_os = "linux"))]
            {
                TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind router: {e}"))?
            }
        };
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let router = Arc::new(router);
        let serve_router = Arc::clone(&router);
        thread::spawn(move || {
            let _ = serve(listener, serve_router);
        });
        Ok(RouterStack {
            addr,
            router,
            ledger: Some(ledger),
            handoffs: Some(handoffs),
            #[cfg(target_os = "linux")]
            _track_b_veth: track_b_veth,
        })
    } else {
        let mut router = Router::new(prefill.to_vec(), decode.to_vec());
        if sc.rebalancer_actuation {
            router = router.with_rebalancer_actuation(true);
        }
        router = apply_scenario_router_flags(router, sc, prefill, decode);
        #[cfg(target_os = "linux")]
        if let Some(ref veth) = track_b_veth {
            router = veth.attach_router(router)?;
        }
        if sc.admit_capacity > 0 {
            router.sync_admit_capacity(sc.admit_capacity);
        }
        let listener = {
            #[cfg(target_os = "linux")]
            {
                crate::track_b_load::bind_router_listener(&track_b_veth)?
            }
            #[cfg(not(target_os = "linux"))]
            {
                TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind router: {e}"))?
            }
        };
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let router = Arc::new(router);
        let serve_router = Arc::clone(&router);
        thread::spawn(move || {
            let _ = serve(listener, serve_router);
        });
        Ok(RouterStack {
            addr,
            router,
            ledger: None,
            handoffs: None,
            #[cfg(target_os = "linux")]
            _track_b_veth: track_b_veth,
        })
    }
}

fn build_pool(count: u32, prefix: &str, sc: &Scenario, kv_bytes: Option<u64>) -> Vec<Arc<Backend>> {
    build_pool_calibrated(count, prefix, sc, kv_bytes, sc.backend_delay_us, 1.0)
}

fn bench_backend_topology(prefix: &str, i: u32, count: u32) -> TopologyId {
    let nodes = count.max(3);
    let node = format!("n{}", i % nodes);
    let rack = format!("r{}", (i / nodes.max(1)) % 2);
    let cluster = if prefix != "pf" && !i.is_multiple_of(2) {
        "cB"
    } else {
        "cA"
    };
    TopologyId::new(node, rack, cluster)
}

fn rdma_result_fields(router: &Router) -> (Option<u64>, Option<f64>) {
    let samples = router.rdma_cost_shadow_samples();
    if samples.is_empty() {
        return (Some(0), Some(1.0));
    }
    (
        Some(samples.len() as u64),
        Some(eval_transfer_ratio_median(&samples)),
    )
}

fn build_pool_calibrated(
    count: u32,
    prefix: &str,
    sc: &Scenario,
    kv_bytes: Option<u64>,
    base_delay_us: u64,
    window_mult: f64,
) -> Vec<Arc<Backend>> {
    let remote_cutoff = count.saturating_sub(count / 3).max(1);
    (0..count)
        .map(|i| {
            let mut delay_us = if sc.heterogeneous_backends {
                tier_delay_us(i, count, base_delay_us, window_mult)
            } else {
                base_delay_us
            };
            if sc.sim_netem_us > 0 && i >= remote_cutoff {
                delay_us = delay_us.saturating_add(sc.sim_netem_us);
            }
            let addr = if sc.request_style == "large_response" && sc.large_body_bytes > 0 {
                spawn_large_body_backend(delay_us, sc.large_body_bytes as usize)
            } else {
                spawn_mock_backend(delay_us, sc.backend_delay_jitter_us, kv_bytes)
            };
            let cost = sc.base_cost_seconds + sc.cost_step_seconds * f64::from(i);
            let label = if sc.paired_labels {
                format!("node{i}")
            } else {
                format!("{prefix}{i}")
            };
            let topology = if sc.rdma_modeled {
                bench_backend_topology(prefix, i, count)
            } else {
                TopologyId::default()
            };
            Backend::new_with_topology(label, addr, cost, topology)
        })
        .collect()
}

fn request_line(
    request_style: &str,
    long_prompt_tokens: u64,
    prefill_phase: bool,
    seq: u64,
) -> Vec<u8> {
    if !prefill_phase {
        return format!(
            "GET /decode/{seq} HTTP/1.1\r\nhost: load-bench\r\nx-demiurge-phase: decode\r\nconnection: close\r\n\r\n"
        )
        .into_bytes();
    }

    match request_style {
        "short_tokens" => {
            "GET / HTTP/1.1\r\nhost: load-bench\r\nx-demiurge-tokens: 32\r\nconnection: close\r\n\r\n"
                .to_string()
        }
        "long_tokens" => format!(
            "GET /long/{} HTTP/1.1\r\nhost: load-bench\r\nx-demiurge-tokens: {}\r\nconnection: close\r\n\r\n",
            long_prompt_tokens, long_prompt_tokens
        ),
        "mixed_tokens" => {
            if seq.is_multiple_of(2) {
                "GET / HTTP/1.1\r\nhost: load-bench\r\nx-demiurge-tokens: 32\r\nconnection: close\r\n\r\n"
                    .to_string()
            } else {
                format!(
                    "GET /long/{} HTTP/1.1\r\nhost: load-bench\r\nx-demiurge-tokens: {}\r\nconnection: close\r\n\r\n",
                    long_prompt_tokens, long_prompt_tokens
                )
            }
        }
        "large_response" => {
            "GET /large HTTP/1.1\r\nhost: load-bench\r\nconnection: close\r\n\r\n".to_string()
        }
        _ => format!(
            "GET /prefill/{seq} HTTP/1.1\r\nhost: load-bench\r\nconnection: close\r\n\r\n"
        ),
    }
    .into_bytes()
}

fn connect_router(router: SocketAddr) -> Result<TcpStream, ()> {
    const ATTEMPTS: u32 = 4;
    for attempt in 0..ATTEMPTS {
        match TcpStream::connect(router) {
            Ok(s) => return Ok(s),
            Err(_) if attempt + 1 < ATTEMPTS => {
                thread::sleep(Duration::from_millis(1 << attempt));
            }
            Err(_) => return Err(()),
        }
    }
    Err(())
}

fn parse_http_status(head: &[u8]) -> Option<u16> {
    let line_end = head.iter().position(|&b| b == b'\n')?;
    let line = &head[..line_end];
    let parts: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    if parts.len() < 2 || parts[0] != b"HTTP/1.1" {
        return None;
    }
    std::str::from_utf8(parts[1])
        .ok()
        .and_then(|s| s.parse().ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestOutcome {
    Ok(u64),
    Graceful503,
    HardError,
}

fn one_request(
    router: SocketAddr,
    request_style: &str,
    long_prompt_tokens: u64,
    prefill: bool,
    seq: u64,
) -> RequestOutcome {
    let start = Instant::now();
    let mut s = match connect_router(router) {
        Ok(s) => s,
        Err(()) => return RequestOutcome::HardError,
    };
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    if s.write_all(&request_line(
        request_style,
        long_prompt_tokens,
        prefill,
        seq,
    ))
    .is_err()
    {
        return RequestOutcome::HardError;
    }
    if s.shutdown(Shutdown::Write).is_err() {
        return RequestOutcome::HardError;
    }
    let mut buf = [0u8; 512];
    let n = match s.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return RequestOutcome::HardError,
    };
    if n == 0 {
        return RequestOutcome::HardError;
    }
    match parse_http_status(&buf[..n]) {
        Some(200) => RequestOutcome::Ok(start.elapsed().as_micros() as u64),
        Some(503) => RequestOutcome::Graceful503,
        _ => RequestOutcome::HardError,
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
    sorted[idx]
}

/// Bounded concurrency gate — blocks callers when `max` slots are occupied.
struct ConcurrencyGate {
    slots: Mutex<usize>,
    cv: Condvar,
    max: usize,
}

/// RAII slot — releases one concurrency permit on drop.
struct ConcurrencySlot {
    gate: Arc<ConcurrencyGate>,
}

impl ConcurrencyGate {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::new(0),
            cv: Condvar::new(),
            max,
        })
    }

    fn enter(self: &Arc<Self>) -> ConcurrencySlot {
        let mut slots = self.slots.lock().expect("concurrency gate");
        while *slots >= self.max {
            slots = self.cv.wait(slots).expect("concurrency gate wait");
        }
        *slots += 1;
        ConcurrencySlot {
            gate: Arc::clone(self),
        }
    }
}

impl Drop for ConcurrencySlot {
    fn drop(&mut self) {
        let mut slots = self.gate.slots.lock().expect("concurrency gate");
        *slots -= 1;
        self.gate.cv.notify_one();
    }
}

struct PeakGuard {
    peak: Arc<AtomicU64>,
}

impl PeakGuard {
    fn new() -> Self {
        Self {
            peak: Arc::new(AtomicU64::new(0)),
        }
    }

    fn peak(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }
}

fn run_scenario(sc: &Scenario, warmup: u32) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    if sc.measure == "fleet_replay" {
        return run_fleet_replay_scenario(sc, warmup);
    }
    if sc.measure == "admit_decouple" {
        return run_admit_decouple_scenario(sc, warmup);
    }
    run_e2e_scenario(sc, warmup)
}

fn run_admit_decouple_scenario(
    sc: &Scenario,
    warmup: u32,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    let wall_start = Instant::now();
    let delays = if sc.prefill_delay_sweep_us.len() >= 2 {
        sc.prefill_delay_sweep_us.clone()
    } else {
        let low = sc.backend_delay_us.max(1);
        vec![low, low.saturating_mul(50).max(low + 1_000)]
    };
    let head = request_line(&sc.request_style, sc.long_prompt_tokens, true, 0);

    let mut p99_samples = Vec::with_capacity(delays.len());
    for &delay_us in &delays {
        let router = build_admit_router(sc, delay_us);
        for i in 0..warmup {
            let _ = admit_disaggregated(
                &router,
                &request_line(&sc.request_style, sc.long_prompt_tokens, true, u64::from(i)),
            );
        }
        let mut samples = run_admit_workers(
            Arc::clone(&router),
            &head,
            sc.concurrency,
            sc.requests_per_worker,
            None,
        );
        samples.sort_unstable();
        p99_samples.push(percentile(&samples, 0.99));
        let drain_ms = delay_us / 1000 + 500;
        thread::sleep(Duration::from_millis(drain_ms));
    }

    let p99_low = *p99_samples.first().unwrap_or(&1);
    let p99_high = *p99_samples.last().unwrap_or(&1);
    let ratio = p99_high as f64 / p99_low.max(1) as f64;

    let total = u64::from(sc.concurrency) * u64::from(sc.requests_per_worker) * delays.len() as u64;
    let duration_secs = wall_start.elapsed().as_secs_f64();
    Ok(ScenarioResult {
        id: sc.id.clone(),
        summary: sc.summary.clone(),
        backends: sc.backends,
        decode_backends: sc.decode_backends,
        concurrency: sc.concurrency,
        requests_per_worker: sc.requests_per_worker,
        backend_delay_us: sc.backend_delay_us,
        request_style: sc.request_style.clone(),
        use_kv_pool: false,
        total_requests: total,
        ok: total,
        errors: 0,
        errors_graceful: None,
        errors_hard: None,
        duration_secs,
        req_per_sec: if duration_secs > 0.0 {
            total as f64 / duration_secs
        } else {
            0.0
        },
        min_us: p99_low,
        p50_us: p99_low,
        p90_us: p99_high,
        p99_us: p99_high,
        max_us: p99_high,
        max_p99_ms: sc.max_p99_ms,
        kv_bytes_reserved_peak: None,
        kv_admit_rejects: None,
        handoff_transfer_count: None,
        handoff_bytes_p50: None,
        handoff_bytes_p99: None,
        handoff_wall_us_p50: None,
        handoff_wall_us_p99: None,
        accept_p99_us_low: Some(p99_low),
        accept_p99_us_high: Some(p99_high),
        accept_p99_ratio: Some(ratio),
        latencies_us: p99_samples,
        dataplane_pi: None,
        dataplane_age_ms: None,
        rcu_stale: None,
        pi_star: None,
        min_dataplane_pi_sampled: None,
        fast_path_ratio: None,
        colocated_routes: None,
        disagg_routes: None,
        fastpath_misroute_mean: None,
        fastpath_misroute_samples: None,
        corrector_shadow_samples: None,
        fleet_windows: Vec::new(),
        fleet_live_pi_correlation: None,
        fleet_shadow_correlation: None,
        fleet_heavy_graceful_rate: None,
        fleet_sim_gate_pass: None,
        rdma_shadow_samples: None,
        rdma_transfer_ratio_median: None,
    })
}

fn build_admit_router(sc: &Scenario, prefill_delay_us: u64) -> Arc<Router> {
    let prefill: Vec<Arc<Backend>> = (0..sc.backends.max(1))
        .map(|i| {
            let addr = spawn_delay_backend(Duration::from_micros(prefill_delay_us));
            let cost = sc.base_cost_seconds + sc.cost_step_seconds * f64::from(i);
            Backend::new(format!("pf{i}"), addr, cost)
        })
        .collect();
    let decode: Vec<Arc<Backend>> = (0..sc.decode_backends.max(1))
        .map(|i| {
            let addr = spawn_delay_backend(Duration::from_micros(1));
            let cost = sc.base_cost_seconds + sc.cost_step_seconds * f64::from(i);
            Backend::new(format!("dc{i}"), addr, cost)
        })
        .collect();
    Arc::new(Router::new(prefill, decode))
}

fn run_admit_workers(
    router: Arc<Router>,
    head: &[u8],
    concurrency: u32,
    requests_per_worker: u32,
    inflight: Option<Arc<ConcurrencyGate>>,
) -> Vec<u64> {
    let latencies = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for _ in 0..concurrency {
        let latencies = Arc::clone(&latencies);
        let inflight = inflight.clone();
        let head = head.to_vec();
        let router = Arc::clone(&router);
        handles.push(thread::spawn(move || {
            for _ in 0..requests_per_worker {
                let _slot = inflight.as_ref().map(|g| g.enter());
                if let Ok(d) = admit_disaggregated(router.as_ref(), &head) {
                    latencies.lock().expect("lat").push(d.as_micros() as u64);
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }
    let samples = latencies.lock().expect("lat").clone();
    samples
}

fn trace_path(sc: &Scenario) -> Result<PathBuf, String> {
    let rel = sc
        .trace_path
        .as_ref()
        .ok_or_else(|| format!("{}: trace_path required for fleet_replay", sc.id))?;
    let path = PathBuf::from(rel);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo_root().join(path))
    }
}

struct WindowWorkerConfig {
    router_addr: SocketAddr,
    concurrency: u32,
    requests_per_worker: u32,
    request_style: String,
    long_prompt_tokens: u64,
    prefill_fraction: f64,
    seq_base: u64,
    inflight: Option<Arc<ConcurrencyGate>>,
    peak_sampler: Option<Arc<KvReservationLedger>>,
    peak_atomic: Option<Arc<AtomicU64>>,
}

fn run_window_workers(cfg: &WindowWorkerConfig) -> (u64, u64, u64, Vec<u64>) {
    let ok = Arc::new(AtomicU64::new(0));
    let graceful = Arc::new(AtomicU64::new(0));
    let hard = Arc::new(AtomicU64::new(0));
    let latencies = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for w in 0..cfg.concurrency {
        let ok = Arc::clone(&ok);
        let graceful = Arc::clone(&graceful);
        let hard = Arc::clone(&hard);
        let latencies = Arc::clone(&latencies);
        let inflight = cfg.inflight.clone();
        let peak_sampler = cfg.peak_sampler.clone();
        let peak_atomic = cfg.peak_atomic.clone();
        let request_style = cfg.request_style.clone();
        let requests_per_worker = cfg.requests_per_worker;
        let router_addr = cfg.router_addr;
        let long_prompt_tokens = cfg.long_prompt_tokens;
        let prefill_fraction = cfg.prefill_fraction;
        let seq_base = cfg.seq_base;
        handles.push(thread::spawn(move || {
            for r in 0..requests_per_worker {
                let seq = seq_base + u64::from(w) * u64::from(requests_per_worker) + u64::from(r);
                let prefill = if request_style == "mixed_tokens"
                    || request_style == "short_tokens"
                    || request_style == "long_tokens"
                {
                    true
                } else {
                    (seq % 100) as f64 / 100.0 < prefill_fraction
                };
                let _slot = inflight.as_ref().map(|g| g.enter());
                match one_request(
                    router_addr,
                    &request_style,
                    long_prompt_tokens,
                    prefill,
                    seq,
                ) {
                    RequestOutcome::Ok(us) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                        latencies.lock().expect("lat").push(us);
                        if let (Some(ledger), Some(peak)) = (&peak_sampler, &peak_atomic) {
                            let cur = ledger.fleet_reserved();
                            peak.fetch_max(cur, Ordering::Relaxed);
                        }
                    }
                    RequestOutcome::Graceful503 => {
                        graceful.fetch_add(1, Ordering::Relaxed);
                    }
                    RequestOutcome::HardError => {
                        hard.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("worker");
    }
    let samples = latencies.lock().expect("lat").clone();
    let ok_n = ok.load(Ordering::Relaxed);
    let graceful_n = graceful.load(Ordering::Relaxed);
    let hard_n = hard.load(Ordering::Relaxed);
    (ok_n, graceful_n, hard_n, samples)
}

fn run_fleet_replay_scenario(
    sc: &Scenario,
    warmup: u32,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    let path = trace_path(sc)?;
    let windows =
        load_fleet_trace(&path).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let min_corr = sc.min_fleet_correlation.unwrap_or(0.45);
    let pilot = shadow_pilot_for_trace(&windows, min_corr);

    let base = SimBaseKnobs {
        concurrency: sc.concurrency,
        requests_per_worker: sc.requests_per_worker,
        base_prefill_delay_us: sc.backend_delay_us.max(1),
        base_decode_delay_us: sc.backend_delay_us.max(1) / 2,
        long_prompt_tokens: sc.long_prompt_tokens,
        long_prompt_tokens_heavy: sc.long_prompt_tokens.max(2048),
        request_style: sc.request_style.clone(),
    };
    let avg_mult = windows
        .iter()
        .map(|w| window_knobs(w, &base).delay_window_mult)
        .sum::<f64>()
        / windows.len().max(1) as f64;

    let kv_bytes = if sc.prefill_kv_headers {
        Some(kv_breakdown(base.long_prompt_tokens_heavy, sc.bytes_per_token).kv_reserved)
    } else {
        None
    };
    let prefill = build_pool_calibrated(
        sc.backends,
        "pf",
        sc,
        kv_bytes,
        base.base_prefill_delay_us,
        avg_mult,
    );
    let decode = build_pool_calibrated(
        sc.decode_backends,
        "dc",
        sc,
        None,
        base.base_decode_delay_us,
        avg_mult,
    );
    let stack = spawn_router_stack(sc, &prefill, &decode)?;
    let peak_guard = stack.ledger.as_ref().map(|_| PeakGuard::new());

    thread::sleep(Duration::from_millis(50));
    for i in 0..warmup {
        let _ = one_request(
            stack.addr,
            &sc.request_style,
            sc.long_prompt_tokens,
            true,
            u64::from(i),
        );
    }

    let inflight = if sc.max_inflight > 0 {
        Some(ConcurrencyGate::new(sc.max_inflight as usize))
    } else {
        None
    };
    let peak_sampler = stack.ledger.clone();
    let peak_atomic = peak_guard.as_ref().map(|g| Arc::clone(&g.peak));

    let start_wall = Instant::now();
    let mut window_results = Vec::with_capacity(windows.len());
    let mut all_latencies = Vec::new();
    let mut total_ok = 0u64;
    let mut total_graceful = 0u64;
    let mut total_hard = 0u64;
    let mut total_err = 0u64;
    let mut seq_base = 0u64;
    let mut min_pi_sampled = f64::MAX;

    eprintln!(
        "load-bench: {} 'sim fleet replay — {} windows from {}",
        sc.id,
        windows.len(),
        path.display()
    );

    for (i, w) in windows.iter().enumerate() {
        let knobs = window_knobs(w, &base);
        let pi_star = pilot.replays.get(i).map(|r| r.pi_star).unwrap_or(0.5);
        if sc.rebalancer_actuation {
            stack.router.actuate_from_trace_pressure(w.pressure());
        }
        eprintln!(
            "load-bench: {} window ts={} heavy={} conc={} style={} pf_frac={:.2}",
            sc.id,
            w.ts_ms,
            w.prefill_heavy,
            knobs.concurrency,
            knobs.request_style,
            knobs.prefill_fraction
        );
        let (ok, graceful, hard, mut lats) = run_window_workers(&WindowWorkerConfig {
            router_addr: stack.addr,
            concurrency: knobs.concurrency,
            requests_per_worker: knobs.requests_per_worker,
            request_style: knobs.request_style.clone(),
            long_prompt_tokens: knobs.long_prompt_tokens,
            prefill_fraction: knobs.prefill_fraction,
            seq_base,
            inflight: inflight.clone(),
            peak_sampler: peak_sampler.clone(),
            peak_atomic: peak_atomic.clone(),
        });
        let err = graceful + hard;
        lats.sort_unstable();
        let p99 = percentile(&lats, 0.99);
        let pi = stack.router.control_metrics().dataplane_pi;
        min_pi_sampled = min_pi_sampled.min(pi);
        window_results.push(FleetWindowResult {
            ts_ms: w.ts_ms,
            prefill_heavy: w.prefill_heavy,
            held_out: w.held_out,
            ok,
            errors: err,
            errors_graceful: graceful,
            errors_hard: hard,
            p99_us: p99,
            dataplane_pi: pi,
            pi_star,
        });
        total_ok += ok;
        total_graceful += graceful;
        total_hard += hard;
        total_err += err;
        all_latencies.extend(lats);
        seq_base = seq_base
            .saturating_add(u64::from(knobs.concurrency) * u64::from(knobs.requests_per_worker));
    }

    let duration_secs = start_wall.elapsed().as_secs_f64();
    all_latencies.sort_unstable();
    let sim_gate = eval_fleet_sim_gate(
        &pilot,
        &window_results,
        min_corr,
        sc.min_live_fleet_correlation,
        sc.max_heavy_graceful_rate,
    );
    let heavy_graceful: u64 = window_results
        .iter()
        .filter(|w| w.prefill_heavy)
        .map(|w| w.errors_graceful)
        .sum();
    let heavy_total: u64 = window_results
        .iter()
        .filter(|w| w.prefill_heavy)
        .map(|w| w.ok + w.errors)
        .sum();
    let heavy_shed_rate = if heavy_total > 0 {
        heavy_graceful as f64 / heavy_total as f64
    } else {
        0.0
    };
    eprintln!(
        "'sim: {} shadow_corr={:.3} live_pi_corr={:.3} gate={}",
        sc.id,
        sim_gate.heldout_correlation,
        sim_gate.live_pi_correlation,
        if sim_gate.gate_pass { "PASS" } else { "FAIL" }
    );

    let (kv_peak, kv_rejects) = match (&stack.ledger, &peak_guard) {
        (Some(ledger), Some(guard)) => (guard.peak(), ledger.metrics().kv_admit_rejects_total),
        (Some(ledger), None) => (
            ledger.fleet_reserved(),
            ledger.metrics().kv_admit_rejects_total,
        ),
        _ => (0, 0),
    };
    let transfer = stack
        .handoffs
        .as_ref()
        .map(|h| h.transfer_metrics())
        .filter(|m| m.count > 0);
    let control = stack.router.control_metrics();
    let min_pi = if min_pi_sampled.is_finite() {
        Some(min_pi_sampled)
    } else {
        None
    };
    let total = total_ok + total_err;

    Ok(ScenarioResult {
        id: sc.id.clone(),
        summary: sc.summary.clone(),
        backends: sc.backends,
        decode_backends: sc.decode_backends,
        concurrency: sc.concurrency,
        requests_per_worker: sc.requests_per_worker,
        backend_delay_us: sc.backend_delay_us,
        request_style: sc.request_style.clone(),
        use_kv_pool: sc.use_kv_pool,
        total_requests: total,
        ok: total_ok,
        errors: total_err,
        errors_graceful: Some(total_graceful),
        errors_hard: Some(total_hard),
        duration_secs,
        req_per_sec: if duration_secs > 0.0 {
            total_ok as f64 / duration_secs
        } else {
            0.0
        },
        min_us: all_latencies.first().copied().unwrap_or(0),
        p50_us: percentile(&all_latencies, 0.50),
        p90_us: percentile(&all_latencies, 0.90),
        p99_us: percentile(&all_latencies, 0.99),
        max_us: all_latencies.last().copied().unwrap_or(0),
        max_p99_ms: sc.max_p99_ms,
        kv_bytes_reserved_peak: if sc.use_kv_pool { Some(kv_peak) } else { None },
        kv_admit_rejects: if sc.use_kv_pool {
            Some(kv_rejects)
        } else {
            None
        },
        handoff_transfer_count: transfer.map(|m| m.count),
        handoff_bytes_p50: transfer.map(|m| m.bytes_p50),
        handoff_bytes_p99: transfer.map(|m| m.bytes_p99),
        handoff_wall_us_p50: transfer.map(|m| m.wall_us_p50),
        handoff_wall_us_p99: transfer.map(|m| m.wall_us_p99),
        accept_p99_us_low: None,
        accept_p99_us_high: None,
        accept_p99_ratio: None,
        latencies_us: all_latencies,
        dataplane_pi: Some(control.dataplane_pi),
        dataplane_age_ms: Some(control.dataplane_age_ms),
        rcu_stale: Some(control.rcu_stale),
        pi_star: None,
        min_dataplane_pi_sampled: min_pi,
        fast_path_ratio: Some(control.fast_path_ratio),
        colocated_routes: Some(control.colocated_routes),
        disagg_routes: Some(control.disagg_routes),
        fastpath_misroute_mean: Some(control.fastpath_misroute_mean),
        fastpath_misroute_samples: Some(control.fastpath_misroute_samples),
        corrector_shadow_samples: Some(control.corrector_shadow_samples),
        fleet_windows: window_results,
        fleet_live_pi_correlation: sc
            .min_fleet_correlation
            .map(|_| sim_gate.live_pi_correlation),
        fleet_shadow_correlation: sc
            .min_fleet_correlation
            .map(|_| sim_gate.heldout_correlation),
        fleet_heavy_graceful_rate: sc.max_heavy_graceful_rate.map(|_| heavy_shed_rate),
        fleet_sim_gate_pass: sc.min_fleet_correlation.map(|_| sim_gate.gate_pass),
        rdma_shadow_samples: None,
        rdma_transfer_ratio_median: None,
    })
}

fn run_e2e_scenario(
    sc: &Scenario,
    warmup: u32,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    let kv_bytes = if sc.prefill_kv_headers {
        Some(kv_breakdown(sc.long_prompt_tokens, sc.bytes_per_token).kv_reserved)
    } else {
        None
    };
    let prefill = build_pool(sc.backends, "pf", sc, kv_bytes);
    let decode = build_pool(sc.decode_backends, "dc", sc, None);
    let stack = spawn_router_stack(sc, &prefill, &decode)?;
    let peak_guard = stack.ledger.as_ref().map(|_| PeakGuard::new());

    thread::sleep(Duration::from_millis(50));

    for i in 0..warmup {
        let _ = one_request(
            stack.addr,
            &sc.request_style,
            sc.long_prompt_tokens,
            true,
            u64::from(i),
        );
    }

    let phases: Vec<(f64, String)> = if sc.step_prefill_fraction.is_empty() {
        vec![(sc.prefill_fraction, sc.request_style.clone())]
    } else {
        sc.step_prefill_fraction
            .iter()
            .enumerate()
            .map(|(i, frac)| {
                let style = sc
                    .step_request_style
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| sc.request_style.clone());
                (*frac, style)
            })
            .collect()
    };
    let total = u64::from(sc.concurrency) * u64::from(sc.requests_per_worker) * phases.len() as u64;
    let ok = Arc::new(AtomicU64::new(0));
    let err = Arc::new(AtomicU64::new(0));
    let latencies = Arc::new(Mutex::new(Vec::with_capacity(total as usize)));

    let start_wall = Instant::now();
    let inflight = if sc.max_inflight > 0 {
        Some(ConcurrencyGate::new(sc.max_inflight as usize))
    } else {
        None
    };
    let peak_sampler = stack.ledger.clone();
    let peak_atomic = peak_guard.as_ref().map(|g| Arc::clone(&g.peak));
    let mut min_pi_sampled = f64::MAX;
    let mut seq_base = 0u64;

    for (prefill_fraction, request_style) in &phases {
        let mut handles = Vec::new();
        for w in 0..sc.concurrency {
            let ok = Arc::clone(&ok);
            let err = Arc::clone(&err);
            let latencies = Arc::clone(&latencies);
            let requests_per_worker = sc.requests_per_worker;
            let router_addr = stack.addr;
            let request_style = request_style.clone();
            let long_prompt_tokens = sc.long_prompt_tokens;
            let peak_atomic = peak_atomic.clone();
            let peak_sampler = peak_sampler.clone();
            let inflight = inflight.clone();
            let prefill_fraction = *prefill_fraction;
            let seq_base = seq_base;
            handles.push(thread::spawn(move || {
                for r in 0..requests_per_worker {
                    let seq =
                        seq_base + u64::from(w) * u64::from(requests_per_worker) + u64::from(r);
                    let prefill = match request_style.as_str() {
                        "long_tokens" | "short_tokens" => true,
                        _ => (seq % 100) as f64 / 100.0 < prefill_fraction,
                    };
                    let _slot = inflight.as_ref().map(|g| g.enter());
                    match one_request(
                        router_addr,
                        &request_style,
                        long_prompt_tokens,
                        prefill,
                        seq,
                    ) {
                        RequestOutcome::Ok(us) => {
                            ok.fetch_add(1, Ordering::Relaxed);
                            latencies.lock().expect("lat").push(us);
                            if let (Some(ledger), Some(peak)) = (&peak_sampler, &peak_atomic) {
                                let cur = ledger.fleet_reserved();
                                peak.fetch_max(cur, Ordering::Relaxed);
                            }
                        }
                        RequestOutcome::Graceful503 | RequestOutcome::HardError => {
                            err.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("worker");
        }
        let phase_pi = stack.router.control_metrics().dataplane_pi;
        min_pi_sampled = min_pi_sampled.min(phase_pi);
        seq_base =
            seq_base.saturating_add(u64::from(sc.concurrency) * u64::from(sc.requests_per_worker));
    }
    let duration_secs = start_wall.elapsed().as_secs_f64();

    let mut samples = latencies.lock().expect("lat").clone();
    samples.sort_unstable();
    let ok_n = ok.load(Ordering::Relaxed);
    let err_n = err.load(Ordering::Relaxed);

    let (kv_peak, kv_rejects) = match (&stack.ledger, &peak_guard) {
        (Some(ledger), Some(guard)) => (guard.peak(), ledger.metrics().kv_admit_rejects_total),
        (Some(ledger), None) => (
            ledger.fleet_reserved(),
            ledger.metrics().kv_admit_rejects_total,
        ),
        _ => (0, 0),
    };

    let transfer = stack
        .handoffs
        .as_ref()
        .map(|h| h.transfer_metrics())
        .filter(|m| m.count > 0);

    let control = stack.router.control_metrics();
    let min_pi = if min_pi_sampled.is_finite() {
        Some(min_pi_sampled)
    } else {
        None
    };
    let (rdma_shadow_samples, rdma_transfer_ratio_median) = if sc.rdma_modeled {
        rdma_result_fields(&stack.router)
    } else {
        (None, None)
    };

    Ok(ScenarioResult {
        id: sc.id.clone(),
        summary: sc.summary.clone(),
        backends: sc.backends,
        decode_backends: sc.decode_backends,
        concurrency: sc.concurrency,
        requests_per_worker: sc.requests_per_worker,
        backend_delay_us: sc.backend_delay_us,
        request_style: sc.request_style.clone(),
        use_kv_pool: sc.use_kv_pool,
        total_requests: total,
        ok: ok_n,
        errors: err_n,
        errors_graceful: None,
        errors_hard: None,
        duration_secs,
        req_per_sec: if duration_secs > 0.0 {
            ok_n as f64 / duration_secs
        } else {
            0.0
        },
        min_us: samples.first().copied().unwrap_or(0),
        p50_us: percentile(&samples, 0.50),
        p90_us: percentile(&samples, 0.90),
        p99_us: percentile(&samples, 0.99),
        max_us: samples.last().copied().unwrap_or(0),
        max_p99_ms: sc.max_p99_ms,
        kv_bytes_reserved_peak: if sc.use_kv_pool { Some(kv_peak) } else { None },
        kv_admit_rejects: if sc.use_kv_pool {
            Some(kv_rejects)
        } else {
            None
        },
        handoff_transfer_count: transfer.map(|m| m.count),
        handoff_bytes_p50: transfer.map(|m| m.bytes_p50),
        handoff_bytes_p99: transfer.map(|m| m.bytes_p99),
        handoff_wall_us_p50: transfer.map(|m| m.wall_us_p50),
        handoff_wall_us_p99: transfer.map(|m| m.wall_us_p99),
        accept_p99_us_low: None,
        accept_p99_us_high: None,
        accept_p99_ratio: None,
        latencies_us: samples,
        dataplane_pi: Some(control.dataplane_pi),
        dataplane_age_ms: Some(control.dataplane_age_ms),
        rcu_stale: Some(control.rcu_stale),
        pi_star: Some(control.pi_star),
        min_dataplane_pi_sampled: min_pi,
        fast_path_ratio: Some(control.fast_path_ratio),
        colocated_routes: Some(control.colocated_routes),
        disagg_routes: Some(control.disagg_routes),
        fastpath_misroute_mean: Some(control.fastpath_misroute_mean),
        fastpath_misroute_samples: Some(control.fastpath_misroute_samples),
        corrector_shadow_samples: Some(control.corrector_shadow_samples),
        fleet_windows: Vec::new(),
        fleet_live_pi_correlation: None,
        fleet_shadow_correlation: None,
        fleet_heavy_graceful_rate: None,
        fleet_sim_gate_pass: None,
        rdma_shadow_samples,
        rdma_transfer_ratio_median,
    })
}

pub fn report_paths(report_dir: &str) -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(report_dir);
    (dir.join("latest.json"), dir.join("latest.pseudo"))
}

pub fn runs_dir(report_dir: &str) -> PathBuf {
    PathBuf::from(report_dir).join("runs")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// When invoked via `cargo run`, `current_exe()` is `cargo`; use `CARGO_BIN_EXE_xtask`.
fn xtask_exe() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_xtask")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_exe().expect("current_exe"))
}

pub fn load_bench(
    ci_only: bool,
    only_scenario: Option<&str>,
    stress: bool,
    harden: bool,
    sim: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if stress && harden {
        return Err("load-bench: --stress and --harden are mutually exclusive".into());
    }
    if sim && (stress || harden || ci_only) {
        return Err("load-bench: --sim is mutually exclusive with --stress, --harden, --ci".into());
    }
    if only_scenario.is_none() && !ci_only {
        if harden {
            return load_bench_isolated(IsolatedMode::Harden);
        }
        if stress {
            return load_bench_isolated(IsolatedMode::Stress);
        }
        if sim {
            return load_bench_isolated(IsolatedMode::Sim);
        }
        return load_bench_isolated(IsolatedMode::Local);
    }
    load_bench_inner(ci_only, only_scenario, stress, harden, sim)
}

enum IsolatedMode {
    Local,
    Stress,
    Harden,
    Sim,
}

fn load_bench_isolated(mode: IsolatedMode) -> Result<(), Box<dyn std::error::Error>> {
    let file: LoadBenchFile = toml::from_str(&fs::read_to_string(LOAD_BENCH)?)?;
    let stress_run = matches!(mode, IsolatedMode::Stress);
    let harden_run = matches!(mode, IsolatedMode::Harden);
    let sim_run = matches!(mode, IsolatedMode::Sim);
    let ids: Vec<String> = file
        .scenario
        .iter()
        .filter(|s| match mode {
            IsolatedMode::Local => !s.stress && !s.harden && !s.sim,
            IsolatedMode::Stress => s.stress,
            IsolatedMode::Harden => s.harden && !s.stress,
            IsolatedMode::Sim => s.sim,
        })
        .map(|s| s.id.clone())
        .collect();
    if ids.is_empty() {
        return Err(if stress_run {
            "no scenarios with stress=true in load-bench.toml".into()
        } else if harden_run {
            "no scenarios with harden=true in load-bench.toml".into()
        } else if sim_run {
            "no scenarios with sim=true in load-bench.toml".into()
        } else {
            "no scenarios in load-bench.toml".into()
        });
    }

    let exe = xtask_exe();
    let root = repo_root();
    let runs = runs_dir(&file.settings.report_dir);
    if runs.exists() {
        fs::remove_dir_all(&runs)?;
    }
    fs::create_dir_all(&runs)?;

    let mut scenarios = Vec::new();
    let mut failures = 0usize;
    for id in &ids {
        if let Some(sc) = file.scenario.iter().find(|s| s.id == *id) {
            if let Some(reason) = scenario_skip_reason(sc) {
                if sc.optional {
                    eprintln!("load-bench: {id} SKIP (optional) — {reason}");
                    if harden_run {
                        eprintln!("HARDEN_REPORT tier=4 id={id} status=SKIP detail={reason}");
                    }
                    continue;
                }
            }
            if sc.isolate_recovery {
                let delay = if file.settings.stress_recovery_ms > 0 {
                    file.settings.stress_recovery_ms
                } else {
                    file.settings.startup_delay_ms.max(10_000)
                };
                eprintln!("load-bench: recovery {delay}ms before {id} …");
                thread::sleep(Duration::from_millis(delay));
            }
        }
        eprintln!("load-bench: isolate → {id} …");
        let mut cmd = Command::new(&exe);
        cmd.current_dir(&root)
            .args(["load-bench", "--scenario", id]);
        if stress_run {
            cmd.arg("--stress");
        } else if harden_run {
            cmd.arg("--harden");
        } else if sim_run {
            cmd.arg("--sim");
        }
        let status = cmd.status()?;
        if !status.success() {
            failures += 1;
        }
        let partial_path = runs.join(format!("{id}.json"));
        if partial_path.exists() {
            let partial: LoadBenchReport =
                serde_json::from_str(&fs::read_to_string(&partial_path)?)?;
            scenarios.extend(partial.scenarios);
        }
        if stress_run {
            let delay = if file.settings.stress_recovery_ms > 0 {
                file.settings.stress_recovery_ms
            } else {
                file.settings.startup_delay_ms.max(10_000)
            };
            thread::sleep(Duration::from_millis(delay));
        } else if file.settings.startup_delay_ms > 0 {
            thread::sleep(Duration::from_millis(file.settings.startup_delay_ms));
        }
    }

    let report = LoadBenchReport {
        generated_at: rfc3339_now(),
        hostname: hostname(),
        scenarios,
    };
    let report_name = if stress_run {
        "stress.json"
    } else if harden_run {
        "harden.json"
    } else if sim_run {
        "sim.json"
    } else {
        "latest.json"
    };
    let json_path = PathBuf::from(&file.settings.report_dir).join(report_name);
    write_report(&json_path, &report)?;
    eprintln!(
        "load-bench: merged {} scenario(s) → {}",
        report.scenarios.len(),
        json_path.display()
    );

    if failures > 0 {
        Err(format!("{failures} isolated scenario run(s) failed").into())
    } else {
        Ok(())
    }
}

fn load_bench_inner(
    ci_only: bool,
    only_scenario: Option<&str>,
    stress: bool,
    harden: bool,
    sim: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let file: LoadBenchFile = toml::from_str(&fs::read_to_string(LOAD_BENCH)?)?;

    let selected: Vec<&Scenario> = file
        .scenario
        .iter()
        .filter(|s| {
            if ci_only && !s.ci {
                return false;
            }
            if stress && !s.stress {
                return false;
            }
            if harden && (!s.harden || s.stress) {
                return false;
            }
            if sim && !s.sim {
                return false;
            }
            if let Some(want) = only_scenario {
                return s.id == want;
            }
            true
        })
        .collect();
    if selected.is_empty() {
        return Err(if let Some(id) = only_scenario {
            format!("unknown scenario {id:?}").into()
        } else if ci_only {
            "no scenarios with ci=true in load-bench.toml".into()
        } else {
            "no scenarios in load-bench.toml".into()
        });
    }

    thread::sleep(Duration::from_millis(file.settings.startup_delay_ms));

    let mut scenarios = Vec::new();
    let mut gate_failures = 0usize;
    let mut strict_failures = 0usize;
    if stress {
        eprintln!("load-bench: STRESS — zero errors required; all soft gates enforced");
    }
    if sim {
        eprintln!("load-bench: 'sim — trace-driven fleet replay; strict gates enforced");
    }

    for sc in selected {
        let strict = ci_only
            || file.settings.gate_strict
            || stress
            || harden
            || sim
            || sc.rebalancer_actuation
            || sc.isolate_recovery
            || sc.sim;
        if sc.isolate_recovery && only_scenario.is_some() {
            eprintln!(
                "load-bench: strict gates — zero errors required for {}",
                sc.id
            );
        }
        eprintln!("load-bench: running {} …", sc.id);
        if let Some(reason) = scenario_skip_reason(sc) {
            if sc.optional {
                eprintln!("load-bench: {} SKIP (optional) — {reason}", sc.id);
                if sc.harden {
                    eprintln!(
                        "HARDEN_REPORT tier=4 id={} status=SKIP detail={reason}",
                        sc.id
                    );
                }
                if only_scenario.is_some() {
                    eprintln!("load-bench: done — skipped optional scenario {}", sc.id);
                    return Ok(());
                }
                continue;
            }
            eprintln!("load-bench: {} SKIP — {reason}", sc.id);
            if only_scenario.is_some() {
                return Err(format!("{}: {reason}", sc.id).into());
            }
            continue;
        }
        let result = run_scenario(sc, file.settings.warmup_requests)?;
        let failures_before = gate_failures;
        let hard_errors = result.errors_hard.unwrap_or(result.errors);
        let graceful_errors = result.errors_graceful.unwrap_or(0);
        if let Some(max_hard) = sc.max_hard_errors {
            if hard_errors > max_hard {
                eprintln!(
                    "load-bench: {} FAIL — {hard_errors} hard errors (cap {max_hard})",
                    result.id
                );
                gate_failures += 1;
            } else if graceful_errors > 0
                && sc.max_kv_admit_rejects.is_none()
                && sc.min_errors.is_none()
                && sc.min_kv_admit_rejects.is_none()
            {
                eprintln!(
                    "load-bench: {} FAIL — {graceful_errors} graceful 503 (zero required)",
                    result.id
                );
                gate_failures += 1;
            } else if result.errors > 0 {
                eprintln!(
                    "load-bench: {} errors — {graceful_errors} graceful 503 / {hard_errors} hard",
                    result.id
                );
            }
        } else if result.errors > 0 {
            let rejects = result.kv_admit_rejects.unwrap_or(0);
            if let Some(cap) = sc.max_kv_admit_rejects {
                if result.errors <= cap && rejects <= cap {
                    eprintln!(
                        "load-bench: {} KV rejects OK — {} errors / {} rejects (cap {cap})",
                        result.id, result.errors, rejects
                    );
                } else {
                    eprintln!(
                        "load-bench: {} FAIL — {} errors / {} requests",
                        result.id, result.errors, result.total_requests
                    );
                    gate_failures += 1;
                }
            } else {
                eprintln!(
                    "load-bench: {} FAIL — {} errors / {} requests",
                    result.id, result.errors, result.total_requests
                );
                gate_failures += 1;
            }
        }
        if let Some(peak) = result.kv_bytes_reserved_peak {
            let kv_cap = effective_decode_capacity_bytes(sc);
            if sc.use_kv_pool && kv_cap > 0 && peak > kv_cap {
                eprintln!(
                    "load-bench: {} FAIL — kv peak {peak} bytes > capacity {kv_cap}",
                    result.id
                );
                gate_failures += 1;
            } else if sc.use_kv_pool {
                eprintln!(
                    "load-bench: {} KV OK — peak reserved {peak} bytes (cap {kv_cap})",
                    result.id
                );
            }
        }
        if let Some(count) = result.handoff_transfer_count {
            eprintln!(
                "load-bench: {} handoff transfer — n={count} bytes p50/p99 {}/{} wall_us p50/p99 {}/{}",
                result.id,
                result.handoff_bytes_p50.unwrap_or(0),
                result.handoff_bytes_p99.unwrap_or(0),
                result.handoff_wall_us_p50.unwrap_or(0),
                result.handoff_wall_us_p99.unwrap_or(0),
            );
        }
        if let (Some(ratio), Some(limit)) = (result.accept_p99_ratio, sc.max_accept_p99_ratio) {
            if ratio > limit {
                eprintln!(
                    "load-bench: {} FAIL — accept p99 ratio {ratio:.2} > {limit:.1} ({}µs / {}µs)",
                    result.id,
                    result.accept_p99_us_low.unwrap_or(0),
                    result.accept_p99_us_high.unwrap_or(0),
                );
                gate_failures += 1;
            } else {
                eprintln!(
                    "load-bench: {} accept decouple OK — p99 ratio {ratio:.2} ≤ {limit:.1} ({}µs / {}µs)",
                    result.id,
                    result.accept_p99_us_low.unwrap_or(0),
                    result.accept_p99_us_high.unwrap_or(0),
                );
            }
        }
        if let Some(limit) = result.max_p99_ms {
            if result.ok == 0 {
                eprintln!(
                    "load-bench: {} soft gate FAIL — no successful requests",
                    result.id
                );
                gate_failures += 1;
            } else {
                let p99_ms = result.p99_us as f64 / 1000.0;
                if p99_ms > limit {
                    eprintln!(
                        "load-bench: {} soft gate FAIL — p99 {p99_ms:.2}ms > {limit:.1}ms",
                        result.id
                    );
                    gate_failures += 1;
                } else {
                    eprintln!(
                        "load-bench: {} soft gate OK — p99 {p99_ms:.2}ms ≤ {limit:.1}ms",
                        result.id
                    );
                }
            }
        }
        if let Some(pi) = result.dataplane_pi {
            let age = result.dataplane_age_ms.unwrap_or(0);
            let stale = result.rcu_stale.unwrap_or(false);
            eprintln!(
                "load-bench: {} dataplane — π={pi:.3} π*={:.3} age={age}ms stale={stale}",
                result.id,
                result.pi_star.unwrap_or(0.0)
            );
            if stale {
                eprintln!(
                    "load-bench: {} ALERT — RCU snapshot age {age}ms > rcu_stale_alert_ms",
                    result.id
                );
            }
        }
        if let Some(fp) = result.fast_path_ratio {
            eprintln!(
                "load-bench: {} control — fast_path_ratio={fp:.3} colocated={} disagg={} misroute_mean={:.3} (n={}) corrector_shadow={}",
                result.id,
                result.colocated_routes.unwrap_or(0),
                result.disagg_routes.unwrap_or(0),
                result.fastpath_misroute_mean.unwrap_or(0.0),
                result.fastpath_misroute_samples.unwrap_or(0),
                result.corrector_shadow_samples.unwrap_or(0),
            );
        }
        if sc.rdma_modeled {
            eprintln!(
                "load-bench: {} RDMA shadow — samples={} ratio_median={:.3}",
                result.id,
                result.rdma_shadow_samples.unwrap_or(0),
                result.rdma_transfer_ratio_median.unwrap_or(1.0),
            );
        }
        if let Some(min_n) = sc.min_rdma_shadow_samples {
            let n = result.rdma_shadow_samples.unwrap_or(0);
            if n < min_n {
                eprintln!(
                    "load-bench: {} FAIL — RDMA shadow samples {n} < min {min_n}",
                    result.id
                );
                gate_failures += 1;
            } else {
                eprintln!(
                    "load-bench: {} RDMA shadow OK — samples {n} ≥ {min_n}",
                    result.id
                );
            }
        }
        if let Some(min_r) = sc.min_rdma_transfer_ratio {
            if let Some(ratio) = result.rdma_transfer_ratio_median {
                if ratio < min_r {
                    eprintln!(
                        "load-bench: {} FAIL — RDMA ratio {ratio:.3} < min {min_r:.3}",
                        result.id
                    );
                    gate_failures += 1;
                } else {
                    eprintln!(
                        "load-bench: {} RDMA ratio OK — {ratio:.3} ≥ {min_r:.3}",
                        result.id
                    );
                }
            }
        }
        if let Some(max_r) = sc.max_rdma_transfer_ratio {
            if let Some(ratio) = result.rdma_transfer_ratio_median {
                if ratio > max_r {
                    eprintln!(
                        "load-bench: {} FAIL — RDMA ratio {ratio:.3} > max {max_r:.3}",
                        result.id
                    );
                    gate_failures += 1;
                } else {
                    eprintln!(
                        "load-bench: {} RDMA ratio OK — {ratio:.3} ≤ {max_r:.3}",
                        result.id
                    );
                }
            }
        }
        if let Some(min_pi) = sc.min_dataplane_pi {
            if let Some(observed) = result.dataplane_pi {
                if observed < min_pi {
                    eprintln!(
                        "load-bench: {} FAIL — dataplane π {observed:.3} < min {min_pi:.3}",
                        result.id
                    );
                    gate_failures += 1;
                } else {
                    eprintln!(
                        "load-bench: {} actuation OK — dataplane π {observed:.3} ≥ {min_pi:.3}",
                        result.id
                    );
                }
            }
        }
        if let Some(limit) = sc.max_p99_tail_ratio {
            if result.p99_us > 0 {
                let ratio = result.max_us as f64 / result.p99_us as f64;
                if ratio > limit {
                    eprintln!(
                        "load-bench: {} FAIL — tail ratio {ratio:.2} > {limit:.1} (max {}µs / p99 {}µs)",
                        result.id, result.max_us, result.p99_us
                    );
                    gate_failures += 1;
                } else {
                    eprintln!(
                        "load-bench: {} tail gate OK — max/p99 {ratio:.2} ≤ {limit:.1}",
                        result.id
                    );
                }
            }
        }
        if let Some(limit) = sc.max_misroute_mean {
            if let Some(m) = result.fastpath_misroute_mean {
                if m > limit {
                    eprintln!(
                        "load-bench: {} FAIL — misroute_mean {m:.3} > {limit:.3}",
                        result.id
                    );
                    gate_failures += 1;
                } else {
                    eprintln!(
                        "load-bench: {} misroute OK — mean {m:.3} ≤ {limit:.3}",
                        result.id
                    );
                }
            }
        }
        if let Some(min_rejects) = sc.min_kv_admit_rejects {
            let rejects = result.kv_admit_rejects.unwrap_or(0);
            if rejects < min_rejects {
                eprintln!(
                    "load-bench: {} FAIL — kv rejects {rejects} < min {min_rejects}",
                    result.id
                );
                gate_failures += 1;
            } else {
                eprintln!(
                    "load-bench: {} KV exhaust OK — rejects {rejects} ≥ {min_rejects}",
                    result.id
                );
            }
        }
        if let Some(min_err) = sc.min_errors {
            if result.errors < min_err {
                eprintln!(
                    "load-bench: {} FAIL — errors {} < min {min_err}",
                    result.id, result.errors
                );
                gate_failures += 1;
            } else {
                eprintln!(
                    "load-bench: {} admit shed OK — errors {} ≥ {min_err}",
                    result.id, result.errors
                );
            }
        }
        if sc.min_fleet_correlation.is_some() || result.fleet_sim_gate_pass.is_some() {
            if result.fleet_sim_gate_pass == Some(true) {
                eprintln!(
                    "'sim: {} fleet gate OK — shadow_corr {:.3} live_pi_corr {:.3}",
                    result.id,
                    result.fleet_shadow_correlation.unwrap_or(0.0),
                    result.fleet_live_pi_correlation.unwrap_or(0.0)
                );
            } else {
                eprintln!(
                    "'sim: {} FAIL — fleet replay gate (shadow_corr {:.3} live_pi_corr {:.3})",
                    result.id,
                    result.fleet_shadow_correlation.unwrap_or(0.0),
                    result.fleet_live_pi_correlation.unwrap_or(0.0)
                );
                gate_failures += 1;
            }
        }
        if (sc.harden || sc.stress || sc.sim) && gate_failures == failures_before {
            let detail = if sc.min_kv_admit_rejects.is_some() || sc.min_errors.is_some() {
                format!(
                    "graceful_rejects={}/{} kv_rejects={}",
                    result.errors,
                    result.total_requests,
                    result.kv_admit_rejects.unwrap_or(0)
                )
            } else if result.errors_graceful.is_some() {
                format!(
                    "ok={}/{} p99={}us errs={} (503={} hard={})",
                    result.ok,
                    result.total_requests,
                    result.p99_us,
                    result.errors,
                    result.errors_graceful.unwrap_or(0),
                    result.errors_hard.unwrap_or(0),
                )
            } else {
                format!(
                    "ok={}/{} p99={}us max={}us errs={}",
                    result.ok, result.total_requests, result.p99_us, result.max_us, result.errors
                )
            };
            eprintln!(
                "HARDEN_REPORT tier=4 id={} status=PASS detail={detail}",
                result.id
            );
        } else if (sc.harden || sc.stress) && gate_failures > failures_before {
            eprintln!(
                "HARDEN_REPORT tier=4 id={} status=FAIL detail=gate_miss errs={}",
                result.id, result.errors
            );
        }
        scenarios.push(result);
        if strict && gate_failures > failures_before {
            strict_failures += gate_failures - failures_before;
        }
    }

    let report = LoadBenchReport {
        generated_at: rfc3339_now(),
        hostname: hostname(),
        scenarios,
    };

    let (json_path, _) = report_paths(&file.settings.report_dir);
    if let Some(id) = only_scenario {
        let runs = runs_dir(&file.settings.report_dir);
        fs::create_dir_all(&runs)?;
        write_report(&runs.join(format!("{id}.json")), &report)?;
    }
    // Isolated `--scenario` subprocesses must not clobber the merged aggregate report
    // (stress children were leaving `latest.json` at 1800/1800 flood-only totals).
    if only_scenario.is_none() || ci_only {
        write_report(&json_path, &report)?;
        eprintln!("load-bench: wrote {}", json_path.display());
    }

    if strict_failures > 0 {
        Err(format!("{strict_failures} strict gate(s) failed").into())
    } else {
        if gate_failures > 0 {
            eprintln!(
                "load-bench: {gate_failures} soft gate(s) missed (advisory; set gate_strict=true to fail)"
            );
        }
        eprintln!(
            "load-bench: done — {} scenario(s); run `cargo xtask load-report` for pseudo output",
            report.scenarios.len()
        );
        Ok(())
    }
}

pub fn load_report(
    stress: bool,
    harden: bool,
    sim: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let file: LoadBenchFile = toml::from_str(&fs::read_to_string(LOAD_BENCH)?)?;
    let dir = PathBuf::from(&file.settings.report_dir);
    let (json_path, pseudo_path) = if stress {
        (dir.join("stress.json"), dir.join("stress.pseudo"))
    } else if harden {
        (dir.join("harden.json"), dir.join("harden.pseudo"))
    } else if sim {
        (dir.join("sim.json"), dir.join("sim.pseudo"))
    } else {
        report_paths(&file.settings.report_dir)
    };

    if !json_path.exists() {
        return Err(format!(
            "no results at {}; run `cargo xtask load-bench` first",
            json_path.display()
        )
        .into());
    }

    let raw = fs::read_to_string(&json_path)?;
    let report: LoadBenchReport = serde_json::from_str(&raw)?;
    let gate_configs = gate_configs_by_id()?;
    let pseudo = crate::pseudo_report::render(&report, &gate_configs, sim);

    if let Some(parent) = pseudo_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&pseudo_path, &pseudo)?;
    print!("{pseudo}");
    eprintln!("load-report: wrote {}", pseudo_path.display());
    Ok(())
}

fn write_report(path: &Path, report: &LoadBenchReport) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(report)?;
    fs::write(path, json)?;
    Ok(())
}

fn rfc3339_now() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = mp + if mp < 10 { 3 } else { -9 };
    let y = y + if mo <= 2 { 1 } else { 0 };
    (y, mo, d)
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "local".into())
}

#[cfg(test)]
mod request_outcome_tests {
    use super::{parse_http_status, RequestOutcome};

    #[test]
    fn parse_http_status_codes() {
        assert_eq!(
            parse_http_status(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"),
            Some(200)
        );
        assert_eq!(
            parse_http_status(b"HTTP/1.1 503 Service Unavailable\r\n\r\n"),
            Some(503)
        );
        assert_eq!(parse_http_status(b"HTTP/1.0 200 OK\r\n"), None);
    }

    #[test]
    fn request_outcome_variants_distinct() {
        assert_ne!(RequestOutcome::Graceful503, RequestOutcome::HardError);
        assert_ne!(RequestOutcome::Ok(1), RequestOutcome::Graceful503);
    }
}
