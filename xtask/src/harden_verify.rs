//! Run Tiers 1–4 hardening checks and emit an aggregate observable report.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::harden_report::{parse_line, render, render_markdown, HardenEntry};
use crate::load_bench::{LoadBenchReport, ScenarioResult};

const REPORT_DIR: &str = "target/harden-verify";

fn scenario_passes(s: &ScenarioResult) -> bool {
    if s.id.contains("KV-EXHAUST") {
        let rejects = s.kv_admit_rejects.unwrap_or(0);
        return rejects >= 100 && s.errors <= s.total_requests;
    }
    if s.id.contains("ADMIT-FLOOD") {
        // Intentional userspace admit shed (503); load-bench gates min_errors + max cap.
        return s.errors >= 50 && s.errors <= s.total_requests && s.ok > 0;
    }
    if s.id.contains("RDMA-TOPO") {
        let samples = s.rdma_shadow_samples.unwrap_or(0);
        let ratio = s.rdma_transfer_ratio_median.unwrap_or(0.0);
        return s.errors == 0 && samples >= 50 && (0.95..=1.05).contains(&ratio);
    }
    s.errors == 0 || s.kv_admit_rejects.unwrap_or(0) >= s.errors
}

fn scenario_status(s: &ScenarioResult) -> String {
    if scenario_passes(s) {
        "PASS".into()
    } else {
        "FAIL".into()
    }
}

pub fn harden_verify(skip_load: bool, skip_tests: bool) -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(REPORT_DIR);
    fs::create_dir_all(&dir)?;

    let mut entries = Vec::new();
    let mut suite_failures = 0usize;

    let mut run_suite = |pkg: &str, filter: &str| -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("harden-verify: cargo test {filter} -p {pkg} …");
        let output = Command::new("cargo")
            .args(["test", filter, "-p", pkg, "--", "--nocapture"])
            .output()?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for line in combined.lines() {
            if let Some(entry) = parse_line(line) {
                entries.push(entry);
            }
        }
        if !output.status.success() {
            suite_failures += 1;
            eprintln!("harden-verify: FAIL — {pkg}::{filter}");
        } else {
            eprintln!("harden-verify: PASS — {pkg}::{filter}");
        }
        Ok(())
    };

    if !skip_tests {
        run_suite("demiurge-router", "harden_")?;
        run_suite("demiurge-dataplane", "admit_bucket_invariants")?;
        run_suite("demiurge-control", "reservation_ledger_invariants")?;
        #[cfg(target_os = "linux")]
        run_suite("demiurge-router", "harden_io_uring")?;
    }

    let mut load_report: Option<LoadBenchReport> = None;
    let harden_json = PathBuf::from("target/load-bench/harden.json");
    if harden_json.exists() {
        load_report = Some(serde_json::from_str(&fs::read_to_string(&harden_json)?)?);
    }

    if !skip_load {
        eprintln!("harden-verify: load-bench --harden …");
        if let Err(e) = crate::load_bench::load_bench(false, None, false, true, false) {
            suite_failures += 1;
            eprintln!("harden-verify: load-bench --harden failed: {e}");
        }
        if harden_json.exists() {
            load_report = Some(serde_json::from_str(&fs::read_to_string(&harden_json)?)?);
        }
    }

    let mut seen: std::collections::BTreeSet<String> =
        entries.iter().map(|e| e.id.clone()).collect();

    if let Some(ref report) = load_report {
        for s in &report.scenarios {
            if !seen.insert(s.id.clone()) {
                continue;
            }
            let rejects = s.kv_admit_rejects.unwrap_or(0);
            let status = scenario_status(s);
            let detail = if s.id.contains("KV-EXHAUST") || s.id.contains("ADMIT-FLOOD") {
                format!(
                    "graceful_rejects={}/{} kv_rejects={rejects}",
                    s.errors, s.total_requests
                )
            } else if s.id.contains("RDMA-TOPO") {
                format!(
                    "shadow_samples={} ratio={:.3} ok={}/{}",
                    s.rdma_shadow_samples.unwrap_or(0),
                    s.rdma_transfer_ratio_median.unwrap_or(1.0),
                    s.ok,
                    s.total_requests
                )
            } else {
                format!(
                    "ok={}/{} p99={:.2}ms max={}µs",
                    s.ok,
                    s.total_requests,
                    s.p99_us as f64 / 1000.0,
                    s.max_us
                )
            };
            entries.push(HardenEntry {
                tier: 4,
                id: s.id.clone(),
                status,
                detail,
            });
        }
    }

    #[cfg(not(target_os = "linux"))]
    if !seen.contains("LOAD-IOURING-LARGE-BODY") {
        seen.insert("LOAD-IOURING-LARGE-BODY".into());
        entries.push(HardenEntry {
            tier: 4,
            id: "LOAD-IOURING-LARGE-BODY".into(),
            status: "SKIP".into(),
            detail: "Linux only (optional)".into(),
        });
    }

    // Include stress/load aggregates when present (linux-nightly pre-release).
    for path in [
        "target/load-bench/stress.json",
        "target/load-bench/latest.json",
    ] {
        let p = PathBuf::from(path);
        if !p.exists() {
            continue;
        }
        let extra: LoadBenchReport = serde_json::from_str(&fs::read_to_string(&p)?)?;
        for s in extra.scenarios {
            if !seen.insert(s.id.clone()) {
                continue;
            }
            entries.push(HardenEntry {
                tier: 4,
                id: s.id.clone(),
                status: scenario_status(&s),
                detail: format!(
                    "ok={}/{} p99={:.2}ms max={}µs",
                    s.ok,
                    s.total_requests,
                    s.p99_us as f64 / 1000.0,
                    s.max_us
                ),
            });
        }
    }

    let pseudo = render(&entries, load_report.as_ref());
    let md = render_markdown(&entries, load_report.as_ref());
    fs::write(dir.join("report.pseudo"), &pseudo)?;
    fs::write(dir.join("report.md"), &md)?;
    eprintln!("harden-verify: wrote {REPORT_DIR}/report.pseudo");

    let entry_failures = entries
        .iter()
        .filter(|e| e.status != "PASS" && e.status != "SKIP")
        .count();
    if suite_failures > 0 || entry_failures > 0 {
        Err(format!(
            "harden-verify: {suite_failures} suite(s) failed, {entry_failures} case(s) non-PASS"
        )
        .into())
    } else {
        eprintln!(
            "harden-verify: done — {} case(s); see {REPORT_DIR}/report.pseudo",
            entries.len()
        );
        Ok(())
    }
}
