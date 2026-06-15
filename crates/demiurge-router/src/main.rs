//! `demiurge-router` binary: a minimal phase-aware, cost-based forwarder.
//!
//! Configuration via environment:
//!   DEMIURGE_LISTEN        listen address           (default 127.0.0.1:8080)
//!   DEMIURGE_PREFILL       prefill pool spec         label@host:port@seconds,...
//!   DEMIURGE_DECODE        decode pool spec          label@host:port@seconds,...
//!   DEMIURGE_ADMIT_MODE    userspace | xdp | hybrid  (default userspace)
//!   DEMIURGE_XDP_IFACE     attach kernel admit-shed on this iface (Linux)
//!   DEMIURGE_IOURING       1 for io_uring recv/send on production TCP proxy (Linux)
//!   DEMIURGE_BANNER        0|1 force disable/enable startup banner (default: TTY)
//!   DEMIURGE_QUIET         1 for compact one-line startup

use std::net::TcpListener;
use std::process::exit;
use std::sync::Arc;

use demiurge_dataplane::AdmitMode;
use demiurge_router::{parse_pool, print_startup_banner, serve, Router};

fn main() {
    if let Err(e) = run() {
        eprintln!("demiurge-router: {e}");
        exit(1);
    }
}

fn run() -> Result<(), String> {
    let listen = std::env::var("DEMIURGE_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let prefill = parse_pool(&std::env::var("DEMIURGE_PREFILL").unwrap_or_default())?;
    let decode = parse_pool(&std::env::var("DEMIURGE_DECODE").unwrap_or_default())?;

    if prefill.is_empty() && decode.is_empty() {
        return Err(
            "no backends; set DEMIURGE_PREFILL and/or DEMIURGE_DECODE (label@host:port@seconds,...)"
                .into(),
        );
    }

    let listener = TcpListener::bind(&listen).map_err(|e| format!("bind {listen}: {e}"))?;

    let admit_mode = AdmitMode::from_env();
    let mut router = Router::new(prefill, decode).with_admit_mode(admit_mode);
    let xdp_iface = std::env::var("DEMIURGE_XDP_IFACE").ok();
    if let Some(ref iface) = xdp_iface {
        router = router
            .with_kernel_admit(iface)
            .map_err(|e| format!("XDP attach on {iface}: {e}"))?;
    }

    print_startup_banner(&router, &listen, xdp_iface.as_deref());

    let router = Arc::new(router);
    serve(listener, router).map_err(|e| e.to_string())
}
