//! io_uring L7 forwarder — hot path reads RCU; Linux wires recv/send. [DEMI-DP-RCU]

use std::sync::Arc;

use crate::RcuRoutingTable;

/// Routing decision materialized from the last RCU snapshot (no control-plane I/O).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForwardDecision {
    pub pi: f64,
    pub generation: u64,
}

/// Userspace proof of the data-plane read path; production wires io_uring recv/send on Linux.
#[derive(Debug, Clone)]
pub struct IoUringForwarder {
    rcu: Arc<RcuRoutingTable>,
}

impl IoUringForwarder {
    pub fn new(rcu: Arc<RcuRoutingTable>) -> Self {
        Self { rcu }
    }

    pub fn from_router_dataplane(dataplane: &Arc<RcuRoutingTable>) -> Self {
        Self::new(Arc::clone(dataplane))
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

#[cfg(target_os = "linux")]
#[path = "forwarder_io_uring.rs"]
mod io_uring_impl;

#[cfg(target_os = "linux")]
pub use io_uring_impl::{IoUringAcceptLoop, IoUringProxySession};

#[cfg(target_os = "linux")]
impl IoUringForwarder {
    pub fn io_uring_enabled_from_env() -> bool {
        std::env::var("DEMIURGE_IOURING")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    }

    /// Production proxy session: reused ring for recv/send on one TCP connection.
    pub fn open_proxy_session(&self) -> std::io::Result<io_uring_impl::IoUringProxySession> {
        let _ = self.forward_decision();
        io_uring_impl::IoUringProxySession::new()
    }

    /// Copy bytes between two stream fds via io_uring read/write (one-shot session).
    pub fn copy_between(
        &self,
        read_fd: std::os::fd::RawFd,
        write_fd: std::os::fd::RawFd,
        max_bytes: usize,
    ) -> std::io::Result<u64> {
        io_uring_impl::copy_between(read_fd, write_fd, max_bytes)
    }

    /// Bench hot path: RCU read + one io_uring NOP submit/wait on a reused ring.
    pub fn bench_forward_nop(&self, ring: &mut io_uring::IoUring) -> std::io::Result<()> {
        io_uring_impl::bench_forward_nop(self, ring)
    }
}

/// Reusable ring for `BENCH-IOURING-FWD` (Linux); RCU-only stub elsewhere.
pub struct IoUringForwardBench {
    fwd: IoUringForwarder,
    #[cfg(target_os = "linux")]
    ring: io_uring::IoUring,
}

impl IoUringForwardBench {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            fwd: IoUringForwarder::new(RcuRoutingTable::new(0.5)),
            #[cfg(target_os = "linux")]
            ring: io_uring::IoUring::new(2)?,
        })
    }

    pub fn run(&mut self) -> std::io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.fwd.bench_forward_nop(&mut self.ring)
        }
        #[cfg(not(target_os = "linux"))]
        {
            std::hint::black_box(self.fwd.forward_decision());
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl IoUringForwarder {
    pub fn io_uring_enabled_from_env() -> bool {
        false
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
