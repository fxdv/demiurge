//! Runtime XDP attach on a veth pair (Linux + root). [DEMI-XDP-SHED]
#![cfg(target_os = "linux")]

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use demiurge_dataplane::{XdpAdmitShed, XdpAttachError, XDP_DEFAULT_CAPACITY};

static VETH_SEQ: AtomicUsize = AtomicUsize::new(0);

struct VethPair {
    iface: String,
    peer: String,
}

impl VethPair {
    fn create() -> Result<Self, String> {
        let n = VETH_SEQ.fetch_add(1, Ordering::Relaxed);
        let a = format!("demi-a{n}");
        let b = format!("demi-b{n}");
        run_ip(&["link", "add", &a, "type", "veth", "peer", "name", &b])?;
        run_ip(&["addr", "add", "192.0.2.1/30", "dev", &a])?;
        run_ip(&["addr", "add", "192.0.2.2/30", "dev", &b])?;
        run_ip(&["link", "set", &a, "up"])?;
        run_ip(&["link", "set", &b, "up"])?;
        Ok(Self { iface: a, peer: b })
    }
}

impl Drop for VethPair {
    fn drop(&mut self) {
        let _ = run_ip(&["link", "del", &self.iface]);
    }
}

fn run_ip(args: &[&str]) -> Result<(), String> {
    let status = Command::new("ip")
        .args(args.iter().copied())
        .status()
        .map_err(|e| format!("ip: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ip exited with {status}"))
    }
}

fn require_root_and_bpf() -> Result<(), String> {
    let root = Command::new("id")
        .arg("-u")
        .output()
        .map(|o| o.stdout == b"0\n")
        .unwrap_or(false);
    if !root {
        return Err("requires root (CAP_BPF / net admin)".into());
    }
    if !XdpAdmitShed::object_path().is_file() {
        return Err(format!(
            "missing {}; run ./scripts/build-bpf.sh",
            XdpAdmitShed::object_path().display()
        ));
    }
    Ok(())
}

#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_attaches_and_seeds_map() {
    require_root_and_bpf().expect("precheck");
    let veth = VethPair::create().expect("veth");
    let shed = XdpAdmitShed::attach(&veth.iface, 4).expect("attach");
    assert_eq!(shed.available().expect("tokens"), 4);
    assert_eq!(shed.capacity().expect("cap"), 4);
    assert_eq!(shed.shed_total().expect("shed"), 0);
}

#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_reseed_updates_tokens() {
    require_root_and_bpf().expect("precheck");
    let veth = VethPair::create().expect("veth");
    let mut shed = XdpAdmitShed::attach(&veth.iface, 4).expect("attach");
    assert_eq!(shed.available().expect("tokens"), 4);

    shed.reseed(2).expect("reseed");
    assert_eq!(shed.available().expect("tokens after reseed"), 2);
    assert_eq!(shed.capacity().expect("cap"), 2);
    assert_eq!(shed.shed_total().expect("shed"), 0);
}

fn send_probes_via_ping(peer: &str, dest: &str, count: u32) -> Result<(), String> {
    let status = Command::new("ping")
        .args([
            "-I",
            peer,
            "-c",
            &count.to_string(),
            "-W",
            "1",
            "-i",
            "0.01",
            dest,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("ping: {e}"))?;
    // Some probes may fail once tokens are exhausted; only require the command to run.
    if status.success() || status.code() == Some(1) {
        Ok(())
    } else {
        Err(format!("ping exited with {status}"))
    }
}

#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_drops_packets_when_exhausted() {
    require_root_and_bpf().expect("precheck");
    std::env::set_var("DEMIURGE_XDP_FLAGS", "skb");
    let veth = VethPair::create().expect("veth");
    let shed = XdpAdmitShed::attach(&veth.iface, 2).expect("attach");

    send_probes_via_ping(&veth.peer, "192.0.2.1", 8).expect("ping probes");

    let shed_count = shed.shed_total().expect("shed");
    let tokens = shed.available().expect("tokens");
    std::env::remove_var("DEMIURGE_XDP_FLAGS");
    assert!(
        shed_count >= 1,
        "expected XDP_DROP after token exhaustion (shed={shed_count}, tokens={tokens})"
    );
    assert_eq!(tokens, 0);
}

#[test]
fn xdp_attach_errors_without_object_on_linux() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let missing = std::env::temp_dir().join("demiurge-missing-bpf.o");
    assert!(!missing.is_file());
    std::env::set_var("DEMIURGE_BPF_OBJECT", missing.to_string_lossy().as_ref());
    let err = match XdpAdmitShed::attach("lo", XDP_DEFAULT_CAPACITY) {
        Err(e) => e,
        Ok(_) => panic!("expected ObjectNotBuilt"),
    };
    std::env::remove_var("DEMIURGE_BPF_OBJECT");
    assert!(matches!(err, XdpAttachError::ObjectNotBuilt));
}
