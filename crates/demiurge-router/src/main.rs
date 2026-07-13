//! `demiurge-router` binary: a minimal phase-aware, cost-based forwarder.
//!
//! Configuration via environment:
//!   DEMIURGE_LISTEN        listen address           (default 127.0.0.1:8080)
//!   DEMIURGE_PREFILL       prefill pool spec         label@host:port@seconds,...
//!   DEMIURGE_DECODE        decode pool spec          label@host:port@seconds,...
//!   DEMIURGE_TOPOLOGY       label@node/rack/cluster,... (optional RDMA shadow)
//!   DEMIURGE_ADMIT_MODE    userspace | xdp | hybrid  (default userspace)
//!   DEMIURGE_XDP_IFACE     attach kernel admit-shed on this iface (Linux)
//!   DEMIURGE_IOURING       1 for io_uring recv/send on production TCP proxy (Linux)
//!   DEMIURGE_BANNER        0|1 force disable/enable startup banner (default: TTY)
//!   DEMIURGE_HANDOFF_TRANSPORT  tcp (default) | mock_rdma | modeled_rdma
//!   DEMIURGE_RDMA_ROUTING       1 to use topology transfer model in decode placement
//!   DEMIURGE_DECODE_KV_CAPACITY_BYTES  fleet decode KV budget (enables ledger + handoff)
//!   DEMIURGE_BYTES_PER_TOKEN   bytes per KV token for reservation (default 128)
//!   DEMIURGE_STATE_PLANE       1 to record live warmth on production traffic

use std::collections::HashMap;
use std::net::TcpListener;
use std::process::exit;
use std::sync::Arc;

use demiurge_cost::TopologyId;
use demiurge_dataplane::AdmitMode;
use demiurge_handoff::handoff_transport_from_env;
use demiurge_router::{
    parse_pool_with_topology, parse_topology_map, print_startup_banner, serve, Phase, Router,
    StatePlane,
};

fn main() {
    if let Err(e) = run() {
        eprintln!("demiurge-router: {e}");
        exit(1);
    }
}

fn parse_u64_env(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
}

/// Read a `DEMIURGE_*` env var as a boolean flag, falling back to `default`.
fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(default)
}

fn build_router(
    prefill: Vec<Arc<demiurge_router::Backend>>,
    decode: Vec<Arc<demiurge_router::Backend>>,
    topology: HashMap<String, TopologyId>,
) -> Result<Router, String> {
    let bytes_per_token = parse_u64_env("DEMIURGE_BYTES_PER_TOKEN").unwrap_or(128);
    let kv_capacity = parse_u64_env("DEMIURGE_DECODE_KV_CAPACITY_BYTES");
    let state_plane_on = env_bool("DEMIURGE_STATE_PLANE", false) || kv_capacity.is_some();

    let admit_mode = AdmitMode::from_env();
    let transport = handoff_transport_from_env(topology);

    let mut router = if let Some(capacity) = kv_capacity {
        let (r, _, _) = Router::with_kv_pool(prefill, decode, capacity, bytes_per_token);
        r.with_handoff_transport(transport)
    } else {
        Router::new(prefill, decode).with_handoff_transport(transport)
    };

    router = router.with_admit_mode(admit_mode);

    if state_plane_on {
        let pf_labels: Vec<String> = router
            .pool(Phase::Prefill)
            .iter()
            .map(|b| b.label.clone())
            .collect();
        let dc_labels: Vec<String> = router
            .pool(Phase::Decode)
            .iter()
            .map(|b| b.label.clone())
            .collect();
        router = router.with_state_plane(StatePlane::new(&pf_labels, &dc_labels));
    }

    Ok(router)
}

/// Parse env, bind listen socket, build router — everything before the accept loop.
fn configure() -> Result<(TcpListener, Arc<Router>), String> {
    let listen = std::env::var("DEMIURGE_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let topology = parse_topology_map(&std::env::var("DEMIURGE_TOPOLOGY").unwrap_or_default())?;
    let prefill = parse_pool_with_topology(
        &std::env::var("DEMIURGE_PREFILL").unwrap_or_default(),
        &topology,
    )?;
    let decode = parse_pool_with_topology(
        &std::env::var("DEMIURGE_DECODE").unwrap_or_default(),
        &topology,
    )?;

    if prefill.is_empty() && decode.is_empty() {
        return Err(
            "no backends; set DEMIURGE_PREFILL and/or DEMIURGE_DECODE (label@host:port@seconds,...)"
                .into(),
        );
    }

    let listener = TcpListener::bind(&listen).map_err(|e| format!("bind {listen}: {e}"))?;

    let mut router = build_router(prefill, decode, topology)?;
    let xdp_iface = std::env::var("DEMIURGE_XDP_IFACE").ok();
    if let Some(ref iface) = xdp_iface {
        router = router
            .with_kernel_admit(iface)
            .map_err(|e| format!("XDP attach on {iface}: {e}"))?;
    }

    print_startup_banner(&router, &listen, xdp_iface.as_deref());

    Ok((listener, Arc::new(router)))
}

fn run() -> Result<(), String> {
    let (listener, router) = configure()?;
    serve(listener, router).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&str, Option<&str>)]) -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let mut saved = Vec::new();
            for (key, val) in vars {
                saved.push(((*key).to_string(), std::env::var(key).ok()));
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, old) in &self.saved {
                match old {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn configure_rejects_empty_pools() {
        let _env = EnvGuard::set(&[("DEMIURGE_PREFILL", None), ("DEMIURGE_DECODE", None)]);
        let Err(err) = configure() else {
            panic!("expected configure to fail on empty pools");
        };
        assert!(err.contains("no backends"));
    }

    #[test]
    fn configure_rejects_invalid_pool_spec() {
        let _env = EnvGuard::set(&[
            ("DEMIURGE_PREFILL", Some("not-a-valid-spec")),
            ("DEMIURGE_DECODE", None),
            ("DEMIURGE_LISTEN", Some("127.0.0.1:0")),
        ]);
        let Err(err) = configure() else {
            panic!("expected configure to fail on invalid pool spec");
        };
        assert!(err.contains("bad backend spec"));
    }

    #[test]
    fn configure_binds_ephemeral_listener() {
        let _env = EnvGuard::set(&[
            ("DEMIURGE_PREFILL", Some("pf@127.0.0.1:9@0.01")),
            ("DEMIURGE_DECODE", None),
            ("DEMIURGE_LISTEN", Some("127.0.0.1:0")),
            ("DEMIURGE_BANNER", Some("0")),
        ]);
        let (listener, _router) = configure().unwrap();
        assert!(listener.local_addr().unwrap().port() > 0);
    }

    #[test]
    fn configure_accepts_decode_only_pool() {
        let _env = EnvGuard::set(&[
            ("DEMIURGE_PREFILL", None),
            ("DEMIURGE_DECODE", Some("dc@127.0.0.1:9@0.02")),
            ("DEMIURGE_LISTEN", Some("127.0.0.1:0")),
            ("DEMIURGE_BANNER", Some("0")),
        ]);
        let (listener, _router) = configure().unwrap();
        assert!(listener.local_addr().unwrap().port() > 0);
    }

    #[test]
    fn configure_enables_kv_ledger_and_state_plane() {
        let _env = EnvGuard::set(&[
            (
                "DEMIURGE_PREFILL",
                Some("pf0@127.0.0.1:9@0.01,pf1@127.0.0.1:10@0.01"),
            ),
            (
                "DEMIURGE_DECODE",
                Some("dc0@127.0.0.1:11@0.02,dc1@127.0.0.1:12@0.02"),
            ),
            ("DEMIURGE_LISTEN", Some("127.0.0.1:0")),
            ("DEMIURGE_BANNER", Some("0")),
            ("DEMIURGE_DECODE_KV_CAPACITY_BYTES", Some("67108864")),
            ("DEMIURGE_BYTES_PER_TOKEN", Some("128")),
        ]);
        let (_listener, router) = configure().unwrap();
        assert!(router.ledger().is_some());
        assert!(router.handoffs().is_some());
        assert!(router.state_plane_active());
        assert_eq!(router.bytes_per_token(), 128);
    }
}
