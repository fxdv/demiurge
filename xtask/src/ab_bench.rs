//! A/B routing-policy benchmark: demiurge cost routing vs baseline policies.
//!
//! Runs the *same* workload through four arms and compares tail latency and
//! throughput:
//!
//! | arm           | selection policy                                    |
//! |---|---|
//! | `cost`        | the real `demiurge-router` serve path (min-cost)    |
//! | `least_conn`  | baseline proxy, fewest in-flight connections        |
//! | `round_robin` | baseline proxy, strict rotation                     |
//! | `random`      | baseline proxy, uniform random                      |
//!
//! The baseline arms use an intentionally minimal proxy (pick → connect →
//! pipe) so the comparison isolates the *placement policy*; the cost arm runs
//! the full router stack, so any router overhead counts *against* it — a
//! conservative comparison. Backends default to local mocks with a tiered
//! (heterogeneous) delay profile, the regime where placement policy matters.
//! Pass `--backends label@host:port@seconds,...` to drive a real fleet (e.g.
//! vLLM instances) with the identical protocol.

use std::io::Write as IoWrite;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use demiurge_router::{serve, Backend, Router};
use serde::Serialize;

use crate::load_bench::{one_request, percentile, spawn_mock_backend, RequestOutcome};

// ─── Configuration ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AbConfig {
    /// Local mock fleet size (ignored when `backend_spec` is set).
    pub backends: u32,
    /// Base mock service delay (µs); tiers scale it 1×..4× across the fleet.
    pub base_delay_us: u64,
    pub concurrency: u32,
    pub requests_per_worker: u32,
    /// `label@host:port@seconds,...` — drive a real fleet instead of mocks.
    pub backend_spec: Option<String>,
    pub report_dir: String,
}

impl Default for AbConfig {
    fn default() -> Self {
        Self {
            backends: 6,
            base_delay_us: 800,
            concurrency: 16,
            requests_per_worker: 60,
            backend_spec: None,
            report_dir: "target/ab-bench".into(),
        }
    }
}

// ─── Policies ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Cost,
    LeastConn,
    RoundRobin,
    Random,
}

impl Policy {
    const ALL: [Policy; 4] = [
        Policy::Cost,
        Policy::LeastConn,
        Policy::RoundRobin,
        Policy::Random,
    ];

    fn name(self) -> &'static str {
        match self {
            Policy::Cost => "cost",
            Policy::LeastConn => "least_conn",
            Policy::RoundRobin => "round_robin",
            Policy::Random => "random",
        }
    }
}

/// Baseline proxy state: per-backend in-flight counters + rotation cursor.
struct BaselinePool {
    addrs: Vec<SocketAddr>,
    inflight: Vec<AtomicUsize>,
    cursor: AtomicUsize,
    rng: AtomicU64,
}

impl BaselinePool {
    fn new(addrs: Vec<SocketAddr>) -> Arc<Self> {
        let inflight = addrs.iter().map(|_| AtomicUsize::new(0)).collect();
        Arc::new(Self {
            addrs,
            inflight,
            cursor: AtomicUsize::new(0),
            rng: AtomicU64::new(0x9E3779B97F4A7C15),
        })
    }

    fn pick(&self, policy: Policy) -> usize {
        match policy {
            Policy::RoundRobin => self.cursor.fetch_add(1, Ordering::Relaxed) % self.addrs.len(),
            Policy::LeastConn => self
                .inflight
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.load(Ordering::Relaxed))
                .map(|(i, _)| i)
                .unwrap_or(0),
            Policy::Random => {
                // xorshift64* — deterministic, seedable, dependency-free.
                let mut x = self.rng.load(Ordering::Relaxed);
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                self.rng.store(x, Ordering::Relaxed);
                (x.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as usize % self.addrs.len()
            }
            Policy::Cost => unreachable!("cost arm uses the real router"),
        }
    }
}

/// Minimal baseline proxy: pick per policy, forward the head, pipe the reply.
fn spawn_baseline_proxy(pool: Arc<BaselinePool>, policy: Policy) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind baseline proxy");
    let addr = listener.local_addr().expect("proxy addr");
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut client) = conn else { continue };
            let pool = Arc::clone(&pool);
            thread::spawn(move || {
                let idx = pool.pick(policy);
                pool.inflight[idx].fetch_add(1, Ordering::Relaxed);
                let result = proxy_once(&mut client, pool.addrs[idx]);
                pool.inflight[idx].fetch_sub(1, Ordering::Relaxed);
                if result.is_err() {
                    let _ = client.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n",
                    );
                }
            });
        }
    });
    addr
}

fn proxy_once(client: &mut TcpStream, backend: SocketAddr) -> std::io::Result<()> {
    use std::io::Read;
    let mut head = [0u8; 4096];
    let n = client.read(&mut head)?;
    let mut upstream = TcpStream::connect(backend)?;
    upstream.write_all(&head[..n])?;
    upstream.shutdown(std::net::Shutdown::Write)?;
    let mut up_read = upstream;
    std::io::copy(&mut up_read, client)?;
    let _ = client.shutdown(std::net::Shutdown::Write);
    Ok(())
}

// ─── Arm execution ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ArmResult {
    pub policy: String,
    pub ok: u64,
    pub errors: u64,
    pub duration_secs: f64,
    pub req_per_sec: f64,
    pub p50_us: u64,
    pub p90_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

#[derive(Debug, Serialize)]
pub struct AbReport {
    pub backends: u32,
    pub base_delay_us: u64,
    pub concurrency: u32,
    pub requests_per_worker: u32,
    pub external_fleet: bool,
    pub arms: Vec<ArmResult>,
}

/// Tiered heterogeneous delay: thirds of the fleet at 1×, 2×, 4× base delay.
fn tier_delay(base_us: u64, i: u32, count: u32) -> u64 {
    let tier = i * 3 / count.max(1);
    base_us << tier.min(2)
}

/// One mock fleet per arm (fresh sockets, identical delay profile).
fn build_mock_fleet(cfg: &AbConfig) -> Vec<(String, SocketAddr, f64)> {
    (0..cfg.backends)
        .map(|i| {
            let delay = tier_delay(cfg.base_delay_us, i, cfg.backends);
            let addr = spawn_mock_backend(delay, 0, None);
            // Base cost mirrors the true service time so the cost arm's
            // analytic core is honest, exactly as a calibrated deployment.
            (format!("b{i}"), addr, delay as f64 / 1_000_000.0)
        })
        .collect()
}

fn parse_external_fleet(spec: &str) -> Result<Vec<(String, SocketAddr, f64)>, String> {
    let mut out = Vec::new();
    for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let parts: Vec<&str> = item.split('@').collect();
        if parts.len() != 3 {
            return Err(format!(
                "bad backend {item:?}; want label@host:port@seconds"
            ));
        }
        let addr = parts[1]
            .parse()
            .map_err(|e| format!("bad addr {:?}: {e}", parts[1]))?;
        let secs = parts[2]
            .parse()
            .map_err(|e| format!("bad seconds {:?}: {e}", parts[2]))?;
        out.push((parts[0].to_string(), addr, secs));
    }
    if out.is_empty() {
        return Err("empty --backends spec".into());
    }
    Ok(out)
}

fn run_arm(cfg: &AbConfig, policy: Policy) -> Result<ArmResult, String> {
    let fleet = match &cfg.backend_spec {
        Some(spec) => parse_external_fleet(spec)?,
        None => build_mock_fleet(cfg),
    };

    let front = match policy {
        Policy::Cost => {
            let pool: Vec<Arc<Backend>> = fleet
                .iter()
                .map(|(label, addr, secs)| Backend::new(label.clone(), *addr, *secs))
                .collect();
            let router = Arc::new(Router::new(pool, vec![]));
            let listener =
                TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind router: {e}"))?;
            let addr = listener.local_addr().map_err(|e| e.to_string())?;
            thread::spawn(move || {
                let _ = serve(listener, router);
            });
            addr
        }
        baseline => {
            let addrs = fleet.iter().map(|(_, addr, _)| *addr).collect();
            spawn_baseline_proxy(BaselinePool::new(addrs), baseline)
        }
    };
    thread::sleep(Duration::from_millis(50));

    // Warmup: prime listener backlogs and (cost arm) inflight signals.
    for i in 0..8 {
        let _ = one_request(front, "short_tokens", 0, true, i);
    }

    let ok = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let latencies = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    let mut handles = Vec::new();
    for w in 0..cfg.concurrency {
        let ok = Arc::clone(&ok);
        let errors = Arc::clone(&errors);
        let latencies = Arc::clone(&latencies);
        let requests = cfg.requests_per_worker;
        handles.push(thread::spawn(move || {
            for r in 0..requests {
                let seq = u64::from(w) * u64::from(requests) + u64::from(r);
                match one_request(front, "short_tokens", 0, true, seq) {
                    RequestOutcome::Ok(us) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                        latencies.lock().expect("lat").push(us);
                    }
                    RequestOutcome::Graceful503 | RequestOutcome::HardError => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "worker panicked".to_string())?;
    }
    let duration_secs = started.elapsed().as_secs_f64();

    let mut samples = latencies.lock().expect("lat").clone();
    samples.sort_unstable();
    let ok_n = ok.load(Ordering::Relaxed);
    Ok(ArmResult {
        policy: policy.name().into(),
        ok: ok_n,
        errors: errors.load(Ordering::Relaxed),
        duration_secs,
        req_per_sec: if duration_secs > 0.0 {
            ok_n as f64 / duration_secs
        } else {
            0.0
        },
        p50_us: percentile(&samples, 0.50),
        p90_us: percentile(&samples, 0.90),
        p99_us: percentile(&samples, 0.99),
        max_us: samples.last().copied().unwrap_or(0),
    })
}

// ─── Entry point & report ────────────────────────────────────────────────────

pub fn ab_bench(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = AbConfig::default();
    let mut i = 0;
    while i < args.len() {
        let take = |i: &mut usize| -> Option<&String> {
            *i += 1;
            args.get(*i)
        };
        match args[i].as_str() {
            "--backends-count" => {
                cfg.backends = take(&mut i)
                    .ok_or("--backends-count needs a value")?
                    .parse()?;
            }
            "--base-delay-us" => {
                cfg.base_delay_us = take(&mut i)
                    .ok_or("--base-delay-us needs a value")?
                    .parse()?;
            }
            "--concurrency" => {
                cfg.concurrency = take(&mut i).ok_or("--concurrency needs a value")?.parse()?;
            }
            "--requests" => {
                cfg.requests_per_worker =
                    take(&mut i).ok_or("--requests needs a value")?.parse()?;
            }
            "--backends" => {
                cfg.backend_spec = Some(take(&mut i).ok_or("--backends needs a value")?.clone());
            }
            other => return Err(format!("ab-bench: unknown flag {other:?}").into()),
        }
        i += 1;
    }

    eprintln!(
        "ab-bench: {} backends (tiered {}µs..{}µs), {}×{} requests per arm",
        cfg.backends,
        cfg.base_delay_us,
        tier_delay(
            cfg.base_delay_us,
            cfg.backends.saturating_sub(1),
            cfg.backends
        ),
        cfg.concurrency,
        cfg.requests_per_worker,
    );

    let mut arms = Vec::new();
    for policy in Policy::ALL {
        eprintln!("ab-bench: running arm `{}` …", policy.name());
        arms.push(run_arm(&cfg, policy)?);
        thread::sleep(Duration::from_millis(200));
    }

    let report = AbReport {
        backends: cfg.backends,
        base_delay_us: cfg.base_delay_us,
        concurrency: cfg.concurrency,
        requests_per_worker: cfg.requests_per_worker,
        external_fleet: cfg.backend_spec.is_some(),
        arms,
    };

    println!(
        "\n  {:<12} {:>7} {:>6} {:>10} {:>9} {:>9} {:>9} {:>9}",
        "policy", "ok", "err", "req/s", "p50 µs", "p90 µs", "p99 µs", "max µs"
    );
    println!("  {}", "─".repeat(78));
    let cost_p99 = report
        .arms
        .iter()
        .find(|a| a.policy == "cost")
        .map(|a| a.p99_us)
        .unwrap_or(0);
    for a in &report.arms {
        let delta = if a.policy != "cost" && cost_p99 > 0 && a.p99_us > 0 {
            format!(
                "  ({:+.1}% p99 vs cost)",
                (a.p99_us as f64 / cost_p99 as f64 - 1.0) * 100.0
            )
        } else {
            String::new()
        };
        println!(
            "  {:<12} {:>7} {:>6} {:>10.1} {:>9} {:>9} {:>9} {:>9}{delta}",
            a.policy, a.ok, a.errors, a.req_per_sec, a.p50_us, a.p90_us, a.p99_us, a.max_us
        );
    }

    let dir = PathBuf::from(&cfg.report_dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("latest.json");
    std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
    eprintln!("\nab-bench: wrote {}", path.display());
    Ok(())
}
