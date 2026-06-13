//! Scored backend for control-plane pairing (mirrors router cost without I/O).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use demiurge_cost::{compose, BarrierFactor, Corrector, Cost, Discount, TimeCore};

#[derive(Debug)]
pub struct ScoredBackend {
    pub label: String,
    pub addr: SocketAddr,
    base_service_seconds: f64,
    inflight: AtomicUsize,
}

impl ScoredBackend {
    pub fn new(label: impl Into<String>, addr: SocketAddr, base_service_seconds: f64) -> Arc<Self> {
        Arc::new(Self {
            label: label.into(),
            addr,
            base_service_seconds,
            inflight: AtomicUsize::new(0),
        })
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    pub fn base_cost(&self, extra_barriers: &[BarrierFactor], discounts: &[Discount]) -> Cost {
        let core = TimeCore::clamped(self.base_service_seconds);
        let queue = BarrierFactor::clamped(1.0 + self.inflight() as f64);
        let mut barriers = Vec::with_capacity(1 + extra_barriers.len());
        barriers.push(queue);
        barriers.extend_from_slice(extra_barriers);
        compose(core, &barriers, discounts, Corrector::identity())
    }
}
