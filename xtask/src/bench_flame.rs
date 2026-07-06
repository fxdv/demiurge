//! `bench-flame` — flame-style SVG of the CPU-gate call hierarchy with
//! headroom heat, limit bars, and median trends across runs.
//!
//! `parent` links in `design/bench-gates.toml` define the call structure
//! (nesting = who invokes whom). Each gate is measured independently, so box
//! widths are log₂-scaled per-op medians — **not** additive time attribution;
//! every box carries its own numbers. Runs append to
//! `target/bench-probe/history.jsonl` (sparklines + Δ vs previous run) and
//! render `target/bench-probe/flame.svg`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::bench_gate::{measure_all, GateMeasurement};
use crate::load_bench::{hostname, rfc3339_now};

const OUT_DIR: &str = "target/bench-probe";
const SVG_PATH: &str = "target/bench-probe/flame.svg";
const HISTORY_PATH: &str = "target/bench-probe/history.jsonl";
/// Sparkline window (runs), including the current one.
const SPARK_POINTS: usize = 12;

// Layout (px).
const MARGIN: f64 = 26.0;
const HEADER_H: f64 = 64.0;
const LEGEND_H: f64 = 66.0;
const ROW_H: f64 = 68.0;
const BOX_H: f64 = 58.0;
const SIB_GAP: f64 = 10.0;
const ROOT_GUTTER: f64 = 30.0;
/// Leaf width = OWN_BASE + log₂(median ns) · OWN_K.
const OWN_BASE: f64 = 128.0;
const OWN_K: f64 = 22.0;

#[derive(Debug, Serialize, Deserialize)]
struct HistoryEntry {
    ts: String,
    host: String,
    gates: BTreeMap<String, HistoryPoint>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct HistoryPoint {
    median_ns: u64,
    p95_ns: u64,
    limit_ns: u64,
}

struct Node {
    m: GateMeasurement,
    children: Vec<usize>,
    depth: usize,
    subtree_w: f64,
    x: f64,
    /// Medians from history (oldest → newest), current run last.
    spark: Vec<u64>,
    /// Median delta vs previous run, if any history exists.
    delta_ns: Option<i64>,
}

pub fn bench_flame() -> Result<(), Box<dyn Error>> {
    let samples = std::env::var("BENCH_FLAME_SAMPLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let measurements = measure_all(samples)?;

    let history = read_history();
    let mut nodes = build_forest(measurements, &history)?;
    let roots: Vec<usize> = (0..nodes.len())
        .filter(|&i| nodes[i].m.parent.is_none())
        .collect();

    for &r in &roots {
        compute_subtree_w(&mut nodes, r);
    }
    let mut x = MARGIN;
    for &r in &roots {
        assign_x(&mut nodes, r, x);
        x += nodes[r].subtree_w + ROOT_GUTTER;
    }
    let canvas_w = x - ROOT_GUTTER + MARGIN;
    let rows = nodes.iter().map(|n| n.depth).max().unwrap_or(0) + 1;
    let canvas_h = HEADER_H + rows as f64 * ROW_H + LEGEND_H;

    let ts = rfc3339_now();
    let host = hostname();
    let svg = render_svg(&nodes, rows, canvas_w, canvas_h, &ts, &host, samples);

    fs::create_dir_all(OUT_DIR)?;
    fs::write(SVG_PATH, svg)?;
    append_history(&nodes, &ts, &host)?;

    let thin: Vec<&str> = nodes
        .iter()
        .filter(|n| n.m.thin)
        .map(|n| n.m.id.as_str())
        .collect();
    println!(
        "bench-flame: {} gate(s), {samples} samples — thin: {}",
        nodes.len(),
        if thin.is_empty() {
            "none".to_string()
        } else {
            thin.join(", ")
        }
    );
    let mut movers: Vec<String> = nodes
        .iter()
        .filter_map(|n| {
            let d = n.delta_ns?;
            (d.abs() >= 2).then(|| {
                format!(
                    "{} {}{}ns",
                    short_id(&n.m.id),
                    if d > 0 { "+" } else { "−" },
                    d.abs()
                )
            })
        })
        .collect();
    if history.is_empty() {
        println!("bench-flame: no run history yet — trends start with this run");
    } else if movers.is_empty() {
        println!(
            "bench-flame: Δ vs last run — all gates within ±1ns (history n={})",
            history.len()
        );
    } else {
        movers.sort();
        println!(
            "bench-flame: Δ vs last run — {} (history n={})",
            movers.join(" · "),
            history.len()
        );
    }
    println!("bench-flame: wrote {SVG_PATH} (history → {HISTORY_PATH})");
    Ok(())
}

fn read_history() -> Vec<HistoryEntry> {
    let Ok(text) = fs::read_to_string(HISTORY_PATH) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn append_history(nodes: &[Node], ts: &str, host: &str) -> Result<(), Box<dyn Error>> {
    let entry = HistoryEntry {
        ts: ts.to_string(),
        host: host.to_string(),
        gates: nodes
            .iter()
            .map(|n| {
                (
                    n.m.id.clone(),
                    HistoryPoint {
                        median_ns: n.m.median_ns,
                        p95_ns: n.m.p95_ns,
                        limit_ns: n.m.limit_ns,
                    },
                )
            })
            .collect(),
    };
    let mut line = serde_json::to_string(&entry)?;
    line.push('\n');
    let existing = fs::read_to_string(HISTORY_PATH).unwrap_or_default();
    fs::write(Path::new(HISTORY_PATH), existing + &line)?;
    Ok(())
}

fn build_forest(
    measurements: Vec<GateMeasurement>,
    history: &[HistoryEntry],
) -> Result<Vec<Node>, Box<dyn Error>> {
    let idx_of: BTreeMap<String, usize> = measurements
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.clone(), i))
        .collect();

    // Validate parents and depth (walk-up also rejects cycles).
    let mut depths = vec![0usize; measurements.len()];
    for (i, m) in measurements.iter().enumerate() {
        let mut depth = 0usize;
        let mut cur = m;
        while let Some(ref pid) = cur.parent {
            let &pi = idx_of.get(pid).ok_or_else(|| {
                format!("bench-flame: gate `{}` has unknown parent `{pid}`", m.id)
            })?;
            depth += 1;
            if depth > measurements.len() {
                return Err(format!("bench-flame: parent cycle involving `{}`", m.id).into());
            }
            cur = &measurements[pi];
        }
        depths[i] = depth;
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); measurements.len()];
    for (i, m) in measurements.iter().enumerate() {
        if let Some(ref pid) = m.parent {
            children[idx_of[pid]].push(i);
        }
    }

    Ok(measurements
        .into_iter()
        .enumerate()
        .map(|(i, m)| {
            let mut spark: Vec<u64> = history
                .iter()
                .filter_map(|e| e.gates.get(&m.id).map(|p| p.median_ns))
                .collect();
            let delta_ns = spark.last().map(|&prev| m.median_ns as i64 - prev as i64);
            spark.push(m.median_ns);
            let start = spark.len().saturating_sub(SPARK_POINTS);
            Node {
                spark: spark[start..].to_vec(),
                delta_ns,
                m,
                children: std::mem::take(&mut children[i]),
                depth: depths[i],
                subtree_w: 0.0,
                x: 0.0,
            }
        })
        .collect())
}

fn own_w(median_ns: u64) -> f64 {
    OWN_BASE + (median_ns.max(1) as f64).log2() * OWN_K
}

fn compute_subtree_w(nodes: &mut Vec<Node>, i: usize) -> f64 {
    let kids = nodes[i].children.clone();
    let mut kids_w = 0.0;
    for (n, &k) in kids.iter().enumerate() {
        if n > 0 {
            kids_w += SIB_GAP;
        }
        kids_w += compute_subtree_w(nodes, k);
    }
    let w = own_w(nodes[i].m.median_ns).max(kids_w);
    nodes[i].subtree_w = w;
    w
}

fn assign_x(nodes: &mut Vec<Node>, i: usize, x: f64) {
    nodes[i].x = x;
    let kids = nodes[i].children.clone();
    let kids_w: f64 = kids.iter().map(|&k| nodes[k].subtree_w).sum::<f64>()
        + SIB_GAP * kids.len().saturating_sub(1) as f64;
    let mut cx = x + (nodes[i].subtree_w - kids_w) / 2.0;
    for &k in &kids {
        assign_x(nodes, k, cx);
        cx += nodes[k].subtree_w + SIB_GAP;
    }
}

fn short_id(id: &str) -> &str {
    id.strip_prefix("BENCH-").unwrap_or(id)
}

/// Heat by headroom: thin gates burn red; otherwise hue cools with log headroom
/// (amber ≈100% → cyan ≥ ~1500%).
fn heat_fill(m: &GateMeasurement) -> String {
    if m.thin {
        return "hsl(357 72% 52%)".into();
    }
    let t = ((m.headroom_pct.max(60.0).log10() - 2.0) / 1.18).clamp(0.0, 1.0);
    let hue = 24.0 + t * 176.0;
    format!("hsl({hue:.0} 58% 42%)")
}

fn heat_stroke(m: &GateMeasurement) -> String {
    if m.thin {
        return "hsl(357 80% 68%)".into();
    }
    let t = ((m.headroom_pct.max(60.0).log10() - 2.0) / 1.18).clamp(0.0, 1.0);
    let hue = 24.0 + t * 176.0;
    format!("hsl({hue:.0} 62% 60%)")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn sparkline(spark: &[u64], x: f64, y: f64, w: f64, h: f64) -> String {
    if spark.is_empty() {
        return String::new();
    }
    let lo = *spark.iter().min().unwrap() as f64;
    let hi = *spark.iter().max().unwrap() as f64;
    let span = (hi - lo).max(1.0);
    let n = spark.len();
    let pt = |i: usize, v: u64| {
        let px = if n == 1 {
            x + w
        } else {
            x + w * i as f64 / (n - 1) as f64
        };
        let py = y + h - (v as f64 - lo) / span * h;
        (px, py)
    };
    let points: Vec<String> = spark
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let (px, py) = pt(i, v);
            format!("{px:.1},{py:.1}")
        })
        .collect();
    let (lx, ly) = pt(n - 1, spark[n - 1]);
    let mut out = String::new();
    if n > 1 {
        let _ = write!(
            out,
            r##"<polyline points="{}" fill="none" stroke="#ffffff" stroke-opacity="0.5" stroke-width="1"/>"##,
            points.join(" ")
        );
    }
    let _ = write!(
        out,
        r##"<circle cx="{lx:.1}" cy="{ly:.1}" r="1.8" fill="#ffffff" fill-opacity="0.85"/>"##
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn render_svg(
    nodes: &[Node],
    rows: usize,
    w: f64,
    h: f64,
    ts: &str,
    host: &str,
    samples: u32,
) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}" font-family="SF Mono, JetBrains Mono, Menlo, monospace">"#
    );
    let _ = writeln!(
        s,
        r##"<defs><linearGradient id="gloss" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#ffffff" stop-opacity="0.10"/><stop offset="0.5" stop-color="#ffffff" stop-opacity="0.02"/><stop offset="1" stop-color="#000000" stop-opacity="0.12"/></linearGradient></defs>"##
    );
    let _ = writeln!(s, r##"<rect width="100%" height="100%" fill="#0e1116"/>"##);

    // Header.
    let _ = writeln!(
        s,
        r##"<text x="{MARGIN}" y="30" fill="#e6edf3" font-size="15" font-weight="bold" letter-spacing="3">DEMIURGE · CPU GATE FLAME</text>"##
    );
    let _ = writeln!(
        s,
        r##"<text x="{MARGIN}" y="48" fill="#9aa4b2" font-size="11">{} · host {} · {samples} samples · medians ns/op</text>"##,
        xml_escape(ts),
        xml_escape(host),
    );
    let _ = writeln!(
        s,
        r##"<line x1="{MARGIN}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="#1c2230" stroke-width="1"/>"##,
        y = HEADER_H - 6.0,
        x2 = w - MARGIN,
    );

    // Faint row separators (flame grows upward; roots at the bottom row).
    for r in 1..rows {
        let y = HEADER_H + r as f64 * ROW_H - (ROW_H - BOX_H) / 2.0;
        let _ = writeln!(
            s,
            r##"<line x1="{MARGIN}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="#161b26" stroke-width="1" stroke-dasharray="1 5"/>"##,
            x2 = w - MARGIN,
        );
    }

    for n in nodes {
        let bx = n.x;
        let bw = n.subtree_w;
        let by = HEADER_H + (rows - 1 - n.depth) as f64 * ROW_H;
        let fill = heat_fill(&n.m);
        let stroke = heat_stroke(&n.m);

        let delta_txt = match n.delta_ns {
            None => "Δ·".to_string(),
            Some(0) => "Δ0".to_string(),
            Some(d) if d > 0 => format!("Δ+{d}ns"),
            Some(d) => format!("Δ−{}ns", d.abs()),
        };
        let delta_color = match n.delta_ns {
            Some(d) if d > 0 => "#ff9d9d",
            Some(d) if d < 0 => "#7bd88f",
            _ => "#9aa4b2",
        };

        let _ = writeln!(s, "<g>");
        let _ = writeln!(
            s,
            r##"<title>{id} — {summary}
floor {floor}ns · median {median}ns · p95 {p95}ns · limit {limit}ns
headroom {headroom:.0}%{thin} · {spark_n} run(s) in sparkline</title>"##,
            id = xml_escape(&n.m.id),
            summary = xml_escape(&n.m.summary),
            floor = n.m.floor_ns,
            median = n.m.median_ns,
            p95 = n.m.p95_ns,
            limit = n.m.limit_ns,
            headroom = n.m.headroom_pct,
            thin = if n.m.thin { " · THIN" } else { "" },
            spark_n = n.spark.len(),
        );
        let _ = writeln!(
            s,
            r##"<rect x="{bx:.1}" y="{by:.1}" width="{bw:.1}" height="{BOX_H}" rx="7" fill="{fill}" stroke="{stroke}" stroke-width="1"/>"##
        );
        let _ = writeln!(
            s,
            r##"<rect x="{bx:.1}" y="{by:.1}" width="{bw:.1}" height="{BOX_H}" rx="7" fill="url(#gloss)"/>"##
        );
        let tx = bx + 9.0;
        let _ = writeln!(
            s,
            r##"<text x="{tx:.1}" y="{y:.1}" fill="#f4f8fc" font-size="12" font-weight="bold">{}</text>"##,
            xml_escape(short_id(&n.m.id)),
            y = by + 17.0,
        );
        let _ = writeln!(
            s,
            r##"<text x="{tx:.1}" y="{y:.1}" fill="#eef3f8" fill-opacity="0.92" font-size="10.5">{median}ns · {headroom:.0}% <tspan fill="{delta_color}">{delta_txt}</tspan></text>"##,
            y = by + 31.0,
            median = n.m.median_ns,
            headroom = n.m.headroom_pct,
        );
        // Sparkline strip.
        s.push_str(&sparkline(
            &n.spark,
            tx,
            by + 36.0,
            (bw - 18.0).max(20.0),
            11.0,
        ));
        // Limit bar: filled fraction = median / limit.
        let bar_y = by + BOX_H - 6.0;
        let bar_w = bw - 18.0;
        let used = (n.m.median_ns as f64 / n.m.limit_ns.max(1) as f64).min(1.0);
        let _ = writeln!(
            s,
            r##"<rect x="{tx:.1}" y="{bar_y:.1}" width="{bar_w:.1}" height="3" rx="1.5" fill="#000000" fill-opacity="0.35"/>"##
        );
        let _ = writeln!(
            s,
            r##"<rect x="{tx:.1}" y="{bar_y:.1}" width="{uw:.1}" height="3" rx="1.5" fill="#ffffff" fill-opacity="0.85"/>"##,
            uw = (bar_w * used).max(1.5),
        );
        let _ = writeln!(s, "</g>");
    }

    // Legend.
    let ly = h - LEGEND_H + 14.0;
    let _ = writeln!(
        s,
        r##"<line x1="{MARGIN}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="#1c2230" stroke-width="1"/>"##,
        y = ly - 8.0,
        x2 = w - MARGIN,
    );
    let swatches = [
        ("hsl(357 72% 52%)", "thin — headroom under ~185%"),
        ("hsl(50 58% 42%)", "warm"),
        (
            "hsl(200 58% 42%)",
            "comfortable — hue cools with log headroom",
        ),
    ];
    let mut sx = MARGIN;
    for (color, label) in swatches {
        let _ = writeln!(
            s,
            r##"<rect x="{sx:.1}" y="{y:.1}" width="12" height="12" rx="3" fill="{color}"/>"##,
            y = ly,
        );
        let _ = writeln!(
            s,
            r##"<text x="{tx:.1}" y="{ty:.1}" fill="#9aa4b2" font-size="10.5">{}</text>"##,
            xml_escape(label),
            tx = sx + 17.0,
            ty = ly + 10.0,
        );
        sx += 17.0 + label.chars().count() as f64 * 6.4 + 22.0;
    }
    let _ = writeln!(
        s,
        r##"<text x="{MARGIN}" y="{y:.1}" fill="#68717d" font-size="10.5">width ∝ log₂(own median ns) · nesting = call structure (independent per-op medians, not time attribution) · bar = median/limit · sparkline = median across last {SPARK_POINTS} runs · Δ vs previous run</text>"##,
        y = ly + 30.0,
    );
    let _ = writeln!(s, "</svg>");
    s
}
