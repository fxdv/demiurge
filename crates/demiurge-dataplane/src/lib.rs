//! Data-plane primitives: RCU routing table and admission shed.
//!
//! **Phase 5:** Proof path in userspace; production XDP/io_uring wiring follows.

mod admission;
mod admit_mode;
mod forwarder;
mod rcu;
mod xdp;

pub use admission::{AdmitBucket, ShedReason};
pub use admit_mode::AdmitMode;
#[cfg(target_os = "linux")]
pub use forwarder::IoUringProxySession;
pub use forwarder::{ForwardDecision, IoUringForwardBench, IoUringForwarder};
pub use rcu::{pool_core_scale, DataPlaneSnapshot, RcuRoutingTable};
pub use xdp::{XdpAdmitShed, XdpAttachError, DEFAULT_CAPACITY as XDP_DEFAULT_CAPACITY};

/// Scale base admit burst by dataplane π for control-plane actuation sync.
pub fn admit_capacity_for_pi(base_burst: u64, pi: f64) -> u64 {
    let scaled = (base_burst as f64 * pi.clamp(0.1, 1.0)).round();
    (scaled as u64).max(1)
}
