//! Local TCP load scenarios against a live demiurge-router stack.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use demiurge_router::{serve, Backend, Router};
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
    #[serde(default)]
    gate_strict: bool,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    id: String,
    summary: String,
    #[serde(default)]
    ci: bool,
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
}

fn default_prefill_fraction() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub id: String,
    pub summary: String,
    pub backends: u32,
    pub concurrency: u32,
    pub requests_per_worker: u32,
    pub backend_delay_us: u64,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub latencies_us: Vec<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoadBenchReport {
    pub generated_at: String,
    pub hostname: String,
    pub scenarios: Vec<ScenarioResult>,
}

fn spawn_mock_backend(delay_us: u64) -> SocketAddr {
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
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
            let _ = s.shutdown(Shutdown::Write);
        }
    });
    addr
}

fn spawn_router(prefill: &[Arc<Backend>], decode: &[Arc<Backend>]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind router");
    let addr = listener.local_addr().expect("router addr");
    let router = Arc::new(Router::new(prefill.to_vec(), decode.to_vec()));
    thread::spawn(move || {
        let _ = serve(listener, router);
    });
    addr
}

fn build_pool(count: u32, prefix: &str, sc: &Scenario) -> Vec<Arc<Backend>> {
    (0..count)
        .map(|i| {
            let addr = spawn_mock_backend(sc.backend_delay_us);
            let cost = sc.base_cost_seconds + sc.cost_step_seconds * f64::from(i);
            Backend::new(format!("{prefix}{i}"), addr, cost)
        })
        .collect()
}

fn request_line(prefill: bool, seq: u64) -> Vec<u8> {
    if prefill {
        format!("GET /prefill/{seq} HTTP/1.1\r\nhost: load-bench\r\nconnection: close\r\n\r\n")
            .into_bytes()
    } else {
        format!(
            "GET /decode/{seq} HTTP/1.1\r\nhost: load-bench\r\nx-demiurge-phase: decode\r\nconnection: close\r\n\r\n"
        )
        .into_bytes()
    }
}

fn one_request(router: SocketAddr, prefill: bool, seq: u64) -> Result<u64, ()> {
    let start = Instant::now();
    let mut s = TcpStream::connect(router).map_err(|_| ())?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    s.write_all(&request_line(prefill, seq)).map_err(|_| ())?;
    s.shutdown(Shutdown::Write).map_err(|_| ())?;
    let mut buf = [0u8; 512];
    let _ = s.read(&mut buf).map_err(|_| ())?;
    Ok(start.elapsed().as_micros() as u64)
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
    sorted[idx]
}

fn run_scenario(sc: &Scenario, warmup: u32) -> Result<ScenarioResult, Box<dyn std::error::Error>> {
    let prefill = build_pool(sc.backends, "pf", sc);
    let decode = build_pool(sc.decode_backends, "dc", sc);
    let router_addr = spawn_router(&prefill, &decode);
    thread::sleep(Duration::from_millis(50));

    for i in 0..warmup {
        let _ = one_request(router_addr, true, u64::from(i));
    }

    let total = u64::from(sc.concurrency) * u64::from(sc.requests_per_worker);
    let ok = Arc::new(AtomicU64::new(0));
    let err = Arc::new(AtomicU64::new(0));
    let latencies = Arc::new(std::sync::Mutex::new(Vec::with_capacity(total as usize)));

    let start_wall = Instant::now();
    let mut handles = Vec::new();
    for w in 0..sc.concurrency {
        let ok = Arc::clone(&ok);
        let err = Arc::clone(&err);
        let latencies = Arc::clone(&latencies);
        let prefill_fraction = sc.prefill_fraction;
        let requests_per_worker = sc.requests_per_worker;
        handles.push(thread::spawn(move || {
            for r in 0..requests_per_worker {
                let seq = u64::from(w) * u64::from(requests_per_worker) + u64::from(r);
                let prefill = (seq % 100) as f64 / 100.0 < prefill_fraction;
                match one_request(router_addr, prefill, seq) {
                    Ok(us) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                        latencies.lock().unwrap().push(us);
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
    let duration_secs = start_wall.elapsed().as_secs_f64();

    let mut samples = latencies.lock().unwrap().clone();
    samples.sort_unstable();
    let ok_n = ok.load(Ordering::Relaxed);
    let err_n = err.load(Ordering::Relaxed);

    Ok(ScenarioResult {
        id: sc.id.clone(),
        summary: sc.summary.clone(),
        backends: sc.backends,
        concurrency: sc.concurrency,
        requests_per_worker: sc.requests_per_worker,
        backend_delay_us: sc.backend_delay_us,
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
        latencies_us: samples,
    })
}

pub fn report_paths(report_dir: &str) -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(report_dir);
    (dir.join("latest.json"), dir.join("latest.pseudo"))
}

pub fn load_bench(ci_only: bool) -> Result<(), Box<dyn std::error::Error>> {
    let file: LoadBenchFile = toml::from_str(&fs::read_to_string(LOAD_BENCH)?)?;

    let selected: Vec<&Scenario> = if ci_only {
        file.scenario.iter().filter(|s| s.ci).collect()
    } else {
        file.scenario.iter().collect()
    };
    if selected.is_empty() {
        return Err(if ci_only {
            "no scenarios with ci=true in load-bench.toml".into()
        } else {
            "no scenarios in load-bench.toml".into()
        });
    }

    thread::sleep(Duration::from_millis(file.settings.startup_delay_ms));

    let mut scenarios = Vec::new();
    let mut gate_failures = 0usize;
    // CI regression runs always enforce gates; local full runs use settings.gate_strict.
    let strict = ci_only || file.settings.gate_strict;

    for sc in selected {
        eprintln!("load-bench: running {} …", sc.id);
        let result = run_scenario(sc, file.settings.warmup_requests)?;
        if result.errors > 0 {
            eprintln!(
                "load-bench: {} FAIL — {} errors / {} requests",
                result.id, result.errors, result.total_requests
            );
            gate_failures += 1;
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
        scenarios.push(result);
    }

    let report = LoadBenchReport {
        generated_at: rfc3339_now(),
        hostname: hostname(),
        scenarios,
    };

    let (json_path, _) = report_paths(&file.settings.report_dir);
    write_report(&json_path, &report)?;
    eprintln!("load-bench: wrote {}", json_path.display());

    if strict && gate_failures > 0 {
        Err(format!("{gate_failures} scenario soft gate(s) failed (gate_strict=true)").into())
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

pub fn load_report() -> Result<(), Box<dyn std::error::Error>> {
    let file: LoadBenchFile = toml::from_str(&fs::read_to_string(LOAD_BENCH)?)?;
    let (json_path, pseudo_path) = report_paths(&file.settings.report_dir);

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
    // Good enough for local bench reports (UTC, no leap seconds).
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
