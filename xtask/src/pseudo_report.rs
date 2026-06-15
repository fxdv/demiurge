//! Pseudo-graphical ASCII report for load-bench results.

use std::collections::HashMap;
use std::fmt::Write as FmtWrite;

use crate::load_bench::{
    evaluate_scenario_gates, GateVerdict, LoadBenchReport, ScenarioGateConfig,
};

const W: usize = 96;

fn pad_line(inner: &str) -> String {
    let max_len = W - 4;
    let content = if inner.chars().count() > max_len {
        let truncated: String = inner.chars().take(max_len - 1).collect();
        format!("{truncated}…")
    } else {
        inner.to_string()
    };
    format!("║ {content:<width$} ║", width = W - 4)
}

fn bar(f: &mut String, label: &str, value: f64, max: f64, width: usize) {
    let frac = if max > 0.0 {
        (value / max).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = ((frac * width as f64).round() as usize).min(width);
    let empty = width - filled;
    let _ = write!(
        f,
        "{label:<16} {}{} {:>6.0}",
        "█".repeat(filled),
        "░".repeat(empty),
        value
    );
}

fn histogram(f: &mut String, latencies_us: &[u64], buckets: usize) {
    if latencies_us.is_empty() {
        let _ = writeln!(f, "  (no samples)");
        return;
    }
    let max = *latencies_us.iter().max().unwrap_or(&1);
    let min = *latencies_us.iter().min().unwrap_or(&0);
    let span = (max - min).max(1);
    let mut counts = vec![0u64; buckets];
    for &us in latencies_us {
        let b = ((us - min) as f64 / span as f64 * buckets as f64) as usize;
        counts[b.min(buckets - 1)] += 1;
    }
    let peak = *counts.iter().max().unwrap_or(&1).max(&1);
    let bar_w = 32usize;
    for (i, &c) in counts.iter().enumerate() {
        let lo = min + span * i as u64 / buckets as u64;
        let hi = min + span * (i + 1) as u64 / buckets as u64;
        let label = format!("{:>5}-{:>5}µs", lo, hi);
        bar(f, &label, c as f64, peak as f64, bar_w);
    }
}

fn fmt_ms(us: u64) -> String {
    format!("{:.2}", us as f64 / 1000.0)
}

fn gate_status(pass: bool) -> &'static str {
    if pass {
        "PASS ✓"
    } else {
        "FAIL ✗"
    }
}

fn write_gates(out: &mut String, gates: &[GateVerdict]) {
    if gates.is_empty() {
        return;
    }
    let passed = gates.iter().filter(|g| g.pass).count();
    let all_pass = passed == gates.len();
    let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
    let _ = writeln!(out, "{}", pad_line("GATES (strict + soft)"));
    for gate in gates {
        let _ = writeln!(
            out,
            "{}",
            pad_line(&format!(
                "  {:<18} → {}  {}",
                gate.name,
                gate_status(gate.pass),
                gate.detail
            ))
        );
    }
    let _ = writeln!(
        out,
        "{}",
        pad_line(&format!(
            "  scenario summary .... → {}  ({passed}/{} gates)",
            gate_status(all_pass),
            gates.len()
        ))
    );
}

pub fn render(
    report: &LoadBenchReport,
    gate_configs: &HashMap<String, ScenarioGateConfig>,
    sim_brand: bool,
) -> String {
    let mut out = String::new();
    let mut suite_gates = 0usize;
    let mut suite_pass = 0usize;
    let mut scenario_fail = 0usize;

    let _ = writeln!(out, "╔{}╗", "═".repeat(W - 2));
    let title = if sim_brand {
        "DEMIURGE · 'sim · FLEET SIMULATION · PSEUDO REPORT"
    } else {
        "DEMIURGE · LOCAL LOAD BENCH · PSEUDO REPORT"
    };
    let _ = writeln!(out, "{}", pad_line(title));
    let _ = writeln!(out, "╠{}╣", "═".repeat(W - 2));
    let _ = writeln!(
        out,
        "{}",
        pad_line(&format!("generated: {}", report.generated_at))
    );
    let _ = writeln!(
        out,
        "{}",
        pad_line(&format!("host:      {}", report.hostname))
    );
    let _ = writeln!(out, "╠{}╣", "═".repeat(W - 2));

    for (idx, s) in report.scenarios.iter().enumerate() {
        if idx > 0 {
            let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
        }
        let _ = writeln!(out, "{}", pad_line(&format!("scenario:  {}", s.id)));
        let _ = writeln!(out, "{}", pad_line(&format!("summary:   {}", s.summary)));
        let _ = writeln!(
            out,
            "{}",
            pad_line(&format!(
                "topology:  {} pf · {} dc · {}×{} reqs · style {} · delay {}µs",
                s.backends,
                s.decode_backends,
                s.concurrency,
                s.requests_per_worker,
                s.request_style,
                s.backend_delay_us
            ))
        );
        let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
        let _ = writeln!(out, "{}", pad_line("THROUGHPUT"));
        let _ = writeln!(
            out,
            "{}",
            pad_line(&format!("  total .............. {:>8}", s.total_requests))
        );
        let _ = writeln!(
            out,
            "{}",
            pad_line(&format!(
                "  ok / errors ........ {:>8} / {}",
                s.ok, s.errors
            ))
        );
        if s.errors_graceful.is_some() || s.errors_hard.is_some() {
            let _ = writeln!(
                out,
                "{}",
                pad_line(&format!(
                    "  503 / hard ......... {:>8} / {}",
                    s.errors_graceful.unwrap_or(0),
                    s.errors_hard.unwrap_or(0),
                ))
            );
        }
        let _ = writeln!(
            out,
            "{}",
            pad_line(&format!("  wall ............... {:>8.2}s", s.duration_secs))
        );
        let _ = writeln!(
            out,
            "{}",
            pad_line(&format!("  req/s .............. {:>8.1}", s.req_per_sec))
        );
        let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
        let _ = writeln!(out, "{}", pad_line("LATENCY"));
        let _ = writeln!(out, "{}", pad_line("  min   p50   p90   p99   max  (ms)"));
        let _ = writeln!(
            out,
            "{}",
            pad_line(&format!(
                "  {:>5} {:>5} {:>5} {:>5} {:>5}",
                fmt_ms(s.min_us),
                fmt_ms(s.p50_us),
                fmt_ms(s.p90_us),
                fmt_ms(s.p99_us),
                fmt_ms(s.max_us),
            ))
        );
        let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
        let _ = writeln!(out, "{}", pad_line("HISTOGRAM (latency µs)"));
        let mut hist = String::new();
        histogram(&mut hist, &s.latencies_us, 8);
        for line in hist.lines() {
            if line.is_empty() {
                continue;
            }
            let _ = writeln!(out, "{}", pad_line(line));
        }
        if let (Some(low), Some(high), Some(ratio)) = (
            s.accept_p99_us_low,
            s.accept_p99_us_high,
            s.accept_p99_ratio,
        ) {
            let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
            let _ = writeln!(out, "{}", pad_line("ACCEPT DECOUPLE (P1)"));
            let _ = writeln!(
                out,
                "{}",
                pad_line(&format!("  p99 low / high (µs) .. {:>6} / {}", low, high))
            );
            let _ = writeln!(
                out,
                "{}",
                pad_line(&format!("  p99 ratio ............ {:>8.2}", ratio))
            );
        }
        if let Some(peak) = s.kv_bytes_reserved_peak {
            let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
            let _ = writeln!(out, "{}", pad_line("KV POOL (Phase 2)"));
            let _ = writeln!(
                out,
                "{}",
                pad_line(&format!("  peak reserved ....... {:>12} bytes", peak))
            );
            if let Some(rejects) = s.kv_admit_rejects {
                let _ = writeln!(
                    out,
                    "{}",
                    pad_line(&format!("  admit rejects ....... {:>12}", rejects))
                );
            }
        }
        if let Some(n) = s.handoff_transfer_count {
            let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
            let _ = writeln!(out, "{}", pad_line("HAND-OFF TRANSFER (Phase 2 exit)"));
            let _ = writeln!(
                out,
                "{}",
                pad_line(&format!("  transfers ............ {:>12}", n))
            );
            let _ = writeln!(
                out,
                "{}",
                pad_line(&format!(
                    "  bytes p50 / p99 ...... {:>6} / {}",
                    s.handoff_bytes_p50.unwrap_or(0),
                    s.handoff_bytes_p99.unwrap_or(0)
                ))
            );
            let _ = writeln!(
                out,
                "{}",
                pad_line(&format!(
                    "  wall p50 / p99 (µs) .. {:>6} / {}",
                    s.handoff_wall_us_p50.unwrap_or(0),
                    s.handoff_wall_us_p99.unwrap_or(0)
                ))
            );
        }
        if let Some(pi) = s.dataplane_pi {
            let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
            let _ = writeln!(out, "{}", pad_line("DATAPLANE"));
            let _ = writeln!(
                out,
                "{}",
                pad_line(&format!(
                    "  π / π* / age ......... {:>5.3} / {:.3} / {}ms",
                    pi,
                    s.pi_star.unwrap_or(0.0),
                    s.dataplane_age_ms.unwrap_or(0)
                ))
            );
        }
        if !s.fleet_windows.is_empty() {
            let _ = writeln!(out, "╟{}╢", "─".repeat(W - 2));
            let _ = writeln!(out, "{}", pad_line("'sim FLEET WINDOWS (live replay)"));
            let _ = writeln!(
                out,
                "{}",
                pad_line("  ts(ms)  heavy  ok  503  hard  p99(ms)  π_live  π*_shadow")
            );
            for w in &s.fleet_windows {
                let _ = writeln!(
                    out,
                    "{}",
                    pad_line(&format!(
                        "  {:>7}  {:>5}  {:>3}  {:>3}  {:>4}  {:>7}  {:>6.3}  {:>6.3}",
                        w.ts_ms,
                        if w.prefill_heavy { "yes" } else { "no" },
                        w.ok,
                        w.errors_graceful,
                        w.errors_hard,
                        w.p99_us as f64 / 1000.0,
                        w.dataplane_pi,
                        w.pi_star,
                    ))
                );
            }
            if let Some(corr) = s.fleet_live_pi_correlation {
                let _ = writeln!(
                    out,
                    "{}",
                    pad_line(&format!("  held-out live π corr .. {:>6.3}", corr))
                );
            }
        }
        if let Some(cfg) = gate_configs.get(&s.id) {
            let gates = evaluate_scenario_gates(cfg, s);
            suite_gates += gates.len();
            suite_pass += gates.iter().filter(|g| g.pass).count();
            if !gates.is_empty() && gates.iter().any(|g| !g.pass) {
                scenario_fail += 1;
            }
            write_gates(&mut out, &gates);
        }
    }

    if suite_gates > 0 {
        let _ = writeln!(out, "╠{}╣", "═".repeat(W - 2));
        let suite_ok = scenario_fail == 0;
        let _ = writeln!(
            out,
            "{}",
            pad_line(&format!(
                "SUITE GATES → {}  {suite_pass}/{suite_gates} gates · {}/{} scenarios",
                gate_status(suite_ok),
                report.scenarios.len() - scenario_fail,
                report.scenarios.len()
            ))
        );
    }

    let _ = writeln!(out, "╚{}╝", "═".repeat(W - 2));
    out
}
