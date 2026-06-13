//! Release-mode CPU gates for Demiurge hot paths.
//!
//! Thresholds live in `design/bench-gates.toml`.

use std::error::Error;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use demiurge_cost::{compose, BarrierFactor, Corrector, Discount, TimeCore};
use demiurge_router::{route, select, Backend, Router};
use serde::Deserialize;

const BENCH_GATES: &str = "design/bench-gates.toml";

#[derive(Debug, Deserialize)]
struct BenchGatesFile {
    settings: Settings,
    #[serde(default)]
    gate: Vec<Gate>,
}

#[derive(Debug, Deserialize)]
struct Settings {
    ci_slack: f64,
    samples: u32,
}

#[derive(Debug, Deserialize)]
struct Gate {
    id: String,
    #[allow(dead_code)]
    phase: u32,
    #[allow(dead_code)]
    summary: String,
    warmup_iters: u32,
    bench_iters: u32,
    max_median_ns: u64,
}

/// Median nanoseconds per iteration of `f` after warmup.
fn median_ns_per_op(warmup: u32, iters: u32, samples: u32, mut f: impl FnMut()) -> u64 {
    for _ in 0..warmup {
        f();
    }
    let mut sample_ns: Vec<u64> = (0..samples)
        .map(|_| {
            let start = Instant::now();
            for _ in 0..iters {
                f();
            }
            start.elapsed().as_nanos() as u64 / u64::from(iters)
        })
        .collect();
    sample_ns.sort_unstable();
    sample_ns[sample_ns.len() / 2]
}

fn bench_compose_8() {
    let core = TimeCore::new(0.05).expect("core");
    let barriers: Vec<_> = (0..4)
        .map(|i| BarrierFactor::new(1.0 + f64::from(i) * 0.05).expect("barrier"))
        .collect();
    let discounts: Vec<_> = (0..2)
        .map(|_| Discount::new(0.92).expect("discount"))
        .collect();
    let corr = Corrector::identity();
    std::hint::black_box(compose(core, &barriers, &discounts, corr));
}

fn bench_select_64() {
    let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let pool: Vec<Arc<Backend>> = (0..64)
        .map(|i| Backend::new(format!("b{i}"), addr, 0.01 + f64::from(i) * 0.000_5))
        .collect();
    std::hint::black_box(select(&pool));
}

fn bench_backend_cost() {
    let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    let b = Backend::new("b0", addr, 0.05);
    b.incr_inflight();
    std::hint::black_box(b.cost());
}

fn sample_router() -> Router {
    let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
    Router::new(
        vec![Backend::new("pf", addr, 0.01)],
        vec![Backend::new("dc", addr, 0.02)],
    )
}

const CLASSIFY_HEAD: &[u8] =
    b"GET /long/2048 HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 2048\r\n\r\n";

const SHORT_HEAD: &[u8] = b"GET / HTTP/1.1\r\nhost: x\r\nx-demiurge-tokens: 32\r\n\r\n";

fn bench_classify() {
    let router = sample_router();
    let _ = std::hint::black_box(route(&router, SHORT_HEAD));
}

fn bench_route_dispatch() {
    let router = sample_router();
    let _ = std::hint::black_box(route(&router, CLASSIFY_HEAD));
}

fn run_gate(gate: &Gate, settings: &Settings) -> Result<(u64, u64), Box<dyn Error>> {
    let measured = match gate.id.as_str() {
        "BENCH-COMPOSE-8" => median_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            bench_compose_8,
        ),
        "BENCH-SELECT-64" => median_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            bench_select_64,
        ),
        "BENCH-BACKEND-COST" => median_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            bench_backend_cost,
        ),
        "BENCH-CLASSIFY" => median_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            bench_classify,
        ),
        "BENCH-ROUTE-DISPATCH" => median_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            bench_route_dispatch,
        ),
        other => return Err(format!("unknown bench gate id {other:?}").into()),
    };

    let slack = if std::env::var("CI").is_ok() {
        settings.ci_slack
    } else {
        1.0
    };
    let limit = (gate.max_median_ns as f64 * slack).ceil() as u64;
    Ok((measured, limit))
}

pub fn bench_gate() -> Result<(), Box<dyn Error>> {
    let file: BenchGatesFile = toml::from_str(&fs::read_to_string(BENCH_GATES)?)?;
    if file.gate.is_empty() {
        return Err("no gates declared in bench-gates.toml".into());
    }

    let mut failures = 0usize;
    for gate in &file.gate {
        let (measured, limit) = run_gate(gate, &file.settings)?;
        if measured <= limit {
            println!(
                "bench-gate: {id} OK — median {measured} ns/op (limit {limit} ns)",
                id = gate.id,
            );
        } else {
            eprintln!(
                "bench-gate: {id} FAIL — median {measured} ns/op exceeds limit {limit} ns",
                id = gate.id,
            );
            failures += 1;
        }
    }

    if failures == 0 {
        println!("bench-gate: OK — {} gate(s) passed", file.gate.len());
        Ok(())
    } else {
        Err(format!("{failures} CPU bench gate(s) failed").into())
    }
}
