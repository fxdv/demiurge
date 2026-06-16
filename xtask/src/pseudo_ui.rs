//! Shared pseudo-graphical box width for load-bench and harden reports.
//!
//! Override at runtime: `DEMIURGE_PSEUDO_WIDTH=140 cargo xtask load-report`

pub const DEFAULT_PSEUDO_WIDTH: usize = 120;

/// Box inner width (default 120; env `DEMIURGE_PSEUDO_WIDTH` in 80..=200).
pub fn pseudo_width() -> usize {
    std::env::var("DEMIURGE_PSEUDO_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&w| (80..=200).contains(&w))
        .unwrap_or(DEFAULT_PSEUDO_WIDTH)
}

pub fn inner_width(w: usize) -> usize {
    w.saturating_sub(4)
}

pub fn pad_line(inner: &str, w: usize) -> String {
    let max_len = inner_width(w);
    let content = if inner.chars().count() > max_len {
        let truncated: String = inner.chars().take(max_len - 1).collect();
        format!("{truncated}…")
    } else {
        inner.to_string()
    };
    format!("║ {content:<width$} ║", width = max_len)
}

pub fn h_rule(w: usize, ch: char) -> String {
    ch.to_string().repeat(w.saturating_sub(2))
}

pub fn histogram_bar_width(w: usize) -> usize {
    inner_width(w).saturating_sub(24).clamp(32, 56)
}
