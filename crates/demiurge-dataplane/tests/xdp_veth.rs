//! Runtime XDP attach on a veth pair (Linux + root). [DEMI-XDP-SHED]
//!
//! The kernel bucket gates *new-connection TCP SYNs only*; TCP probes are
//! sent from a peer network namespace so they traverse the wire (same-netns
//! TCP would short-circuit via the local route and never hit XDP).
#![cfg(target_os = "linux")]

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use demiurge_dataplane::{XdpAdmitConfig, XdpAdmitShed, XdpAttachError};

static VETH_SEQ: AtomicUsize = AtomicUsize::new(0);

const HOST_IP: &str = "192.0.2.1";

struct VethPair {
    iface: String,
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
        Ok(Self { iface: a })
    }
}

impl Drop for VethPair {
    fn drop(&mut self) {
        let _ = run_ip(&["link", "del", &self.iface]);
    }
}

/// veth pair with the peer end in its own netns: traffic from the netns
/// genuinely traverses the veth and hits XDP on the host-side ingress.
struct VethNs {
    iface: String,
    ns: String,
}

impl VethNs {
    fn create() -> Result<Self, String> {
        let n = VETH_SEQ.fetch_add(1, Ordering::Relaxed);
        let a = format!("demi-na{n}");
        let b = format!("demi-nb{n}");
        let ns = format!("demi-ns{n}");
        run_ip(&["link", "add", &a, "type", "veth", "peer", "name", &b])?;
        run_ip(&["netns", "add", &ns])?;
        run_ip(&["link", "set", &b, "netns", &ns])?;
        run_ip(&["addr", "add", "192.0.2.1/30", "dev", &a])?;
        run_ip(&["link", "set", &a, "up"])?;
        run_ip(&["-n", &ns, "addr", "add", "192.0.2.2/30", "dev", &b])?;
        run_ip(&["-n", &ns, "link", "set", &b, "up"])?;
        run_ip(&["-n", &ns, "link", "set", "lo", "up"])?;
        // Static neighbors: the first ICMP probe otherwise races ARP on slow CI
        // hosts and `ping -c N` fails on any single lost reply.
        let host_mac = iface_mac(&a)?;
        let peer_mac = iface_mac_in_netns(&ns, &b)?;
        run_ip(&[
            "neigh",
            "add",
            "192.0.2.2",
            "lladdr",
            &peer_mac,
            "dev",
            &a,
            "nud",
            "perm",
        ])?;
        run_ip(&[
            "-n", &ns, "neigh", "add", HOST_IP, "lladdr", &host_mac, "dev", &b, "nud", "perm",
        ])?;
        Ok(Self { iface: a, ns })
    }

    /// Ping `HOST_IP` from inside the netns; true when a round-trip succeeds.
    /// (Same-netns `ping -I peer` cannot work here: requests traverse the
    /// veth but replies to a local address come back via loopback, which a
    /// device-bound ping ignores.)
    fn ping_reliable(&self) -> Result<bool, String> {
        for attempt in 0..5 {
            if self.ping_once()? {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(50 * (attempt as u64 + 1)));
        }
        Ok(false)
    }

    fn ping_once(&self) -> Result<bool, String> {
        let status = Command::new("ip")
            .args([
                "netns", "exec", &self.ns, "ping", "-c", "1", "-W", "2", HOST_IP,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("ping: {e}"))?;
        Ok(status.success())
    }

    /// One TCP connection attempt from the netns to `HOST_IP:port`.
    /// No listener runs on the host side: an *admitted* SYN elicits an
    /// immediate RST (fast failure), a *shed* SYN times out. Either way the
    /// SYN itself traversed the wire, which is all these tests need.
    /// `timeout` is passed to coreutils `timeout` (e.g. `"0.2"`, `"1"`).
    fn syn_probe(&self, port: u16, timeout: &str) {
        let _ = Command::new("ip")
            .args([
                "netns",
                "exec",
                &self.ns,
                "timeout",
                timeout,
                "bash",
                "-c",
                &format!("echo > /dev/tcp/{HOST_IP}/{port}"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

impl Drop for VethNs {
    fn drop(&mut self) {
        let _ = run_ip(&["link", "del", &self.iface]);
        let _ = run_ip(&["netns", "del", &self.ns]);
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

fn parse_iface_mac(output: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(output).map_err(|e| format!("ip link utf8: {e}"))?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("    link/ether ") {
            let mac = rest.split_whitespace().next().unwrap_or("");
            if !mac.is_empty() {
                return Ok(mac.to_string());
            }
        }
    }
    Err(format!("no link/ether in ip link output: {text}"))
}

fn iface_mac(iface: &str) -> Result<String, String> {
    let output = Command::new("ip")
        .args(["link", "show", iface])
        .output()
        .map_err(|e| format!("ip link show {iface}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ip link show {iface} exited with {}",
            output.status
        ));
    }
    parse_iface_mac(&output.stdout)
}

fn iface_mac_in_netns(ns: &str, iface: &str) -> Result<String, String> {
    let output = Command::new("ip")
        .args(["-n", ns, "link", "show", iface])
        .output()
        .map_err(|e| format!("ip -n {ns} link show {iface}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ip -n {ns} link show {iface} exited with {}",
            output.status
        ));
    }
    parse_iface_mac(&output.stdout)
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

fn config(capacity: u64, refill_per_sec: u64, listen_port: Option<u16>) -> XdpAdmitConfig {
    XdpAdmitConfig {
        capacity,
        refill_per_sec,
        listen_port,
    }
}

#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_attaches_and_seeds_map() {
    require_root_and_bpf().expect("precheck");
    let veth = VethPair::create().expect("veth");
    let shed = XdpAdmitShed::attach(&veth.iface, config(4, 0, None)).expect("attach");
    assert_eq!(shed.available().expect("tokens"), 4);
    assert_eq!(shed.capacity().expect("cap"), 4);
    assert_eq!(shed.shed_total().expect("shed"), 0);
    assert_eq!(shed.pass_total().expect("pass"), 0);
    assert!(
        ["driver", "skb", "skb-fallback"].contains(&shed.attach_mode()),
        "unexpected attach mode {}",
        shed.attach_mode()
    );
    assert!(shed.link_alive());
}

/// G5b — admin detach must clear `link_alive` while the iface still exists
/// (Hybrid falls back to the userspace bucket). SKB attaches need
/// `xdpgeneric off`; driver attaches use `xdp off`.
#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_detects_admin_xdp_off() {
    require_root_and_bpf().expect("precheck");
    let veth = VethPair::create().expect("veth");
    let shed = XdpAdmitShed::attach(&veth.iface, config(4, 0, None)).expect("attach");
    assert!(shed.link_alive(), "fresh attach must report alive");

    let off = match shed.attach_mode() {
        "skb" | "skb-fallback" => "xdpgeneric",
        _ => "xdp",
    };
    run_ip(&["link", "set", "dev", &veth.iface, off, "off"]).expect("admin xdp detach");
    assert!(
        !shed.link_alive(),
        "admin {off} off must be visible to link_alive (mode={})",
        shed.attach_mode()
    );
}

#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_reseed_updates_tokens() {
    require_root_and_bpf().expect("precheck");
    let veth = VethPair::create().expect("veth");
    let mut shed = XdpAdmitShed::attach(&veth.iface, config(4, 0, None)).expect("attach");
    assert_eq!(shed.available().expect("tokens"), 4);

    shed.reseed(2).expect("reseed");
    assert_eq!(shed.available().expect("tokens after reseed"), 2);
    assert_eq!(shed.capacity().expect("cap"), 2);
    assert_eq!(shed.shed_total().expect("shed"), 0);
}

// The bucket is an overload valve for *new work*, never a packet firewall:
// ICMP (and any non-SYN traffic) passes untouched even with zero tokens.
#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_passes_non_syn_traffic() {
    require_root_and_bpf().expect("precheck");
    let veth = VethNs::create().expect("veth+ns");
    let shed = XdpAdmitShed::attach(&veth.iface, config(1, 0, None)).expect("attach");

    let ok = veth.ping_reliable().expect("ping probes");
    assert!(ok, "ICMP must pass regardless of bucket state");
    assert_eq!(shed.shed_total().expect("shed"), 0, "ICMP is never gated");
    assert_eq!(
        shed.available().expect("tokens"),
        1,
        "non-SYN traffic must not consume tokens"
    );
}

// [DEMI-XDP-SHED] — new-connection SYNs beyond capacity are dropped in kernel.
#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_sheds_new_syns_when_exhausted() {
    require_root_and_bpf().expect("precheck");
    let veth = VethNs::create().expect("veth+ns");
    let shed = XdpAdmitShed::attach(&veth.iface, config(2, 0, None)).expect("attach");

    for _ in 0..5 {
        veth.syn_probe(8080, "1");
    }

    let pass = shed.pass_total().expect("pass");
    let dropped = shed.shed_total().expect("shed");
    assert_eq!(
        pass, 2,
        "exactly capacity SYNs admitted with refill disabled (pass={pass}, shed={dropped})"
    );
    assert!(
        dropped >= 3,
        "attempts beyond capacity must shed (pass={pass}, shed={dropped})"
    );
    assert_eq!(shed.available().expect("tokens"), 0);
}

// SYNs to unrelated ports never pay the admission toll when a listen port
// is configured.
#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_gates_only_listen_port() {
    require_root_and_bpf().expect("precheck");
    let veth = VethNs::create().expect("veth+ns");
    let shed = XdpAdmitShed::attach(&veth.iface, config(1, 0, Some(4242))).expect("attach");

    for _ in 0..3 {
        veth.syn_probe(9999, "1");
    }
    assert_eq!(shed.pass_total().expect("pass"), 0, "other ports ungated");
    assert_eq!(
        shed.shed_total().expect("shed"),
        0,
        "other ports never shed"
    );
    assert_eq!(shed.available().expect("tokens"), 1);

    veth.syn_probe(4242, "1");
    veth.syn_probe(4242, "1");

    assert_eq!(shed.pass_total().expect("pass"), 1, "gated port admits");
    assert!(
        shed.shed_total().expect("shed") >= 1,
        "gated port sheds past capacity"
    );
}

// In-kernel refill recovers the bucket without any userspace reseed.
#[test]
#[ignore = "needs root + veth; run ./scripts/xdp-veth-smoke.sh"]
fn xdp_admit_shed_refills_tokens_in_kernel() {
    require_root_and_bpf().expect("precheck");
    let veth = VethNs::create().expect("veth+ns");

    // Phase 1 — prove exhaustion without refill (deterministic on CI).
    {
        let shed = XdpAdmitShed::attach(&veth.iface, config(1, 0, None)).expect("attach");
        veth.syn_probe(8080, "0.2");
        assert_eq!(shed.available().expect("tokens"), 0);
        veth.syn_probe(8080, "0.2");
        assert!(
            shed.shed_total().expect("shed") >= 1,
            "exhausted bucket must shed (pass={}, shed={})",
            shed.pass_total().expect("pass"),
            shed.shed_total().expect("shed")
        );
    }

    // Phase 2 — 1 token/s refill re-admits after a wall-clock wait.
    let shed = XdpAdmitShed::attach(&veth.iface, config(1, 1, None)).expect("attach");
    veth.syn_probe(8080, "0.2"); // drain the seeded token
    assert_eq!(shed.available().expect("tokens"), 0);
    std::thread::sleep(Duration::from_millis(2500));
    let pass_before = shed.pass_total().expect("pass");
    veth.syn_probe(8080, "0.2"); // admitted from refilled tokens
    assert!(
        shed.pass_total().expect("pass") > pass_before,
        "refill must re-admit after wait (pass_before={pass_before}, pass={}, shed={})",
        shed.pass_total().expect("pass"),
        shed.shed_total().expect("shed")
    );
}

#[test]
fn xdp_attach_errors_without_object_on_linux() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let missing = std::env::temp_dir().join("demiurge-missing-bpf.o");
    assert!(!missing.is_file());
    std::env::set_var("DEMIURGE_BPF_OBJECT", missing.to_string_lossy().as_ref());
    let err = match XdpAdmitShed::attach("lo", XdpAdmitConfig::default()) {
        Err(e) => e,
        Ok(_) => panic!("expected ObjectNotBuilt"),
    };
    std::env::remove_var("DEMIURGE_BPF_OBJECT");
    assert!(matches!(err, XdpAttachError::ObjectNotBuilt));
}
