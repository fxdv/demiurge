//! Release-mode CPU gates for Demiurge hot paths.
//!
//! Thresholds live in `design/bench-gates.toml`.
//! Run `cargo xtask bench-probe` to measure floor/median/p95 and find thin gates.

use std::error::Error;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use demiurge_control::{greedy_pair, PoolPressure, PoolRebalancer, RebalancerMode, ScoredBackend};
use demiurge_cost::{
    compose, kv_breakdown, phi_barrier_marginal, BarrierFactor, Corrector, Discount, TimeCore,
};
use demiurge_dataplane::RcuRoutingTable;
use demiurge_router::{
    estimate_prompt_tokens, parse_prompt_tokens, route, select, Backend, Router,
};
use demiurge_state::{default_routing_blocks, WarmthMap};
use serde::Deserialize;

const BENCH_GATES: &str = "design/bench-gates.toml";

#[derive(Debug, Clone, Deserialize)]
struct BenchGatesFile {
    settings: Settings,
    #[serde(default)]
    gate: Vec<Gate>,
}

#[derive(Debug, Clone, Deserialize)]
struct Settings {
    ci_slack: f64,
    samples: u32,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Copy)]
struct SampleStats {
    floor_ns: u64,
    median_ns: u64,
    p95_ns: u64,
}

/// Nanoseconds per iteration of `f` after warmup; returns distribution over `samples` runs.
fn sample_ns_per_op(warmup: u32, iters: u32, samples: u32, mut f: impl FnMut()) -> SampleStats {
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
    let median_ns = sample_ns[sample_ns.len() / 2];
    let p95_idx = ((sample_ns.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
    SampleStats {
        floor_ns: *sample_ns.first().unwrap_or(&median_ns),
        median_ns,
        p95_ns: sample_ns[p95_idx],
    }
}

struct ComposeBench {
    core: TimeCore,
    barriers: Vec<BarrierFactor>,
    discounts: Vec<Discount>,
    corr: Corrector,
}

impl ComposeBench {
    fn new() -> Self {
        Self {
            core: TimeCore::new(0.05).expect("core"),
            barriers: (0..4)
                .map(|i| BarrierFactor::new(1.0 + f64::from(i) * 0.05).expect("barrier"))
                .collect(),
            discounts: (0..2)
                .map(|_| Discount::new(0.92).expect("discount"))
                .collect(),
            corr: Corrector::identity(),
        }
    }

    fn run(&self) {
        std::hint::black_box(compose(
            self.core,
            &self.barriers,
            &self.discounts,
            self.corr,
        ));
    }
}

struct SelectBench {
    pool: Vec<Arc<Backend>>,
}

impl SelectBench {
    fn with_size(n: usize) -> Self {
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        Self {
            pool: (0..n)
                .map(|i| Backend::new(format!("b{i}"), addr, 0.01 + i as f64 * 0.000_5))
                .collect(),
        }
    }

    fn run(&self) {
        std::hint::black_box(select(&self.pool));
    }
}

struct BackendCostBench {
    backend: Arc<Backend>,
}

impl BackendCostBench {
    fn new() -> Self {
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let backend = Backend::new("b0", addr, 0.05);
        backend.incr_inflight();
        Self { backend }
    }

    fn run(&self) {
        std::hint::black_box(self.backend.cost());
    }
}

struct RouteBench {
    router: Router,
    head: &'static [u8],
}

impl RouteBench {
    fn short() -> Self {
        Self {
            router: sample_router(),
            head: SHORT_HEAD,
        }
    }

    fn disaggregated() -> Self {
        Self {
            router: sample_router(),
            head: CLASSIFY_HEAD,
        }
    }

    fn run(&self) {
        let _ = std::hint::black_box(route(&self.router, self.head));
    }
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

struct WarmLookupBench {
    map: WarmthMap,
    blocks: Vec<u64>,
}

impl WarmLookupBench {
    fn new() -> Self {
        let mut map = WarmthMap::with_capacity(256);
        for i in 0..16 {
            map.insert(i * 256);
        }
        Self {
            map,
            blocks: default_routing_blocks(2048),
        }
    }

    fn run(&self) {
        std::hint::black_box(self.map.hit_strength(&self.blocks));
    }
}

struct PairGreedyBench {
    pf: Vec<Arc<ScoredBackend>>,
    dc: Vec<Arc<ScoredBackend>>,
}

impl PairGreedyBench {
    fn new() -> Self {
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        Self {
            pf: (0..8)
                .map(|i| ScoredBackend::new(format!("pf{i}"), addr, 0.01 + i as f64 * 0.001))
                .collect(),
            dc: (0..8)
                .map(|i| ScoredBackend::new(format!("dc{i}"), addr, 0.02 + i as f64 * 0.001))
                .collect(),
        }
    }

    fn run(&self) {
        std::hint::black_box(greedy_pair(&self.pf, &self.dc, None, 2048, 1.05));
    }
}

struct RebalanceBench {
    rebalancer: PoolRebalancer,
    signals: PoolPressure,
}

struct RcuSnapshotBench {
    table: Arc<RcuRoutingTable>,
}

impl RcuSnapshotBench {
    fn new() -> Self {
        Self {
            table: RcuRoutingTable::new(0.5),
        }
    }

    fn run(&self) {
        std::hint::black_box(self.table.read_pi());
    }
}

impl RebalanceBench {
    fn new() -> Self {
        Self {
            rebalancer: PoolRebalancer::new(RebalancerMode::Shadow),
            signals: PoolPressure {
                q_prefill: 0.8,
                q_decode: 0.2,
                kv_decode: 0.3,
                ..Default::default()
            },
        }
    }

    fn run(&mut self) {
        std::hint::black_box(self.rebalancer.shadow_pi_star(&self.signals));
        let _ = self.rebalancer.maybe_update(&self.signals);
    }
}

fn bench_kv_reserve() {
    let b = std::hint::black_box(kv_breakdown(
        std::hint::black_box(2048_u64),
        std::hint::black_box(128_u64),
    ));
    let phi = phi_barrier_marginal(
        b.kv_reserved,
        std::hint::black_box(b.kv_reserved / 2),
        std::hint::black_box(b.kv_reserved.saturating_mul(10)),
    );
    std::hint::black_box(phi);
}

fn gate_limit(gate: &Gate, settings: &Settings) -> u64 {
    let slack = if std::env::var("CI").is_ok() {
        settings.ci_slack
    } else {
        1.0
    };
    (gate.max_median_ns as f64 * slack).ceil() as u64
}

fn run_gate(gate: &Gate, settings: &Settings) -> Result<(SampleStats, u64), Box<dyn Error>> {
    let compose = ComposeBench::new();
    let select64 = SelectBench::with_size(64);
    let backend_cost = BackendCostBench::new();
    let route_short = RouteBench::short();
    let route_long = RouteBench::disaggregated();
    let warm_lookup = WarmLookupBench::new();
    let pair_greedy = PairGreedyBench::new();
    let mut rebalance = RebalanceBench::new();
    let rcu_snapshot = RcuSnapshotBench::new();

    let stats = match gate.id.as_str() {
        "BENCH-COMPOSE-8" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            || compose.run(),
        ),
        "BENCH-SELECT-64" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            || select64.run(),
        ),
        "BENCH-BACKEND-COST" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            || backend_cost.run(),
        ),
        "BENCH-CLASSIFY" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            || route_short.run(),
        ),
        "BENCH-ROUTE-DISPATCH" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            || route_long.run(),
        ),
        "BENCH-KV-RESERVE" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            bench_kv_reserve,
        ),
        "BENCH-WARM-LOOKUP" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            || warm_lookup.run(),
        ),
        "BENCH-PAIR-GREEDY" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            || pair_greedy.run(),
        ),
        "BENCH-REBALANCE" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            || rebalance.run(),
        ),
        "BENCH-RCU-SNAPSHOT" => sample_ns_per_op(
            gate.warmup_iters,
            gate.bench_iters,
            settings.samples,
            || rcu_snapshot.run(),
        ),
        other => return Err(format!("unknown bench gate id {other:?}").into()),
    };

    Ok((stats, gate_limit(gate, settings)))
}

fn headroom_pct(median_ns: u64, limit_ns: u64) -> f64 {
    if median_ns == 0 {
        return 100.0;
    }
    (limit_ns as f64 / median_ns as f64 - 1.0) * 100.0
}

fn is_thin(median_ns: u64, limit_ns: u64) -> bool {
    median_ns * 3 > limit_ns || median_ns * 100 > limit_ns * 35
}

/// Nominal TOML limit from p95: 1.5× p95 (CI applies `ci_slack` on top at check time).
fn suggest_max_median_ns(p95_ns: u64) -> u64 {
    (p95_ns as f64 * 1.5).ceil() as u64
}

pub fn bench_gate() -> Result<(), Box<dyn Error>> {
    let file: BenchGatesFile = toml::from_str(&fs::read_to_string(BENCH_GATES)?)?;
    if file.gate.is_empty() {
        return Err("no gates declared in bench-gates.toml".into());
    }

    let mut failures = 0usize;
    for gate in &file.gate {
        let (stats, limit) = run_gate(gate, &file.settings)?;
        if stats.median_ns <= limit {
            println!(
                "bench-gate: {id} OK — median {median} ns/op (floor {floor}, p95 {p95}, limit {limit} ns)",
                id = gate.id,
                median = stats.median_ns,
                floor = stats.floor_ns,
                p95 = stats.p95_ns,
            );
        } else {
            eprintln!(
                "bench-gate: {id} FAIL — median {median} ns/op exceeds limit {limit} ns (floor {floor}, p95 {p95})",
                id = gate.id,
                median = stats.median_ns,
                floor = stats.floor_ns,
                p95 = stats.p95_ns,
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

/// Extended sampling: floor, median, p95, headroom, thin-gate flags, suggested limits.
pub fn bench_probe() -> Result<(), Box<dyn Error>> {
    let file: BenchGatesFile = toml::from_str(&fs::read_to_string(BENCH_GATES)?)?;
    let probe_samples = std::env::var("BENCH_PROBE_SAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let probe_settings = Settings {
        samples: probe_samples,
        ci_slack: file.settings.ci_slack,
    };

    println!(
        "bench-probe: {} gate(s), {probe_samples} samples each (CI slack {:.1}× on limits)\n",
        file.gate.len(),
        file.settings.ci_slack
    );
    println!(
        "{:<22} {:>7} {:>7} {:>7} {:>7} {:>8} {:>6}  suggest",
        "gate", "floor", "median", "p95", "limit", "headroom", "thin"
    );
    println!("{}", "-".repeat(86));

    let mut thin_gates = Vec::new();
    for gate in &file.gate {
        let (stats, limit) = run_gate(gate, &probe_settings)?;
        let headroom = headroom_pct(stats.median_ns, limit);
        let thin = is_thin(stats.median_ns, limit);
        if thin {
            thin_gates.push(gate.id.clone());
        }
        println!(
            "{:<22} {:>6}ns {:>6}ns {:>6}ns {:>6}ns {:>7.0}% {:>6}  max_median_ns ≈ {}",
            gate.id,
            stats.floor_ns,
            stats.median_ns,
            stats.p95_ns,
            limit,
            headroom,
            if thin { "YES" } else { "·" },
            suggest_max_median_ns(stats.p95_ns),
        );
    }

    println!();
    if thin_gates.is_empty() {
        println!("bench-probe: no thin gates — all medians sit comfortably below limits.");
    } else {
        println!(
            "bench-probe: THIN gates (tight headroom): {}",
            thin_gates.join(", ")
        );
        println!("  → optimize these first; they dominate accept-path latency.");
    }

    let compose = ComposeBench::new();
    let backend_cost = BackendCostBench::new();
    let select2 = SelectBench::with_size(2);
    let select64 = SelectBench::with_size(64);
    let route_short = RouteBench::short();
    let route_long = RouteBench::disaggregated();

    println!("\nbench-probe: hot-path stack (ascending — thin walls at the top):");
    let mut stack: Vec<(&str, &str, u64)> = vec![
        (
            "parse_tokens",
            "X-Demiurge-Tokens header scan",
            sample_ns_per_op(5_000, 50_000, probe_samples, || {
                std::hint::black_box(parse_prompt_tokens(CLASSIFY_HEAD));
            })
            .median_ns,
        ),
        (
            "estimate_tokens",
            "header + path + default fallback",
            sample_ns_per_op(5_000, 50_000, probe_samples, || {
                std::hint::black_box(estimate_prompt_tokens(CLASSIFY_HEAD));
            })
            .median_ns,
        ),
        (
            "backend_cost",
            "single target ln(C) + inflight barrier",
            sample_ns_per_op(10_000, 100_000, probe_samples, || backend_cost.run()).median_ns,
        ),
        (
            "compose_8",
            "4 barriers + 2 discounts + identity corrector",
            sample_ns_per_op(10_000, 100_000, probe_samples, || compose.run()).median_ns,
        ),
        (
            "select_2",
            "min-cost over 2 backends",
            sample_ns_per_op(1_000, 10_000, probe_samples, || select2.run()).median_ns,
        ),
        (
            "select_64",
            "min-cost over 64 backends (pool scan)",
            sample_ns_per_op(1_000, 10_000, probe_samples, || select64.run()).median_ns,
        ),
        (
            "route_short",
            "colocated fast path",
            sample_ns_per_op(5_000, 50_000, probe_samples, || route_short.run()).median_ns,
        ),
        (
            "route_long",
            "disaggregated admission + RequestId",
            sample_ns_per_op(5_000, 50_000, probe_samples, || route_long.run()).median_ns,
        ),
    ];
    stack.sort_by_key(|(_, _, ns)| *ns);
    for (id, note, ns) in &stack {
        println!("  {ns:>6} ns/op  {id} — {note}");
    }

    if let Some((_, _, select64_ns)) = stack.iter().find(|(id, _, _)| *id == "select_64") {
        if let Some((_, _, cost_ns)) = stack.iter().find(|(id, _, _)| *id == "backend_cost") {
            let implied = cost_ns * 64;
            println!(
                "\nbench-probe: select_64 ≈ {select64_ns} ns vs 64× backend_cost ≈ {implied} ns — scaling is {}",
                if *select64_ns > implied * 2 {
                    "super-linear (comparisons/branching)"
                } else {
                    "linear in pool size"
                }
            );
        }
    }

    Ok(())
}
