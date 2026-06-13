//! `demiurge-router` binary: a minimal phase-aware, cost-based forwarder.
//!
//! Configuration via environment:
//!   DEMIURGE_LISTEN   listen address           (default 127.0.0.1:8080)
//!   DEMIURGE_PREFILL  prefill pool spec         label@host:port@seconds,...
//!   DEMIURGE_DECODE   decode pool spec          label@host:port@seconds,...

use std::net::TcpListener;
use std::process::exit;
use std::sync::Arc;

use demiurge_router::{parse_pool, serve, Router};

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
    eprintln!(
        "demiurge-router listening on {listen} (prefill={}, decode={})",
        prefill.len(),
        decode.len()
    );
    let router = Arc::new(Router::new(prefill, decode));
    serve(listener, router).map_err(|e| e.to_string())
}
