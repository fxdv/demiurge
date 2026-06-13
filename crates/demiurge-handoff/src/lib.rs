//! KV hand-off artifact between prefill and decode placement. [DEMI-KV-HANDOFF]
//!
//! Phase 2 ships an in-process registry plus HTTP header parsing; TCP blob
//! transport remains pluggable for later RDMA work.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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
}

impl HandoffDescriptor {
    pub fn is_valid(&self) -> bool {
        self.kv_handle.raw() != 0 && self.byte_len > 0
    }
}

/// Parse prefill response headers from an HTTP head or full response prefix.
pub fn parse_prefill_handoff(
    head: &[u8],
    request_id: u64,
    source_label: &str,
) -> Option<HandoffDescriptor> {
    let text = String::from_utf8_lossy(head);
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
    })
}

/// In-process hand-off registry (TCP proof: prefill publishes, decode consumes).
#[derive(Debug, Default)]
pub struct HandoffRegistry {
    inner: Mutex<HashMap<u64, HandoffDescriptor>>,
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
        });
        let h = reg.take(1).expect("handoff");
        assert!(h.is_valid());
        assert!(reg.get(1).is_none());
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
}
