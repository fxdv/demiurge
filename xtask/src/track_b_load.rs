//! Track B load-bench helpers (Linux + root for veth/XDP).

#![cfg(target_os = "linux")]

use std::net::{Ipv4Addr, TcpListener};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use demiurge_router::Router;

static VETH_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| o.stdout == b"0\n")
        .unwrap_or(false)
}

fn run_ip(args: &[&str]) -> Result<(), String> {
    let status = Command::new("ip")
        .args(args)
        .status()
        .map_err(|e| format!("ip: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ip exited with {status}"))
    }
}

/// veth pair for kernel XDP attach; router listens on `listen_ip`.
pub struct TrackBVeth {
    iface: String,
    pub listen_ip: Ipv4Addr,
}

impl TrackBVeth {
    pub fn create() -> Result<Self, String> {
        if !is_root() {
            return Err("track_b_kernel requires root (CAP_NET_ADMIN)".into());
        }
        let n = VETH_SEQ.fetch_add(1, Ordering::Relaxed);
        let a = format!("demi-lb{n}");
        let b = format!("demi-lbp{n}");
        run_ip(&["link", "add", &a, "type", "veth", "peer", "name", &b])?;
        run_ip(&["addr", "add", "192.0.2.1/30", "dev", &a])?;
        run_ip(&["addr", "add", "192.0.2.2/30", "dev", &b])?;
        run_ip(&["link", "set", &a, "up"])?;
        run_ip(&["link", "set", &b, "up"])?;
        Ok(Self {
            iface: a,
            listen_ip: Ipv4Addr::new(192, 0, 2, 1),
        })
    }

    pub fn attach_router(&self, router: Router) -> Result<Router, String> {
        // Dedicated veth: gate every TCP SYN (the router binds an ephemeral
        // port after attach, so there is no port to narrow to yet).
        router
            .with_kernel_admit(&self.iface, None)
            .map_err(|e| format!("XDP attach on {}: {e}", self.iface))
    }
}

impl Drop for TrackBVeth {
    fn drop(&mut self) {
        let _ = run_ip(&["link", "del", &self.iface]);
    }
}

pub fn bind_router_listener(track_b_veth: &Option<TrackBVeth>) -> Result<TcpListener, String> {
    let ip = track_b_veth
        .as_ref()
        .map(|v| v.listen_ip)
        .unwrap_or(Ipv4Addr::LOCALHOST);
    TcpListener::bind((ip, 0)).map_err(|e| format!("bind router: {e}"))
}
