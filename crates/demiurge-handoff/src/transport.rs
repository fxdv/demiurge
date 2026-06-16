//! Pluggable KV hand-off transport (TCP proof default; mock RDMA for Track A).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use demiurge_cost::{rdma_distance, rdma_transfer_seconds, TopologyId};

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

/// Bandwidth + topology-aware mock RDMA (shadow ground truth for transfer cost model).
#[derive(Debug, Clone)]
pub struct ModeledRdmaTransport {
    topology: HashMap<String, TopologyId>,
}

impl ModeledRdmaTransport {
    pub fn new(topology: HashMap<String, TopologyId>) -> Self {
        Self { topology }
    }
}

impl HandoffTransport for ModeledRdmaTransport {
    fn transfer(&self, desc: &HandoffDescriptor, _prefill_wall: Duration) -> TransferOutcome {
        let pf = self
            .topology
            .get(&desc.source_label)
            .cloned()
            .unwrap_or_default();
        let dc_label = desc
            .decode_label
            .as_deref()
            .unwrap_or(desc.source_label.as_str());
        let dc = self.topology.get(dc_label).cloned().unwrap_or_default();
        let seconds = rdma_transfer_seconds(desc.byte_len, rdma_distance(&pf, &dc));
        TransferOutcome {
            bytes: desc.byte_len,
            wall: Duration::from_secs_f64(seconds),
        }
    }
}

/// Select handoff transport from `DEMIURGE_HANDOFF_TRANSPORT` (router binary startup).
pub fn handoff_transport_from_env(
    topology: HashMap<String, TopologyId>,
) -> Arc<dyn HandoffTransport> {
    match std::env::var("DEMIURGE_HANDOFF_TRANSPORT")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "mock_rdma" => Arc::new(MockRdmaTransport::default()),
        "modeled_rdma" => Arc::new(ModeledRdmaTransport::new(topology)),
        _ => Arc::new(HeaderPassthroughTransport),
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
            decode_label: None,
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

    #[test]
    fn modeled_rdma_transport_matches_analytic() {
        let mut topo = HashMap::new();
        topo.insert("pf0".into(), TopologyId::new("n0", "r0", "cA"));
        topo.insert("dc1".into(), TopologyId::new("n1", "r0", "cA"));
        let t = ModeledRdmaTransport::new(topo);
        let mut desc = sample_desc();
        desc.source_label = "pf0".into();
        desc.decode_label = Some("dc1".into());
        desc.byte_len = 1_048_576;
        let out = t.transfer(&desc, Duration::ZERO);
        let distance = rdma_distance(
            &TopologyId::new("n0", "r0", "cA"),
            &TopologyId::new("n1", "r0", "cA"),
        );
        let expected = rdma_transfer_seconds(desc.byte_len, distance);
        assert!((out.wall.as_secs_f64() - expected).abs() < 1e-9);
    }
}
