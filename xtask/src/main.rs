//! Demiurge design-conformance tool.
//!
//! `gen` regenerates every artifact derived from the canonical inputs,
//! `lint` enforces the spec/code/test traceability join, and
//! `bench-gate` runs release-mode CPU gates from `design/bench-gates.toml`,
//! `bench-probe` samples floor/median/p95 to tune limits and find thin gates,
//! `load-bench` runs local TCP load scenarios, `load-report` renders the
//! pseudo-graphical report from the last run, `'sim` runs the fleet simulation
//! spinoff, and `harden-verify` runs Tiers 1–4 die-hard checks with an aggregate observable report.
//!
//! ```text
//! design/demiurge.params.toml -> crates/demiurge-cost/src/generated_params.rs
//!                             -> spec/generated/params_table.tex
//! design/requirements.toml    -> spec/generated/conformance_matrix.tex
//! ```
//!
//! Both commands are pure functions of the repository on disk, so CI can run
//! `gen` and fail on any diff (drift), then run `lint` (traceability).

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

use regex::Regex;
use serde::Deserialize;

mod apostrophe_sim;
mod bench_gate;
mod fleet_pilot;
mod harden_report;
mod harden_verify;
mod load_bench;
mod pseudo_report;
#[cfg(target_os = "linux")]
mod track_b_load;

const PARAMS: &str = "design/demiurge.params.toml";
const REQS: &str = "design/requirements.toml";
const RS_OUT: &str = "crates/demiurge-cost/src/generated_params.rs";
const PARAMS_TEX: &str = "spec/generated/params_table.tex";
const MATRIX_TEX: &str = "spec/generated/conformance_matrix.tex";
const SPEC_TEX: &str = "spec/demiurge.tex";

const SCAN_RS_DIRS: &[&str] = &["crates", "xtask"];
const SCAN_TEX_DIRS: &[&str] = &["spec"];

#[derive(Debug, Deserialize)]
struct Requirements {
    #[serde(default)]
    requirement: Vec<Requirement>,
}

#[derive(Debug, Deserialize)]
struct Requirement {
    id: String,
    kind: String,
    #[serde(default = "default_status")]
    status: String,
    /// Development phase this requirement belongs to (see ROADMAP.md). Phase 0
    /// is the shipped foundation; higher phases are the burndown.
    #[serde(default)]
    phase: u32,
    section: String,
    #[allow(dead_code)]
    summary: String,
    #[serde(default)]
    requires_test: bool,
    #[serde(default)]
    tests: Vec<String>,
}

fn default_status() -> String {
    "intended".to_string()
}

fn build_bpf() -> Result<(), Box<dyn Error>> {
    let status = std::process::Command::new("bash")
        .arg("scripts/build-bpf.sh")
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("build-bpf exited with {status}").into())
    }
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let res = match cmd.as_str() {
        "gen" => gen(),
        "lint" => lint(),
        "bench-gate" => bench_gate::bench_gate(),
        "bench-probe" => bench_gate::bench_probe(),
        "load-bench" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let ci_only = args.iter().any(|a| a == "--ci");
            let stress = args.iter().any(|a| a == "--stress");
            let harden = args.iter().any(|a| a == "--harden");
            let sim = args.iter().any(|a| a == "--sim");
            let scenario = args
                .windows(2)
                .find_map(|w| (w[0] == "--scenario").then_some(w[1].as_str()));
            load_bench::load_bench(ci_only, scenario, stress, harden, sim)
        }
        "load-report" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let stress = args.iter().any(|a| a == "--stress");
            let harden = args.iter().any(|a| a == "--harden");
            let sim = args.iter().any(|a| a == "--sim");
            load_bench::load_report(stress, harden, sim)
        }
        "'sim" | "apostrophe-sim" => apostrophe_sim::apostrophe_sim(),
        "harden-verify" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let skip_load = args.iter().any(|a| a == "--skip-load");
            let skip_tests = args.iter().any(|a| a == "--skip-tests");
            harden_verify::harden_verify(skip_load, skip_tests)
        }
        "build-bpf" => build_bpf(),
        "fleet-pilot" => fleet_pilot::fleet_pilot(),
        "spec" => build_spec(),
        "product-doc" => build_product_doc(),
        other => {
            eprintln!(
                "xtask: unknown subcommand {other:?}; expected `gen`, `lint`, `spec`, `product-doc`, `bench-gate`, `bench-probe`, `load-bench`, `load-report`, `harden-verify`, `build-bpf`, `fleet-pilot`, or `'sim`"
            );
            exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("xtask {cmd}: {e}");
        exit(1);
    }
}

fn build_spec() -> Result<(), Box<dyn Error>> {
    gen()?;
    let status = std::process::Command::new("latexmk")
        .args([
            "-pdf",
            "-interaction=nonstopmode",
            "-halt-on-error",
            "demiurge.tex",
        ])
        .current_dir("spec")
        .status()?;
    if status.success() {
        println!("spec: wrote spec/demiurge.pdf");
        Ok(())
    } else {
        Err(format!("latexmk exited with {status}").into())
    }
}

fn build_product_doc() -> Result<(), Box<dyn Error>> {
    let plain = std::env::args().any(|a| a == "--plain");
    let out_dir = Path::new("target/product-doc/docs");
    fs::create_dir_all(out_dir)?;
    let out_md = out_dir.join("PRODUCT-AND-DESIGN.md");
    let out_pdf = out_dir.join("PRODUCT-AND-DESIGN.pdf");

    if plain {
        fs::copy("docs/PRODUCT-AND-DESIGN.md", &out_md)?;
        println!("product-doc: copied docs/PRODUCT-AND-DESIGN.md (no release stamp)");
    } else {
        let status = std::process::Command::new("bash")
            .arg("scripts/generate-product-doc.sh")
            .arg(&out_md)
            .env("ARTIFACT_DIR", "target/product-doc")
            .status()?;
        if !status.success() {
            return Err(format!("generate-product-doc exited with {status}").into());
        }
    }

    let status = std::process::Command::new("bash")
        .args([
            "scripts/compile-product-doc.sh",
            &out_md.to_string_lossy(),
            &out_pdf.to_string_lossy(),
        ])
        .status()?;
    if status.success() {
        println!("product-doc: wrote {}", out_pdf.display());
        Ok(())
    } else {
        Err(format!("compile-product-doc exited with {status}").into())
    }
}

fn tex_escape(s: &str) -> String {
    s.replace('\\', "\\textbackslash{}")
        .replace('_', "\\_")
        .replace('%', "\\%")
        .replace('&', "\\&")
        .replace('#', "\\#")
}

fn write_if_changed(path: &str, contents: &str) -> Result<(), Box<dyn Error>> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    let prev = fs::read_to_string(p).unwrap_or_default();
    if prev != contents {
        fs::write(p, contents)?;
    }
    Ok(())
}

fn gen() -> Result<(), Box<dyn Error>> {
    // ---- parameters -> Rust constants + LaTeX table ----
    let table: toml::Table = toml::from_str(&fs::read_to_string(PARAMS)?)?;

    let mut consts: Vec<(String, String)> = Vec::new(); // (NAME, "ty = lit")
    let mut tex_rows: Vec<(String, String, String)> = Vec::new(); // (group, key, display)
    for (group, gval) in table.iter() {
        let Some(t) = gval.as_table() else { continue };
        for (key, v) in t.iter() {
            let name = format!("{}_{}", group.to_uppercase(), key.to_uppercase());
            let (ty, lit, disp) = match v {
                toml::Value::Integer(i) => ("u64", i.to_string(), i.to_string()),
                toml::Value::Float(f) => {
                    let mut s = format!("{f}");
                    if !s.contains('.') && !s.contains('e') {
                        s.push_str(".0");
                    }
                    ("f64", s.clone(), s)
                }
                toml::Value::Boolean(b) => ("bool", b.to_string(), b.to_string()),
                _ => continue,
            };
            consts.push((name, format!("{ty} = {lit}")));
            tex_rows.push((group.clone(), key.clone(), disp));
        }
    }
    consts.sort();
    tex_rows.sort();

    let mut rs = String::new();
    rs.push_str(
        "// @generated by `cargo xtask gen` from design/demiurge.params.toml. DO NOT EDIT.\n",
    );
    rs.push_str("#![allow(dead_code)]\n\n");
    for (name, tylit) in &consts {
        rs.push_str(&format!("pub const {name}: {tylit};\n"));
    }
    write_if_changed(RS_OUT, &rs)?;

    let mut pt = String::new();
    pt.push_str(
        "% @generated by `cargo xtask gen` from design/demiurge.params.toml. DO NOT EDIT.\n",
    );
    pt.push_str("\\begin{tabular}{lll}\n\\toprule\nGroup & Parameter & Value \\\\\n\\midrule\n");
    for (g, k, d) in &tex_rows {
        pt.push_str(&format!(
            "{} & {} & {} \\\\\n",
            tex_escape(g),
            tex_escape(k),
            tex_escape(d)
        ));
    }
    pt.push_str("\\bottomrule\n\\end{tabular}\n");
    write_if_changed(PARAMS_TEX, &pt)?;

    // ---- requirements -> conformance matrix ----
    let reqs: Requirements = toml::from_str(&fs::read_to_string(REQS)?)?;
    let mut rows = reqs.requirement;
    rows.sort_by(|a, b| a.id.cmp(&b.id));

    let mut cm = String::new();
    cm.push_str("% @generated by `cargo xtask gen` from design/requirements.toml. DO NOT EDIT.\n");
    cm.push_str(
        "\\begin{tabular}{llllll}\n\\toprule\nRequirement & Phase & Status & Kind & Spec \\S & Tests \\\\\n\\midrule\n",
    );
    for r in &rows {
        let tests = if r.tests.is_empty() {
            "--".to_string()
        } else {
            r.tests.len().to_string()
        };
        cm.push_str(&format!(
            "{} & P{} & {} & {} & {} & {} \\\\\n",
            tex_escape(&r.id),
            r.phase,
            tex_escape(&r.status),
            tex_escape(&r.kind),
            tex_escape(&r.section),
            tex_escape(&tests),
        ));
    }
    cm.push_str("\\bottomrule\n\\end{tabular}\n");
    write_if_changed(MATRIX_TEX, &cm)?;

    println!("gen: wrote {RS_OUT}, {PARAMS_TEX}, {MATRIX_TEX}");
    Ok(())
}

/// Strip LaTeX line comments (`%` to end of line, unless escaped as `\%`) so
/// commented-out `\req{...}` examples are not treated as real references.
fn strip_tex_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        for i in 0..bytes.len() {
            if bytes[i] == b'%' && (i == 0 || bytes[i - 1] != b'\\') {
                cut = i;
                break;
            }
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Byte range `(start, end)` of the *content* inside `\name{...}` (excluding the
/// delimiting braces). Nested `{`/`}` inside the argument are balanced.
fn find_macro_content_ranges(tex: &str, name: &str) -> Vec<(usize, usize)> {
    let needle = format!("\\{name}{{");
    let mut regions = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = tex[search_from..].find(&needle) {
        let macro_start = search_from + rel;
        let open_brace = macro_start + needle.len() - 1;
        let Some(close_brace) = find_matching_brace(tex, open_brace) else {
            break;
        };
        regions.push((open_brace + 1, close_brace));
        search_from = close_brace + 1;
    }
    regions
}

fn find_matching_brace(tex: &str, open: usize) -> Option<usize> {
    tex.as_bytes().get(open).filter(|&&b| b == b'{')?;
    let mut depth = 0u32;
    for (i, ch) in tex[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

fn position_in_ranges(pos: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|&(s, e)| pos >= s && pos < e)
}

/// Map each `\req{ID}` in the hand-authored spec to whether it sits inside
/// `\intent{...}`. Used by the blur guard so target-design prose never reads
/// like shipped code (and vice versa).
fn spec_req_placement(tex: &str) -> BTreeMap<String, (bool, bool)> {
    let tex = strip_tex_comments(tex);
    let intent_ranges = find_macro_content_ranges(&tex, "intent");
    let req_re = match Regex::new(r"\\req\{([^}]+)\}") {
        Ok(r) => r,
        Err(_) => return BTreeMap::new(),
    };
    let mut out: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for cap in req_re.captures_iter(&tex) {
        let id = cap[1].to_string();
        let pos = cap.get(0).map(|m| m.start()).unwrap_or(0);
        let inside = position_in_ranges(pos, &intent_ranges);
        let entry = out.entry(id).or_insert((false, false));
        if inside {
            entry.1 = true;
        } else {
            entry.0 = true;
        }
    }
    out
}

fn lint_blur_guard(reqs: &Requirements, spec_tex: &str) -> Vec<String> {
    let placement = spec_req_placement(spec_tex);
    let mut errors = Vec::new();
    for req in &reqs.requirement {
        let (outside, inside) = placement.get(&req.id).copied().unwrap_or((false, false));
        match req.status.as_str() {
            "intended" => {
                if outside {
                    errors.push(format!(
                        "blur: requirement `{}` is intended but \\req{{}} appears outside \\intent{{}} in {SPEC_TEX} (spec must not read as shipped)",
                        req.id
                    ));
                }
                if !inside && !outside {
                    errors.push(format!(
                        "blur: requirement `{}` is intended but has no \\req{{}} in {SPEC_TEX}",
                        req.id
                    ));
                }
            }
            "implemented" if !outside => {
                errors.push(format!(
                    "blur: requirement `{}` is implemented but \\req{{}} is missing or only inside \\intent{{}} in {SPEC_TEX}",
                    req.id
                ));
            }
            _ => {}
        }
    }
    errors
}

fn collect_files(root: &str, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(root) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if p.file_name().map(|n| n == "target").unwrap_or(false) {
                continue;
            }
            collect_files(&p.to_string_lossy(), ext, out);
        } else if p.extension().map(|e| e == ext).unwrap_or(false) {
            out.push(p);
        }
    }
}

/// Collect the names of real test functions: those annotated `#[test]`, plus
/// functions defined inside a `proptest! { ... }` block (which the macro turns
/// into tests). This verifies a *test function exists*, not merely that an ID
/// appears near the string `#[test]`.
fn collect_test_fns(rs_files: &[PathBuf]) -> Result<BTreeSet<String>, Box<dyn Error>> {
    let test_attr = Regex::new(r"#\[test\]\s*(?:#\[[^\]]*\]\s*)*fn\s+([a-z0-9_]+)")?;
    let any_fn = Regex::new(r"\bfn\s+([a-z0-9_]+)\s*\(")?;
    let mut names = BTreeSet::new();
    for f in rs_files {
        let txt = fs::read_to_string(f)?;
        for c in test_attr.captures_iter(&txt) {
            names.insert(c[1].to_string());
        }
        // proptest! turns the functions in its block into #[test]s without a
        // visible attribute, so harvest function names from such files too.
        if txt.contains("proptest!") {
            for c in any_fn.captures_iter(&txt) {
                names.insert(c[1].to_string());
            }
        }
    }
    Ok(names)
}

fn lint() -> Result<(), Box<dyn Error>> {
    let reqs: Requirements = toml::from_str(&fs::read_to_string(REQS)?)?;
    let declared: BTreeSet<String> = reqs.requirement.iter().map(|r| r.id.clone()).collect();

    let id_re = Regex::new(r"\b(?:DEMI|ALG)-[A-Z0-9]+(?:-[A-Z0-9]+)*\b")?;
    let req_re = Regex::new(r"\\req\{([^}]+)\}")?;

    let mut rs_files = Vec::new();
    for d in SCAN_RS_DIRS {
        collect_files(d, "rs", &mut rs_files);
    }
    let test_fns = collect_test_fns(&rs_files)?;

    let mut refs_all: BTreeSet<String> = BTreeSet::new();
    for f in &rs_files {
        let txt = fs::read_to_string(f)?;
        for m in id_re.find_iter(&txt) {
            refs_all.insert(m.as_str().to_string());
        }
    }

    let mut tex_files = Vec::new();
    for d in SCAN_TEX_DIRS {
        collect_files(d, "tex", &mut tex_files);
    }
    for f in &tex_files {
        let txt = strip_tex_comments(&fs::read_to_string(f)?);
        for c in req_re.captures_iter(&txt) {
            refs_all.insert(c[1].to_string());
        }
    }

    let mut errors: Vec<String> = Vec::new();

    for r in refs_all.difference(&declared) {
        errors.push(format!(
            "reference to undeclared requirement `{r}` (add it to {REQS})"
        ));
    }
    for r in declared.difference(&refs_all) {
        errors.push(format!(
            "requirement `{r}` is declared but never referenced in spec or code"
        ));
    }

    for req in &reqs.requirement {
        match req.status.as_str() {
            "implemented" => {
                if !req.requires_test {
                    errors.push(format!(
                        "requirement `{}` is implemented but requires_test=false",
                        req.id
                    ));
                }
                if req.tests.is_empty() {
                    errors.push(format!(
                        "requirement `{}` is implemented but lists no backing tests",
                        req.id
                    ));
                }
                for t in &req.tests {
                    if !test_fns.contains(t) {
                        errors.push(format!(
                            "requirement `{}` names test `{t}` but no such #[test]/proptest fn exists",
                            req.id
                        ));
                    }
                }
            }
            "intended" => {
                if req.requires_test {
                    errors.push(format!(
                        "requirement `{}` is intended (no code) but requires_test=true",
                        req.id
                    ));
                }
                if !req.tests.is_empty() {
                    errors.push(format!(
                        "requirement `{}` is intended but lists backing tests",
                        req.id
                    ));
                }
            }
            other => errors.push(format!(
                "requirement `{}` has unknown status {other:?} (want implemented|intended)",
                req.id
            )),
        }
    }

    let spec_tex = fs::read_to_string(SPEC_TEX).unwrap_or_default();
    errors.extend(lint_blur_guard(&reqs, &spec_tex));

    if errors.is_empty() {
        let impl_n = reqs
            .requirement
            .iter()
            .filter(|r| r.status == "implemented")
            .count();
        let intended_n = reqs.requirement.len() - impl_n;
        println!(
            "lint: OK — {} requirements ({impl_n} implemented & test-backed, {intended_n} intended), all referenced, blur guard clean.",
            declared.len()
        );

        // Per-phase burndown (see ROADMAP.md): implemented / total per phase.
        let mut by_phase: BTreeMap<u32, (usize, usize)> = BTreeMap::new();
        for r in &reqs.requirement {
            let e = by_phase.entry(r.phase).or_default();
            e.1 += 1;
            if r.status == "implemented" {
                e.0 += 1;
            }
        }
        let phases: Vec<String> = by_phase
            .iter()
            .map(|(p, (done, total))| format!("P{p}: {done}/{total}"))
            .collect();
        println!("lint: phase burndown — {}", phases.join("  "));
        Ok(())
    } else {
        for e in &errors {
            eprintln!("  - {e}");
        }
        Err(format!("{} traceability error(s)", errors.len()).into())
    }
}
