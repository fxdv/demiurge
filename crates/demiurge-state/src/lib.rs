//! Eventually-consistent state plane for Demiurge routing.
//!
//! - [DEMI-WARM-DISCOUNT] — warmth hits feed bounded routing discounts.
//! - [DEMI-STATE-AP] — AP gossip; stale warmth → miss only, never crash.

mod gossip;
mod snapshot;
mod warmth;

pub use gossip::{stale_probe, GossipUpdate};
pub use snapshot::{BackendSnapshot, StatePlane, StateSnapshot};
pub use warmth::{
    default_routing_blocks, gated_hit_strength, routing_blocks, salted_blocks, WarmthMap,
};
