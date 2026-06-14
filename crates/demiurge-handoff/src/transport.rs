//! Pluggable KV hand-off transport (TCP proof default; mock RDMA for Track A).

use std::time::Duration;

use crate::HandoffDescriptor;

/// Result of a completed KV blob transfer. [DEMI-HANDOFF-XPORT]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferOutcome {
    pub bytes: u64,
    pub wall: Duration,
}

/// Moves KV payload bytes from prefill to decode pool. Production RDMA lands in Track C.
pub trait HandoffTransport: Send + Sync {
    fn transfer(&self, desc: &HandoffDescriptor, prefill_wall: Duration) -> TransferOutcome;
}

/// Existing Phase 2 proof: transfer cost equals HTTP header metadata + prefill TCP wall.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeaderPassthroughTransport;

impl HandoffTransport for HeaderPassthroughTransport {
    fn transfer(&self, desc: &HandoffDescriptor, prefill_wall: Duration) -> TransferOutcome {
        TransferOutcome {
            bytes: desc.byte_len,
            wall: prefill_wall,
        }
    }
}

/// In-process mock RDMA: same bytes, fixed microsecond latency (Mac / unit tests).
#[derive(Debug, Clone, Copy)]
pub struct MockRdmaTransport {
    pub latency_us: u64,
}

impl MockRdmaTransport {
    pub const DEFAULT_LATENCY_US: u64 = 50;

    pub fn new(latency_us: u64) -> Self {
        Self { latency_us }
    }
}

impl Default for MockRdmaTransport {
    fn default() -> Self {
        Self::new(Self::DEFAULT_LATENCY_US)
    }
}

impl HandoffTransport for MockRdmaTransport {
    fn transfer(&self, desc: &HandoffDescriptor, _prefill_wall: Duration) -> TransferOutcome {
        TransferOutcome {
            bytes: desc.byte_len,
            wall: Duration::from_micros(self.latency_us),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KvHandle;

    fn sample_desc() -> HandoffDescriptor {
        HandoffDescriptor {
            request_id: 1,
            kv_handle: KvHandle::new(),
            byte_len: 8192,
            source_label: "pf0".into(),
        }
    }

    #[test]
    fn header_passthrough_uses_prefill_wall() {
        let t = HeaderPassthroughTransport;
        let wall = Duration::from_millis(12);
        let out = t.transfer(&sample_desc(), wall);
        assert_eq!(out.bytes, 8192);
        assert_eq!(out.wall, wall);
    }

    #[test]
    fn mock_rdma_fixed_latency() {
        let t = MockRdmaTransport::new(40);
        let out = t.transfer(&sample_desc(), Duration::from_millis(99));
        assert_eq!(out.bytes, 8192);
        assert_eq!(out.wall, Duration::from_micros(40));
    }
}
