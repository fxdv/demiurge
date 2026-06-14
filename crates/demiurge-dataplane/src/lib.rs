//! Data-plane primitives: RCU routing table and admission shed.
//!
//! **Phase 5:** Proof path in userspace; production XDP/io_uring wiring follows.

mod admission;
mod forwarder;
mod rcu;
mod xdp;

pub use admission::{AdmitBucket, ShedReason};
pub use forwarder::{ForwardDecision, IoUringForwarder};
pub use rcu::{pool_core_scale, DataPlaneSnapshot, RcuRoutingTable};
pub use xdp::{XdpAdmitShed, XdpAttachError, DEFAULT_CAPACITY as XDP_DEFAULT_CAPACITY};
