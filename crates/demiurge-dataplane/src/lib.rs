//! Data-plane primitives: RCU routing table and admission shed.
//!
//! **Phase 5:** Proof path in userspace; production XDP/io_uring wiring follows.

mod admission;
mod rcu;

pub use admission::{AdmitBucket, ShedReason};
pub use rcu::{DataPlaneSnapshot, RcuRoutingTable};
