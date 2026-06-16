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
//!   DEMIURGE_QUIET         1 for compact one-line startup

use std::net::TcpListener;
use std::process::exit;
use std::sync::Arc;

use demiurge_dataplane::AdmitMode;
use demiurge_router::{
    parse_pool_with_topology, parse_topology_map, print_startup_banner, serve, Router,
};

fn main() {
    if let Err(e) = run() {
        eprintln!("demiurge-router: {e}");
        exit(1);
    }
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

    let admit_mode = AdmitMode::from_env();
    let mut router = Router::new(prefill, decode).with_admit_mode(admit_mode);
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
}
