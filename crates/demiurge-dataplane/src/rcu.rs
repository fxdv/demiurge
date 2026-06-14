//! RCU-published routing snapshot for the data plane. [DEMI-DP-RCU]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;

/// Immutable routing table generation served by the data plane.
#[derive(Debug, Clone)]
pub struct DataPlaneSnapshot {
    pub generation: u64,
    /// Prefill pool capacity share π ∈ [0, 1].
    pub pi: f64,
    pub published_at_ms: u64,
}

impl DataPlaneSnapshot {
    pub fn new(generation: u64, pi: f64) -> Self {
        Self {
            generation,
            pi: pi.clamp(0.0, 1.0),
            published_at_ms: now_ms(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lock-free RCU slot: readers clone an `Arc` without blocking publishers.
#[derive(Debug)]
pub struct RcuRoutingTable {
    current: ArcSwap<DataPlaneSnapshot>,
}

impl RcuRoutingTable {
    pub fn new(initial_pi: f64) -> Arc<Self> {
        Arc::new(Self {
            current: ArcSwap::from_pointee(DataPlaneSnapshot::new(0, initial_pi)),
        })
    }

    /// Hot-path read — never waits on control-plane publish.
    pub fn read(&self) -> Arc<DataPlaneSnapshot> {
        self.current.load_full()
    }

    pub fn read_pi(&self) -> f64 {
        self.current.load().pi
    }

    pub fn generation(&self) -> u64 {
        self.current.load().generation
    }

    pub fn publish(&self, snap: DataPlaneSnapshot) {
        self.current.store(Arc::new(snap));
    }

    pub fn publish_pi(&self, generation: u64, pi: f64) {
        self.publish(DataPlaneSnapshot::new(generation, pi));
    }

    pub fn age_ms(&self) -> u64 {
        let snap = self.current.load();
        now_ms().saturating_sub(snap.published_at_ms)
    }
}
