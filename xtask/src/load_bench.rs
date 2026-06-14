//! Local TCP load scenarios against a live demiurge-router stack.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use demiurge_cost::kv_breakdown;
use demiurge_router::{
    admit_disaggregated, serve, spawn_delay_backend, Backend, KvHandoffRegistry,
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
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoadBenchReport {
    pub generated_at: String,
    pub hostname: String,
    pub scenarios: Vec<ScenarioResult>,
}

struct RouterStack {
    addr: SocketAddr,
    router: Arc<Router>,
    ledger: Option<Arc<KvReservationLedger>>,
    handoffs: Option<Arc<KvHandoffRegistry>>,
}

fn spawn_mock_backend(delay_us: u64, kv_bytes: Option<u64>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let addr = listener.local_addr().expect("backend addr");
    thread::spawn(move || {
        static HANDLE: AtomicU64 = AtomicU64::new(1);
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            if delay_us > 0 {
                thread::sleep(Duration::from_micros(delay_us));
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

fn spawn_router_stack(
    sc: &Scenario,
    prefill: &[Arc<Backend>],
    decode: &[Arc<Backend>],
) -> RouterStack {
    if sc.use_kv_pool {
        let capacity = if sc.decode_capacity_bytes > 0 {
            sc.decode_capacity_bytes
        } else {
            let per = kv_breakdown(sc.long_prompt_tokens, sc.bytes_per_token).kv_reserved;
            per.saturating_mul(10)
        };
        let (router, ledger, handoffs) = Router::with_kv_pool(
            prefill.to_vec(),
            decode.to_vec(),
            capacity,
            sc.bytes_per_token,
        );
        let router = if sc.rebalancer_actuation {
            router.with_rebalancer_actuation(true)
        } else {
            router
        };
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind router");
        let addr = listener.local_addr().expect("router addr");
        let router = Arc::new(router);
        let serve_router = Arc::clone(&router);
        thread::spawn(move || {
            let _ = serve(listener, serve_router);
        });
        RouterStack {
            addr,
            router,
            ledger: Some(ledger),
            handoffs: Some(handoffs),
        }
    } else {
        let mut router = Router::new(prefill.to_vec(), decode.to_vec());
        if sc.rebalancer_actuation {
            router = router.with_rebalancer_actuation(true);
        }
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind router");
        let addr = listener.local_addr().expect("router addr");
        let router = Arc::new(router);
        let serve_router = Arc::clone(&router);
        thread::spawn(move || {
            let _ = serve(listener, serve_router);
        });
        RouterStack {
            addr,
            router,
            ledger: None,
            handoffs: None,
        }
    }
}

fn build_pool(count: u32, prefix: &str, sc: &Scenario, kv_bytes: Option<u64>) -> Vec<Arc<Backend>> {
    (0..count)
        .map(|i| {
            let addr = spawn_mock_backend(sc.backend_delay_us, kv_bytes);
            let cost = sc.base_cost_seconds + sc.cost_step_seconds * f64::from(i);
            let label = if sc.paired_labels {
                format!("node{i}")
            } else {
                format!("{prefix}{i}")
            };
            Backend::new(label, addr, cost)
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

fn one_request(
    router: SocketAddr,
    request_style: &str,
    long_prompt_tokens: u64,
    prefill: bool,
    seq: u64,
) -> Result<u64, ()> {
    let start = Instant::now();
    let mut s = connect_router(router)?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    s.write_all(&request_line(
        request_style,
        long_prompt_tokens,
        prefill,
        seq,
    ))
    .map_err(|_| ())?;
    s.shutdown(Shutdown::Write).map_err(|_| ())?;
    let mut buf = [0u8; 512];
    let n = s.read(&mut buf).map_err(|_| ())?;
    if n == 0 || !buf[..n].windows(12).any(|w| w == b"HTTP/1.1 200") {
        return Err(());
    }
    Ok(start.elapsed().as_micros() as u64)
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
    sorted[idx]
}

struct InflightGate {
    slots: Mutex<usize>,
    cv: Condvar,
    max: usize,
}

struct InflightGuard {
    gate: Arc<InflightGate>,
}

impl InflightGate {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::new(0),
            cv: Condvar::new(),
            max,
        })
    }

    fn enter(self: &Arc<Self>) -> InflightGuard {
        let mut slots = self.slots.lock().expect("inflight");
        while *slots >= self.max {
            slots = self.cv.wait(slots).expect("inflight wait");
        }
        *slots += 1;
        InflightGuard {
            gate: Arc::clone(self),
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut slots = self.gate.slots.lock().expect("inflight");
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
    if sc.measure == "admit_decouple" {
        return run_admit_decouple_scenario(sc, warmup);
    }
    run_e2e_scenario(sc, warmup)
}

fn run_admit_decouple_scenario(
    sc: &Scenario,
    warmup: u32,
) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
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
        duration_secs: 0.0,
        req_per_sec: 0.0,
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
    inflight: Option<Arc<InflightGate>>,
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
    let stack = spawn_router_stack(sc, &prefill, &decode);
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
        Some(InflightGate::new(sc.max_inflight as usize))
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
                        Ok(us) => {
                            ok.fetch_add(1, Ordering::Relaxed);
                            latencies.lock().expect("lat").push(us);
                            if let (Some(ledger), Some(peak)) = (&peak_sampler, &peak_atomic) {
                                let cur = ledger.fleet_reserved();
                                peak.fetch_max(cur, Ordering::Relaxed);
                            }
                        }
                        Err(()) => {
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
) -> Result<(), Box<dyn std::error::Error>> {
    if only_scenario.is_none() && !ci_only {
        if stress {
            return load_bench_isolated(IsolatedMode::Stress);
        }
        return load_bench_isolated(IsolatedMode::Local);
    }
    load_bench_inner(ci_only, only_scenario, stress)
}

enum IsolatedMode {
    Local,
    Stress,
}

fn load_bench_isolated(mode: IsolatedMode) -> Result<(), Box<dyn std::error::Error>> {
    let file: LoadBenchFile = toml::from_str(&fs::read_to_string(LOAD_BENCH)?)?;
    let stress_run = matches!(mode, IsolatedMode::Stress);
    let ids: Vec<String> = file
        .scenario
        .iter()
        .filter(|s| match mode {
            IsolatedMode::Local => !s.stress,
            IsolatedMode::Stress => s.stress,
        })
        .map(|s| s.id.clone())
        .collect();
    if ids.is_empty() {
        return Err(if stress_run {
            "no scenarios with stress=true in load-bench.toml".into()
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
) -> Result<(), Box<dyn std::error::Error>> {
    let file: LoadBenchFile = toml::from_str(&fs::read_to_string(LOAD_BENCH)?)?;

    let selected: Vec<&Scenario> = file
        .scenario
        .iter()
        .filter(|s| {
            if ci_only && !s.ci {
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

    for sc in selected {
        let strict = ci_only
            || file.settings.gate_strict
            || stress
            || sc.rebalancer_actuation
            || sc.isolate_recovery;
        if sc.isolate_recovery && only_scenario.is_some() {
            eprintln!(
                "load-bench: strict gates — zero errors required for {}",
                sc.id
            );
        }
        eprintln!("load-bench: running {} …", sc.id);
        let result = run_scenario(sc, file.settings.warmup_requests)?;
        let failures_before = gate_failures;
        if result.errors > 0 {
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
            if sc.use_kv_pool && sc.decode_capacity_bytes > 0 && peak > sc.decode_capacity_bytes {
                eprintln!(
                    "load-bench: {} FAIL — kv peak {peak} bytes > capacity {}",
                    result.id, sc.decode_capacity_bytes
                );
                gate_failures += 1;
            } else if sc.use_kv_pool {
                eprintln!(
                    "load-bench: {} KV OK — peak reserved {peak} bytes (cap {})",
                    result.id,
                    if sc.decode_capacity_bytes > 0 {
                        sc.decode_capacity_bytes.to_string()
                    } else {
                        "auto".into()
                    }
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
    write_report(&json_path, &report)?;
    eprintln!("load-bench: wrote {}", json_path.display());

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

pub fn load_report(stress: bool) -> Result<(), Box<dyn std::error::Error>> {
    let file: LoadBenchFile = toml::from_str(&fs::read_to_string(LOAD_BENCH)?)?;
    let dir = PathBuf::from(&file.settings.report_dir);
    let (json_path, pseudo_path) = if stress {
        (dir.join("stress.json"), dir.join("stress.pseudo"))
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
    let pseudo = crate::pseudo_report::render(&report);

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
