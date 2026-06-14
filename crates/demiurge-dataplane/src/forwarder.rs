//! io_uring L7 forwarder skeleton — hot path reads RCU only. [DEMI-DP-RCU]

use std::sync::Arc;

use crate::RcuRoutingTable;

/// Routing decision materialized from the last RCU snapshot (no control-plane I/O).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForwardDecision {
    pub pi: f64,
    pub generation: u64,
}

/// Userspace proof of the data-plane read path; production wires io_uring recv/send here.
#[derive(Debug, Clone)]
pub struct IoUringForwarder {
    rcu: Arc<RcuRoutingTable>,
}

impl IoUringForwarder {
    pub fn new(rcu: Arc<RcuRoutingTable>) -> Self {
        Self { rcu }
    }

    pub fn rcu(&self) -> &Arc<RcuRoutingTable> {
        &self.rcu
    }

    /// Hot path: lock-free RCU read only.
    pub fn forward_decision(&self) -> ForwardDecision {
        let snap = self.rcu.read();
        ForwardDecision {
            pi: snap.pi,
            generation: snap.generation,
        }
    }

    pub fn current_pi(&self) -> f64 {
        self.rcu.read_pi()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarder_reads_rcu_without_blocking() {
        let rcu = RcuRoutingTable::new(0.42);
        let fwd = IoUringForwarder::new(rcu);
        let d = fwd.forward_decision();
        assert!((d.pi - 0.42).abs() < f64::EPSILON);
    }
}
