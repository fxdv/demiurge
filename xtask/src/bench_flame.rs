//! `bench-flame` — flame-style SVG of the CPU-gate call hierarchy with
//! headroom heat, limit bars, and median trends across runs.
//!
//! `parent` links in `design/bench-gates.toml` define the call structure
//! (nesting = who invokes whom). Each gate is measured independently, so box
//! widths are log₂-scaled per-op medians — **not** additive time attribution;
//! every box carries its own numbers. Runs append to
//! `target/bench-probe/history.jsonl` (sparklines + Δ vs previous run) and
//! render `target/bench-probe/flame.svg`.
//!
//! Themes: `dark` (default) and `blueprint` — a drafting-sheet look (grid,
//! sheet frame, title block) with categorical verdict colors: red hatch =
//! thin, blue = so-so (headroom < 500%), green = ok.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Blueprint,
}

impl Theme {
    fn parse(name: Option<&str>) -> Result<Self, Box<dyn Error>> {
        match name.unwrap_or("dark") {
            "dark" => Ok(Theme::Dark),
            "blueprint" | "redprint" => Ok(Theme::Blueprint),
            other => Err(format!(
                "bench-flame: unknown theme {other:?}; expected `dark` or `blueprint`"
            )
            .into()),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Blueprint => "blueprint",
        }
    }
}

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

pub fn bench_flame(theme_name: Option<&str>) -> Result<(), Box<dyn Error>> {
    let theme = Theme::parse(theme_name)?;
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
    let svg = render_svg(theme, &nodes, rows, canvas_w, canvas_h, &ts, &host, samples);

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
    println!(
        "bench-flame: wrote {SVG_PATH} [{}] (history → {HISTORY_PATH})",
        theme.label()
    );
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

/// Dark theme heat: thin gates burn red; otherwise hue cools with log
/// headroom (amber ≈100% → cyan ≥ ~1500%).
fn dark_fill(m: &GateMeasurement) -> String {
    if m.thin {
        return "hsl(357 72% 52%)".into();
    }
    let t = ((m.headroom_pct.max(60.0).log10() - 2.0) / 1.18).clamp(0.0, 1.0);
    let hue = 24.0 + t * 176.0;
    format!("hsl({hue:.0} 58% 42%)")
}

fn dark_stroke(m: &GateMeasurement) -> String {
    if m.thin {
        return "hsl(357 80% 68%)".into();
    }
    let t = ((m.headroom_pct.max(60.0).log10() - 2.0) / 1.18).clamp(0.0, 1.0);
    let hue = 24.0 + t * 176.0;
    format!("hsl({hue:.0} 62% 60%)")
}

/// Blueprint verdict class: 0 = thin (red), 1 = so-so (blue), 2 = ok (green).
fn bp_tier(m: &GateMeasurement) -> usize {
    if m.thin {
        0
    } else if m.headroom_pct < 500.0 {
        1
    } else {
        2
    }
}

const BP_STROKE: [&str; 3] = ["#ff5252", "#57a8ff", "#5fd97e"];
const BP_STROKE_W: [f64; 3] = [1.8, 1.2, 1.0];
const BP_FILL: [&str; 3] = ["url(#hatch)", "#57a8ff", "#5fd97e"];
const BP_FILL_OP: [f64; 3] = [1.0, 0.10, 0.07];

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn sparkline(spark: &[u64], x: f64, y: f64, w: f64, h: f64, color: &str, op: f64) -> String {
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
            r##"<polyline points="{}" fill="none" stroke="{color}" stroke-opacity="{op}" stroke-width="1"/>"##,
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
    theme: Theme,
    nodes: &[Node],
    rows: usize,
    w: f64,
    h: f64,
    ts: &str,
    host: &str,
    samples: u32,
) -> String {
    let bp = theme == Theme::Blueprint;
    let bg = if bp { "#0d1420" } else { "#0e1116" };
    let text = if bp { "#dfe7f1" } else { "#e6edf3" };
    let text_dim = if bp { "#94a3b8" } else { "#9aa4b2" };
    let text_faint = if bp { "#66738a" } else { "#68717d" };
    let rule = if bp { "#9db4d0" } else { "#1c2230" };
    let rule_op = if bp { 0.35 } else { 1.0 };
    let row_sep = if bp { "#8fa9c9" } else { "#161b26" };
    let row_sep_op = if bp { 0.25 } else { 1.0 };
    let spark_color = if bp { "#cfe0f2" } else { "#ffffff" };
    let spark_op = if bp { 0.6 } else { 0.5 };

    let mut s = String::new();
    let _ = writeln!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}" font-family="SF Mono, JetBrains Mono, Menlo, monospace">"#
    );
    let _ = writeln!(s, "<defs>");
    let _ = writeln!(
        s,
        r##"<linearGradient id="gloss" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#ffffff" stop-opacity="0.10"/><stop offset="0.5" stop-color="#ffffff" stop-opacity="0.02"/><stop offset="1" stop-color="#000000" stop-opacity="0.12"/></linearGradient>"##
    );
    if bp {
        let _ = writeln!(
            s,
            r##"<pattern id="gmin" width="16" height="16" patternUnits="userSpaceOnUse"><path d="M16 0H0V16" fill="none" stroke="#8fa9c9" stroke-opacity="0.07" stroke-width="0.5"/></pattern>"##
        );
        let _ = writeln!(
            s,
            r##"<pattern id="gmaj" width="80" height="80" patternUnits="userSpaceOnUse"><path d="M80 0H0V80" fill="none" stroke="#8fa9c9" stroke-opacity="0.15" stroke-width="0.7"/></pattern>"##
        );
        let _ = writeln!(
            s,
            r##"<pattern id="hatch" width="6" height="6" patternTransform="rotate(45)" patternUnits="userSpaceOnUse"><line x1="0" y1="0" x2="0" y2="6" stroke="#ff6b6b" stroke-opacity="0.45" stroke-width="1.8"/></pattern>"##
        );
    }
    let _ = writeln!(s, "</defs>");
    let _ = writeln!(s, r##"<rect width="100%" height="100%" fill="{bg}"/>"##);
    if bp {
        // Drafting grid + double sheet frame.
        let _ = writeln!(
            s,
            r##"<rect width="100%" height="100%" fill="url(#gmin)"/>"##
        );
        let _ = writeln!(
            s,
            r##"<rect width="100%" height="100%" fill="url(#gmaj)"/>"##
        );
        let _ = writeln!(
            s,
            r##"<rect x="6.5" y="6.5" width="{fw:.1}" height="{fh:.1}" fill="none" stroke="#9db4d0" stroke-opacity="0.55" stroke-width="1.5"/>"##,
            fw = w - 13.0,
            fh = h - 13.0,
        );
        let _ = writeln!(
            s,
            r##"<rect x="11.5" y="11.5" width="{fw:.1}" height="{fh:.1}" fill="none" stroke="#9db4d0" stroke-opacity="0.28" stroke-width="0.6"/>"##,
            fw = w - 23.0,
            fh = h - 23.0,
        );
    }

    // Header.
    let _ = writeln!(
        s,
        r##"<text x="{MARGIN}" y="30" fill="{text}" font-size="15" font-weight="bold" letter-spacing="3">DEMIURGE · CPU GATE FLAME</text>"##
    );
    let _ = writeln!(
        s,
        r##"<text x="{MARGIN}" y="48" fill="{text_dim}" font-size="11">{} · host {} · {samples} samples · medians ns/op</text>"##,
        xml_escape(ts),
        xml_escape(host),
    );
    let _ = writeln!(
        s,
        r##"<line x1="{MARGIN}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="{rule}" stroke-opacity="{rule_op}" stroke-width="1"/>"##,
        y = HEADER_H - 6.0,
        x2 = w - MARGIN,
    );

    // Faint row separators (flame grows upward; roots at the bottom row).
    for r in 1..rows {
        let y = HEADER_H + r as f64 * ROW_H - (ROW_H - BOX_H) / 2.0;
        let _ = writeln!(
            s,
            r##"<line x1="{MARGIN}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="{row_sep}" stroke-opacity="{row_sep_op}" stroke-width="1" stroke-dasharray="1 5"/>"##,
            x2 = w - MARGIN,
        );
    }

    for n in nodes {
        let bx = n.x;
        let bw = n.subtree_w;
        let by = HEADER_H + (rows - 1 - n.depth) as f64 * ROW_H;

        let (fill, fill_op, stroke, stroke_w) = if bp {
            let tier = bp_tier(&n.m);
            (
                BP_FILL[tier].to_string(),
                BP_FILL_OP[tier],
                BP_STROKE[tier].to_string(),
                BP_STROKE_W[tier],
            )
        } else {
            (dark_fill(&n.m), 1.0, dark_stroke(&n.m), 1.0)
        };

        let delta_txt = match n.delta_ns {
            None => "Δ·".to_string(),
            Some(0) => "Δ0".to_string(),
            Some(d) if d > 0 => format!("Δ+{d}ns"),
            Some(d) => format!("Δ−{}ns", d.abs()),
        };
        let delta_color = match n.delta_ns {
            Some(d) if d > 0 => "#ff9d9d",
            Some(d) if d < 0 => "#7bd88f",
            _ => text_dim,
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
            r##"<rect x="{bx:.1}" y="{by:.1}" width="{bw:.1}" height="{BOX_H}" rx="{rx}" fill="{fill}" fill-opacity="{fill_op}" stroke="{stroke}" stroke-width="{stroke_w}"/>"##,
            rx = if bp { 2 } else { 7 },
        );
        if !bp {
            let _ = writeln!(
                s,
                r##"<rect x="{bx:.1}" y="{by:.1}" width="{bw:.1}" height="{BOX_H}" rx="7" fill="url(#gloss)"/>"##
            );
        }
        let tx = bx + 9.0;
        let label_fill = if bp { "#ffffff" } else { "#f4f8fc" };
        let stats_fill = if bp { "#c9d6e6" } else { "#eef3f8" };
        let _ = writeln!(
            s,
            r##"<text x="{tx:.1}" y="{y:.1}" fill="{label_fill}" font-size="12" font-weight="bold">{}</text>"##,
            xml_escape(short_id(&n.m.id)),
            y = by + 17.0,
        );
        let _ = writeln!(
            s,
            r##"<text x="{tx:.1}" y="{y:.1}" fill="{stats_fill}" fill-opacity="0.92" font-size="10.5">{median}ns · {headroom:.0}% <tspan fill="{delta_color}">{delta_txt}</tspan></text>"##,
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
            spark_color,
            spark_op,
        ));
        // Limit bar: filled fraction = median / limit.
        let bar_y = by + BOX_H - 6.0;
        let bar_w = bw - 18.0;
        let used = (n.m.median_ns as f64 / n.m.limit_ns.max(1) as f64).min(1.0);
        let (track_fill, track_op) = if bp {
            ("#8fa9c9", 0.25)
        } else {
            ("#000000", 0.35)
        };
        let _ = writeln!(
            s,
            r##"<rect x="{tx:.1}" y="{bar_y:.1}" width="{bar_w:.1}" height="3" rx="1.5" fill="{track_fill}" fill-opacity="{track_op}"/>"##
        );
        let _ = writeln!(
            s,
            r##"<rect x="{tx:.1}" y="{bar_y:.1}" width="{uw:.1}" height="3" rx="1.5" fill="#ffffff" fill-opacity="0.9"/>"##,
            uw = (bar_w * used).max(1.5),
        );
        let _ = writeln!(s, "</g>");
    }

    // Legend.
    let ly = h - LEGEND_H + 14.0;
    let _ = writeln!(
        s,
        r##"<line x1="{MARGIN}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="{rule}" stroke-opacity="{rule_op}" stroke-width="1"/>"##,
        y = ly - 8.0,
        x2 = w - MARGIN,
    );
    let swatches: [(String, &str); 3] = if bp {
        [
            (
                r##"fill="url(#hatch)" stroke="#ff5252" stroke-width="1.6""##.into(),
                "thin — headroom under ~185%",
            ),
            (
                r##"fill="#57a8ff" fill-opacity="0.10" stroke="#57a8ff" stroke-width="1.2""##
                    .into(),
                "so-so — headroom under 500%",
            ),
            (
                r##"fill="#5fd97e" fill-opacity="0.07" stroke="#5fd97e" stroke-width="1""##.into(),
                "ok — headroom 500% and up",
            ),
        ]
    } else {
        [
            (
                r##"fill="hsl(357 72% 52%)""##.into(),
                "thin — headroom under ~185%",
            ),
            (r##"fill="hsl(50 58% 42%)""##.into(), "warm"),
            (
                r##"fill="hsl(200 58% 42%)""##.into(),
                "comfortable — hue cools with log headroom",
            ),
        ]
    };
    let mut sx = MARGIN;
    for (attrs, label) in &swatches {
        let _ = writeln!(
            s,
            r##"<rect x="{sx:.1}" y="{y:.1}" width="12" height="12" rx="{rx}" {attrs}/>"##,
            y = ly,
            rx = if bp { 1 } else { 3 },
        );
        let _ = writeln!(
            s,
            r##"<text x="{tx:.1}" y="{ty:.1}" fill="{text_dim}" font-size="10.5">{}</text>"##,
            xml_escape(label),
            tx = sx + 17.0,
            ty = ly + 10.0,
        );
        sx += 17.0 + label.chars().count() as f64 * 6.4 + 22.0;
    }
    let _ = writeln!(
        s,
        r##"<text x="{MARGIN}" y="{y:.1}" fill="{text_faint}" font-size="10.5">width ∝ log₂(own median ns) · nesting = call structure (independent per-op medians, not time attribution) · bar = median/limit · sparkline = median across last {SPARK_POINTS} runs · Δ vs previous run</text>"##,
        y = ly + 30.0,
    );

    if bp {
        // Title block, drafting-sheet style, bottom-right.
        let tb_w = 268.0;
        let tb_h = 46.0;
        let tb_x = w - MARGIN - tb_w;
        let tb_y = h - LEGEND_H + 4.0;
        let _ = writeln!(
            s,
            r##"<rect x="{tb_x:.1}" y="{tb_y:.1}" width="{tb_w}" height="{tb_h}" fill="{bg}" stroke="#9db4d0" stroke-opacity="0.7" stroke-width="1.2"/>"##
        );
        for dy in [15.0, 31.0] {
            let _ = writeln!(
                s,
                r##"<line x1="{tb_x:.1}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="#9db4d0" stroke-opacity="0.4" stroke-width="0.6"/>"##,
                y = tb_y + dy,
                x2 = tb_x + tb_w,
            );
        }
        let _ = writeln!(
            s,
            r##"<text x="{x:.1}" y="{y:.1}" fill="{text}" font-size="9" font-weight="bold" letter-spacing="1.5">DEMIURGE · CPU GATE FLAME</text>"##,
            x = tb_x + 8.0,
            y = tb_y + 11.0,
        );
        let _ = writeln!(
            s,
            r##"<text x="{x:.1}" y="{y:.1}" fill="{text_dim}" font-size="8.5">DATE {} · HOST {}</text>"##,
            xml_escape(ts),
            xml_escape(host),
            x = tb_x + 8.0,
            y = tb_y + 26.5,
        );
        let _ = writeln!(
            s,
            r##"<text x="{x:.1}" y="{y:.1}" fill="{text_dim}" font-size="8.5">SAMPLES {samples} · SCALE log₂ ns · SHEET 1/1</text>"##,
            x = tb_x + 8.0,
            y = tb_y + 42.0,
        );
    }

    let _ = writeln!(s, "</svg>");
    s
}
