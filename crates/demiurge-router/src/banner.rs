//! Startup banner for the `demiurge-router` binary.

use std::fmt::Write as FmtWrite;
use std::io::{self, IsTerminal, Write};

use demiurge_dataplane::AdmitMode;

use crate::{Phase, Router};

const W: usize = 76;

fn visible_len(s: &str) -> usize {
    let mut chars = s.chars().peekable();
    let mut len = 0usize;
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            while chars.next().is_some_and(|n| n != 'm') {}
            continue;
        }
        len += 1;
    }
    len
}

fn pad(inner: &str) -> String {
    let max = W - 4;
    let vis = visible_len(inner);
    debug_assert!(vis <= max, "banner line too long: {inner:?}");
    format!("║ {}{} ║", inner, " ".repeat(max.saturating_sub(vis)))
}

fn rule(ch: char) -> String {
    ch.to_string().repeat(W - 2)
}

struct Style {
    enabled: bool,
}

impl Style {
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.enabled {
            format!("{code}{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    fn title(&self, s: &str) -> String {
        self.wrap("\x1b[1;97m", s)
    }

    fn label(&self, s: &str) -> String {
        self.wrap("\x1b[2;36m", s)
    }

    fn value(&self, s: &str) -> String {
        self.wrap("\x1b[1m", s)
    }

    fn accent(&self, s: &str) -> String {
        self.wrap("\x1b[1;32m", s)
    }

    fn dim(&self, s: &str) -> String {
        self.wrap("\x1b[2m", s)
    }
}

fn admit_label(mode: AdmitMode, kernel: bool) -> &'static str {
    match mode {
        AdmitMode::Userspace => "userspace token bucket",
        AdmitMode::KernelXdp => "kernel XDP shed",
        AdmitMode::Hybrid if kernel => "hybrid · kernel XDP live",
        AdmitMode::Hybrid => "hybrid · userspace fallback",
    }
}

fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB && bytes.is_multiple_of(MIB) {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= 1024 {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

fn row(label: &str, value: &str, style: &Style) -> String {
    let left = format!("  {}", style.label(&format!("{label:<12}")));
    let right = style.value(value);
    let max = W - 4;
    let gap = max.saturating_sub(visible_len(&left) + visible_len(&right));
    format!("{left}{}{right}", " ".repeat(gap))
}

/// Render the router startup banner (tests and programmatic use).
pub fn render_startup_banner(
    router: &Router,
    listen: &str,
    xdp_iface: Option<&str>,
    color: bool,
) -> String {
    let style = Style { enabled: color };
    let mut out = String::new();

    let version = env!("CARGO_PKG_VERSION");
    let admit_mode = router.admit_mode();
    let kernel = router.kernel_admit_attached();
    let pi = router.dataplane_pi();
    let pf_n = router.pool(Phase::Prefill).len();
    let dc_n = router.pool(Phase::Decode).len();
    let reb = if router.rebalancer_actuation() {
        format!("actuated · π={pi:.3}")
    } else {
        "shadow".into()
    };

    let _ = writeln!(out, "╔{}╗", rule('═'));
    let _ = writeln!(out, "{}", pad(""));
    let title_left = style.title("DEMIURGE ROUTER");
    let title_right = style.dim("phase-aware forwarder");
    let title_gap = (W - 4).saturating_sub(visible_len(&title_left) + visible_len(&title_right));
    let _ = writeln!(
        out,
        "║ {}{}{} ║",
        title_left,
        " ".repeat(title_gap),
        title_right
    );
    let _ = writeln!(
        out,
        "{}",
        pad(&style.dim("KV-native routing · disaggregated prefill/decode · RCU dataplane"))
    );
    let _ = writeln!(out, "{}", pad(""));
    let _ = writeln!(out, "╠{}╣", rule('═'));
    let _ = writeln!(out, "{}", pad(&row("listen", listen, &style)));
    let admit_cap = router.admit_bucket().capacity();
    let _ = writeln!(
        out,
        "{}",
        pad(&row(
            "admit",
            &format!("{} · burst {}", admit_label(admit_mode, kernel), admit_cap),
            &style
        ))
    );
    let xdp = if kernel {
        xdp_iface
            .map(|i| format!("attached · {i}"))
            .unwrap_or_else(|| "attached".into())
    } else {
        "off".into()
    };
    let _ = writeln!(out, "{}", pad(&row("kernel xdp", &xdp, &style)));
    #[cfg(target_os = "linux")]
    let io_uring = if router.io_uring_enabled() {
        "production TCP proxy"
    } else {
        "off"
    };
    #[cfg(not(target_os = "linux"))]
    let io_uring = "off (linux only)";
    let _ = writeln!(out, "{}", pad(&row("io_uring", io_uring, &style)));
    let _ = writeln!(out, "{}", pad(&row("rebalancer", &reb, &style)));
    let _ = writeln!(out, "╟{}╢", rule('─'));
    let _ = writeln!(
        out,
        "{}",
        pad(&row(
            "prefill pool",
            &format!("{pf_n} backend{}", if pf_n == 1 { "" } else { "s" }),
            &style
        ))
    );
    let _ = writeln!(
        out,
        "{}",
        pad(&row(
            "decode pool",
            &format!("{dc_n} backend{}", if dc_n == 1 { "" } else { "s" }),
            &style
        ))
    );
    if let Some(ledger) = router.ledger() {
        let cap = format_bytes(ledger.capacity_bytes());
        let _ = writeln!(
            out,
            "{}",
            pad(&row("kv pool", &format!("{cap} decode cap"), &style))
        );
    }
    let _ = writeln!(out, "╠{}╣", rule('═'));
    let ready = format!(
        "{}  {}  v{version}  {}",
        style.accent("● READY"),
        style.dim("accepting connections"),
        style.dim("Ctrl+C to stop")
    );
    let _ = writeln!(out, "{}", pad(&ready));
    let _ = writeln!(out, "╚{}╝", rule('═'));

    out
}

fn banner_enabled() -> bool {
    match std::env::var("DEMIURGE_BANNER").ok().as_deref() {
        Some("0") | Some("false") | Some("no") => false,
        Some("1") | Some("true") | Some("yes") => true,
        _ => io::stderr().is_terminal(),
    }
}

fn quiet_mode() -> bool {
    matches!(
        std::env::var("DEMIURGE_QUIET").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Print startup banner to stderr (compact line when quiet or non-TTY).
pub fn print_startup_banner(router: &Router, listen: &str, xdp_iface: Option<&str>) {
    if quiet_mode() || !banner_enabled() {
        let _ = writeln!(
            io::stderr(),
            "demiurge-router v{} listening on {listen} (prefill={}, decode={}, admit={:?}, xdp={}, io_uring={})",
            env!("CARGO_PKG_VERSION"),
            router.pool(Phase::Prefill).len(),
            router.pool(Phase::Decode).len(),
            router.admit_mode(),
            router.kernel_admit_attached(),
            router.io_uring_enabled(),
        );
        return;
    }

    let color = io::stderr().is_terminal()
        && !matches!(
            std::env::var("NO_COLOR").ok().as_deref(),
            Some("1") | Some("true")
        );
    let banner = render_startup_banner(router, listen, xdp_iface, color);
    let _ = writeln!(io::stderr(), "{banner}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn test_router() -> Router {
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        Router::new(
            vec![crate::Backend::new("pf0", a, 0.01)],
            vec![crate::Backend::new("dc0", a, 0.02)],
        )
    }

    #[test]
    fn banner_contains_listen_and_version() {
        let router = test_router();
        let text = render_startup_banner(&router, "127.0.0.1:8080", None, false);
        assert!(text.contains("DEMIURGE ROUTER"));
        assert!(text.contains("127.0.0.1:8080"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        assert!(text.contains("READY"));
    }

    #[test]
    fn banner_renders_kv_pool_when_configured() {
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let pf = vec![crate::Backend::new("pf", a, 0.01)];
        let dc = vec![crate::Backend::new("dc", a, 0.02)];
        let (router, _, _) = Router::with_kv_pool(pf, dc, 128 * 1024 * 1024, 128);
        let text = render_startup_banner(&router, "127.0.0.1:9090", None, false);
        assert!(text.contains("kv pool"));
        assert!(text.contains("128.0 MiB"));
    }
}
