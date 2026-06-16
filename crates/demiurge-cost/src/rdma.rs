//! RDMA topology distance + analytic transfer time (shadow cost model). [DEMI-RDMA-COST-SHADOW]

use crate::{generated_params::*, BarrierFactor, TimeCore};

/// Placement label for RDMA distance (node / rack / cluster).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct TopologyId {
    pub node: String,
    pub rack: String,
    pub cluster: String,
}

impl TopologyId {
    pub fn new(
        node: impl Into<String>,
        rack: impl Into<String>,
        cluster: impl Into<String>,
    ) -> Self {
        Self {
            node: node.into(),
            rack: rack.into(),
            cluster: cluster.into(),
        }
    }
}

/// Discrete RDMA distance ladder: same node → cross cluster.
pub fn rdma_distance(a: &TopologyId, b: &TopologyId) -> u64 {
    if a.node == b.node {
        return TOPOLOGY_RDMA_SAME_NODE;
    }
    if a.rack == b.rack && a.cluster == b.cluster {
        return TOPOLOGY_RDMA_SAME_RACK;
    }
    if a.cluster == b.cluster {
        return TOPOLOGY_RDMA_SAME_CLUSTER;
    }
    TOPOLOGY_RDMA_CROSS_CLUSTER
}

fn base_latency_us(distance: u64) -> u64 {
    match distance {
        TOPOLOGY_RDMA_SAME_NODE => RDMA_BASE_LATENCY_SAME_NODE_US,
        TOPOLOGY_RDMA_SAME_RACK => RDMA_BASE_LATENCY_SAME_RACK_US,
        TOPOLOGY_RDMA_SAME_CLUSTER => RDMA_BASE_LATENCY_SAME_CLUSTER_US,
        _ => RDMA_BASE_LATENCY_CROSS_CLUSTER_US,
    }
}

/// Predicted KV transfer wall time in seconds (base latency + bytes / link bandwidth).
pub fn rdma_transfer_seconds(bytes: u64, distance: u64) -> f64 {
    let base = base_latency_us(distance) as f64 / 1_000_000.0;
    let bw_bytes_per_s = RDMA_LINK_GBPS * 1e9 / 8.0;
    base + bytes as f64 / bw_bytes_per_s
}

/// Log-space analytic transfer cost for shadow logging.
pub fn rdma_transfer_ln(bytes: u64, pf: &TopologyId, dc: &TopologyId) -> f64 {
    let distance = rdma_distance(pf, dc);
    TimeCore::clamped(rdma_transfer_seconds(bytes, distance))
        .get()
        .ln()
}

/// Multiplicative barrier from predicted transfer seconds (shadow-only; not wired to routing yet).
pub fn rdma_transfer_barrier(bytes: u64, pf: &TopologyId, dc: &TopologyId) -> BarrierFactor {
    let distance = rdma_distance(pf, dc);
    let seconds = rdma_transfer_seconds(bytes, distance);
    let nominal = RDMA_BASE_LATENCY_SAME_NODE_US as f64 / 1_000_000.0;
    let ratio = (seconds / nominal).max(RDMA_MIN_BARRIER);
    BarrierFactor::clamped(ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topo(node: &str, rack: &str, cluster: &str) -> TopologyId {
        TopologyId::new(node, rack, cluster)
    }

    #[test]
    fn rdma_distance_same_node_is_zero() {
        let a = topo("n0", "r0", "cA");
        assert_eq!(rdma_distance(&a, &a), TOPOLOGY_RDMA_SAME_NODE);
    }

    #[test]
    fn rdma_distance_ladder_ordered() {
        let n0 = topo("n0", "r0", "cA");
        let n1 = topo("n1", "r0", "cA");
        let n2 = topo("n2", "r1", "cA");
        let n3 = topo("n3", "r9", "cB");
        assert!(rdma_distance(&n0, &n1) < rdma_distance(&n0, &n2));
        assert!(rdma_distance(&n0, &n2) < rdma_distance(&n0, &n3));
    }

    #[test]
    fn rdma_transfer_monotonic_in_bytes() {
        let pf = topo("n0", "r0", "cA");
        let dc = topo("n1", "r0", "cA");
        let d = rdma_distance(&pf, &dc);
        let small = rdma_transfer_seconds(4096, d);
        let large = rdma_transfer_seconds(4 * 4096, d);
        assert!(large > small);
    }
}
