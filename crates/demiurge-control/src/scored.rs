//! Scored backend for control-plane pairing (mirrors router cost without I/O).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use demiurge_cost::{service_cost, BarrierFactor, Cost, Discount};

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
        service_cost(
            self.base_service_seconds,
            self.inflight(),
            extra_barriers,
            discounts,
        )
    }
}
