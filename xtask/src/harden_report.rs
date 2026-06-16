//! Aggregate pseudo-graphical report for harden-verify runs.

use std::fmt::Write as FmtWrite;

use crate::load_bench::LoadBenchReport;
use crate::pseudo_ui::{h_rule, pad_line, pseudo_width};

#[derive(Debug, Clone)]
pub struct HardenEntry {
    pub tier: u8,
    pub id: String,
    pub status: String,
    pub detail: String,
}

pub fn parse_line(line: &str) -> Option<HardenEntry> {
    let line = line.trim();
    if !line.starts_with("HARDEN_REPORT ") {
        return None;
    }
    let mut tier = 0u8;
    let mut id = String::new();
    let mut status = String::new();
    let mut detail = String::new();
    for part in line.split_whitespace().skip(1) {
        if let Some((k, v)) = part.split_once('=') {
            match k {
                "tier" => tier = v.parse().unwrap_or(0),
                "id" => id = v.to_string(),
                "status" => status = v.to_string(),
                "detail" => detail = v.to_string(),
                _ => {}
            }
        }
    }
    if id.is_empty() {
        return None;
    }
    Some(HardenEntry {
        tier,
        id,
        status,
        detail,
    })
}

pub fn render(entries: &[HardenEntry], load: Option<&LoadBenchReport>) -> String {
    let w = pseudo_width();
    let mut out = String::new();
    let pass = entries.iter().filter(|e| e.status == "PASS").count();
    let skip = entries.iter().filter(|e| e.status == "SKIP").count();
    let fail = entries
        .iter()
        .filter(|e| e.status != "PASS" && e.status != "SKIP")
        .count();

    let _ = writeln!(out, "╔{}╗", h_rule(w, '═'));
    let _ = writeln!(
        out,
        "{}",
        pad_line("DEMIURGE · DIE-HARD VERIFY · PSEUDO REPORT", w)
    );
    let _ = writeln!(out, "╠{}╣", h_rule(w, '═'));
    let _ = writeln!(
        out,
        "{}",
        pad_line(
            &format!(
                "cases: PASS {pass} · SKIP {skip} · FAIL {fail} · total {}",
                entries.len()
            ),
            w,
        )
    );
    let _ = writeln!(out, "╠{}╣", h_rule(w, '═'));

    for tier in 1u8..=4 {
        let tier_entries: Vec<_> = entries.iter().filter(|e| e.tier == tier).collect();
        if tier_entries.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{}", pad_line(&format!("Tier {tier}"), w));
        for e in tier_entries {
            let mark = match e.status.as_str() {
                "PASS" => "✓",
                "SKIP" => "○",
                _ => "✗",
            };
            let _ = writeln!(
                out,
                "{}",
                pad_line(
                    &format!("  {mark} {} — {} ({})", e.id, e.status, e.detail),
                    w
                )
            );
        }
        let _ = writeln!(out, "╟{}╢", h_rule(w, '─'));
    }

    if let Some(report) = load {
        let _ = writeln!(out, "{}", pad_line("Tier 4 load scenarios", w));
        for s in &report.scenarios {
            let p99_ms = s.p99_us as f64 / 1000.0;
            let tail = if s.p99_us > 0 {
                s.max_us as f64 / s.p99_us as f64
            } else {
                0.0
            };
            let _ = writeln!(
                out,
                "{}",
                pad_line(
                    &format!(
                        "  {} ok={}/{} p99={p99_ms:.2}ms max={}µs tail={tail:.1}x",
                        s.id, s.ok, s.total_requests, s.max_us
                    ),
                    w,
                )
            );
            if let Some(rejects) = s.kv_admit_rejects {
                let note = if s.ok == 0 && rejects > 0 {
                    " (graceful shed)"
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "{}",
                    pad_line(&format!("    kv_rejects={rejects}{note}"), w)
                );
            }
            if let Some(m) = s.fastpath_misroute_mean {
                let _ = writeln!(out, "{}", pad_line(&format!("    misroute_mean={m:.3}"), w));
            }
        }
    }

    let _ = writeln!(out, "╚{}╝", h_rule(w, '═'));
    out
}

pub fn render_markdown(entries: &[HardenEntry], load: Option<&LoadBenchReport>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Demiurge die-hard verify\n");
    let _ = writeln!(out, "| Tier | ID | Status | Detail |");
    let _ = writeln!(out, "|------|-----|--------|--------|");
    for e in entries {
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {} |",
            e.tier, e.id, e.status, e.detail
        );
    }
    if let Some(report) = load {
        let _ = writeln!(out, "\n## Load scenarios\n");
        let _ = writeln!(
            out,
            "| Scenario | OK | Total | p99 (ms) | max (µs) | KV rejects |"
        );
        let _ = writeln!(
            out,
            "|----------|-----|-------|----------|----------|------------|"
        );
        for s in &report.scenarios {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {:.2} | {} | {} |",
                s.id,
                s.ok,
                s.total_requests,
                s.p99_us as f64 / 1000.0,
                s.max_us,
                s.kv_admit_rejects.unwrap_or(0)
            );
        }
    }
    out
}
