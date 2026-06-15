//! Integration tests for the spec blur guard (kept out of `lint`'s Rust scan).

use std::collections::BTreeMap;

#[test]
fn intent_range_balances_nested_req() {
    let tex = r"\intent{foo \req{ZZZ-IN-INTENT} bar}";
    let ranges = find_macro_content_ranges(tex, "intent");
    assert_eq!(ranges.len(), 1);
    let placement = spec_req_placement(tex);
    assert!(placement["ZZZ-IN-INTENT"].1);
    assert!(!placement["ZZZ-IN-INTENT"].0);
}

#[test]
fn blur_guard_catches_intended_outside_intent() {
    let tex = r"Normative \req{ZZZ-OUTSIDE}.";
    let reqs = sample_req("intended", "ZZZ-OUTSIDE");
    let errs = lint_blur_guard(&reqs, tex);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].contains("blur"));
}

#[test]
fn blur_guard_catches_implemented_only_in_intent() {
    let tex = r"\intent{planned \req{ZZZ-ONLY-INTENT}}";
    let reqs = sample_req("implemented", "ZZZ-ONLY-INTENT");
    let errs = lint_blur_guard(&reqs, tex);
    assert_eq!(errs.len(), 1);
}

// ---- minimal copies of xtask helpers (not exported from the binary crate) ----

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

fn position_in_ranges(pos: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|&(s, e)| pos >= s && pos < e)
}

fn spec_req_placement(tex: &str) -> BTreeMap<String, (bool, bool)> {
    let tex = strip_tex_comments(tex);
    let intent_ranges = find_macro_content_ranges(&tex, "intent");
    let req_re = regex::Regex::new(r"\\req\{([^}]+)\}").expect("regex");
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

struct Requirement {
    id: String,
    status: String,
}

struct Requirements {
    requirement: Vec<Requirement>,
}

fn sample_req(status: &str, id: &str) -> Requirements {
    Requirements {
        requirement: vec![Requirement {
            id: id.to_string(),
            status: status.to_string(),
        }],
    }
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
                        "blur: requirement `{}` is intended but \\req{{}} appears outside \\intent{{}}",
                        req.id
                    ));
                }
                if !inside && !outside {
                    errors.push(format!(
                        "blur: requirement `{}` is intended but has no \\req{{}}",
                        req.id
                    ));
                }
            }
            "implemented" if !outside => {
                errors.push(format!(
                    "blur: requirement `{}` is implemented but \\req{{}} is missing or only inside \\intent{{}}",
                    req.id
                ));
            }
            _ => {}
        }
    }
    errors
}
