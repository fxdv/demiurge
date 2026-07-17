//! KV hand-off artifact between prefill and decode placement. [DEMI-KV-HANDOFF]
//!
//! Phase 2 ships an in-process registry, HTTP header parsing, and pluggable
//! `HandoffTransport` (TCP header pass-through default; mock RDMA for Track A).
//! Completed transfers record byte length and wall time for p50/p99 telemetry.
//! [DEMI-KV-TRANSFER-TELEM] [DEMI-HANDOFF-XPORT]

mod transport;

pub use transport::{
    handoff_transport_from_env, HandoffTransport, HeaderPassthroughTransport, MockRdmaTransport,
    ModeledRdmaTransport, TransferOutcome,
};

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TRANSFER_SAMPLES: usize = 1024;

static NEXT_KV_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Opaque KV cache handle published at prefill completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KvHandle(u64);

impl KvHandle {
    pub fn new() -> Self {
        Self(NEXT_KV_HANDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    pub fn from_raw(raw: u64) -> Option<Self> {
        if raw == 0 {
            None
        } else {
            Some(Self(raw))
        }
    }

    pub fn raw(self) -> u64 {
        self.0
    }
}

impl Default for KvHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Hand-off descriptor: `(request_id, kv_handle, byte_len, source_backend)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffDescriptor {
    pub request_id: u64,
    pub kv_handle: KvHandle,
    pub byte_len: u64,
    pub source_label: String,
    /// Decode pool label (set by router before transfer for topology-aware transports).
    pub decode_label: Option<String>,
}

impl HandoffDescriptor {
    pub fn is_valid(&self) -> bool {
        self.kv_handle.raw() != 0 && self.byte_len > 0
    }
}

/// Parse prefill response **headers** only (stop at the first `\r\n\r\n`).
/// Body lines that look like `x-demiurge-kv-*` are ignored so a compromised
/// or confused backend cannot inject ledger claims via the body.
pub fn parse_prefill_handoff(
    response: &[u8],
    request_id: u64,
    source_label: &str,
) -> Option<HandoffDescriptor> {
    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(response.len());
    let text = String::from_utf8_lossy(&response[..header_end]);
    let mut kv_handle = None;
    let mut byte_len = None;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("x-demiurge-kv-handle:") {
            kv_handle = rest.trim().parse().ok().and_then(KvHandle::from_raw);
        } else if let Some(rest) = lower.strip_prefix("x-demiurge-kv-bytes:") {
            byte_len = rest.trim().parse().ok();
        }
    }
    Some(HandoffDescriptor {
        request_id,
        kv_handle: kv_handle?,
        byte_len: byte_len?,
        source_label: source_label.to_string(),
        decode_label: None,
    })
}

/// Aggregated hand-off transfer cost (p50 / p99 bytes and wall time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HandoffTransferMetrics {
    pub count: u64,
    pub bytes_p50: u64,
    pub bytes_p99: u64,
    pub wall_us_p50: u64,
    pub wall_us_p99: u64,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p) as usize).min(sorted.len() - 1);
    sorted[idx]
}

/// In-process hand-off registry (TCP proof: prefill publishes, decode consumes).
#[derive(Debug, Default)]
pub struct HandoffRegistry {
    inner: Mutex<HashMap<u64, HandoffDescriptor>>,
    transfer_bytes: Mutex<Vec<u64>>,
    transfer_wall_us: Mutex<Vec<u64>>,
}

impl HandoffRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn publish(&self, desc: HandoffDescriptor) {
        if !desc.is_valid() {
            return;
        }
        self.inner
            .lock()
            .expect("handoff lock")
            .insert(desc.request_id, desc);
    }

    pub fn get(&self, request_id: u64) -> Option<HandoffDescriptor> {
        self.inner
            .lock()
            .expect("handoff lock")
            .get(&request_id)
            .cloned()
    }

    /// Take and remove the hand-off (decode path consumed the artifact).
    pub fn take(&self, request_id: u64) -> Option<HandoffDescriptor> {
        self.inner.lock().expect("handoff lock").remove(&request_id)
    }

    /// Record a completed KV hand-off transfer. [DEMI-KV-TRANSFER-TELEM]
    pub fn record_transfer(&self, bytes: u64, wall: Duration) {
        if bytes == 0 {
            return;
        }
        let wall_us = wall.as_micros().min(u64::MAX as u128) as u64;
        let mut sizes = self.transfer_bytes.lock().expect("transfer bytes");
        if sizes.len() >= TRANSFER_SAMPLES {
            sizes.remove(0);
        }
        sizes.push(bytes);
        let mut walls = self.transfer_wall_us.lock().expect("transfer wall");
        if walls.len() >= TRANSFER_SAMPLES {
            walls.remove(0);
        }
        walls.push(wall_us);
    }

    /// p50 / p99 bytes and wall time over recorded transfers.
    pub fn transfer_metrics(&self) -> HandoffTransferMetrics {
        let mut bytes = self.transfer_bytes.lock().expect("transfer bytes").clone();
        let mut walls = self.transfer_wall_us.lock().expect("transfer wall").clone();
        if bytes.is_empty() {
            return HandoffTransferMetrics::default();
        }
        bytes.sort_unstable();
        walls.sort_unstable();
        HandoffTransferMetrics {
            count: bytes.len() as u64,
            bytes_p50: percentile(&bytes, 0.50),
            bytes_p99: percentile(&bytes, 0.99),
            wall_us_p50: percentile(&walls, 0.50),
            wall_us_p99: percentile(&walls, 0.99),
        }
    }

    /// Log transfer cost summary to stderr (load bench / ops).
    pub fn log_transfer_cost(&self, label: &str) {
        let m = self.transfer_metrics();
        if m.count == 0 {
            return;
        }
        eprintln!(
            "handoff-transfer: {label} n={} bytes p50/p99={}/{} wall_us p50/p99={}/{}",
            m.count, m.bytes_p50, m.bytes_p99, m.wall_us_p50, m.wall_us_p99
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_waits_for_handoff() {
        let reg = HandoffRegistry::new();
        assert!(reg.get(1).is_none());
        reg.publish(HandoffDescriptor {
            request_id: 1,
            kv_handle: KvHandle::new(),
            byte_len: 4096,
            source_label: "pf0".into(),
            decode_label: None,
        });
        let h = reg.take(1).expect("handoff");
        assert!(h.is_valid());
        assert!(reg.get(1).is_none());
    }

    #[test]
    fn handoff_transfer_telemetry_p50_p99() {
        let reg = HandoffRegistry::new();
        for (bytes, us) in [(100_u64, 10_u64), (200, 20), (300, 30), (400, 40)] {
            reg.record_transfer(bytes, Duration::from_micros(us));
        }
        let m = reg.transfer_metrics();
        assert_eq!(m.count, 4);
        assert_eq!(m.bytes_p50, 300);
        assert_eq!(m.bytes_p99, 400);
        assert_eq!(m.wall_us_p50, 30);
        assert_eq!(m.wall_us_p99, 40);
    }

    #[test]
    fn parse_handoff_headers() {
        let head =
            b"HTTP/1.1 200 OK\r\nx-demiurge-kv-handle: 42\r\nx-demiurge-kv-bytes: 8192\r\n\r\n";
        let d = parse_prefill_handoff(head, 7, "pf-a").expect("parsed");
        assert_eq!(d.request_id, 7);
        assert_eq!(d.kv_handle.raw(), 42);
        assert_eq!(d.byte_len, 8192);
    }

    #[test]
    fn parse_handoff_ignores_body_injected_headers() {
        let response = b"HTTP/1.1 200 OK\r\n\
x-demiurge-kv-handle: 42\r\n\
x-demiurge-kv-bytes: 8192\r\n\
\r\n\
x-demiurge-kv-handle: 99\r\n\
x-demiurge-kv-bytes: 999999999\r\n";
        let d = parse_prefill_handoff(response, 7, "pf-a").expect("parsed");
        assert_eq!(d.kv_handle.raw(), 42);
        assert_eq!(d.byte_len, 8192);
    }
}
