//! RCU-published fleet state snapshot.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::warmth::WarmthMap;

#[derive(Debug, Clone)]
pub struct BackendSnapshot {
    pub label: String,
    pub warmth: WarmthMap,
    /// Normalized queue pressure in `[0, 1]`.
    pub occupancy: f64,
    pub kv_bytes_live: u64,
    pub kv_capacity_bytes: u64,
}

impl BackendSnapshot {
    pub fn new(label: impl Into<String>, kv_capacity_bytes: u64) -> Self {
        Self {
            label: label.into(),
            warmth: WarmthMap::default(),
            occupancy: 0.0,
            kv_bytes_live: 0,
            kv_capacity_bytes,
        }
    }

    pub fn kv_pressure(&self) -> f64 {
        if self.kv_capacity_bytes == 0 {
            return 0.0;
        }
        (self.kv_bytes_live as f64 / self.kv_capacity_bytes as f64).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub generation: u64,
    pub prefill: HashMap<String, BackendSnapshot>,
    pub decode: HashMap<String, BackendSnapshot>,
}

impl StateSnapshot {
    pub fn empty() -> Self {
        Self {
            generation: 0,
            prefill: HashMap::new(),
            decode: HashMap::new(),
        }
    }
}

/// Eventually-consistent state plane with gossip merge. [DEMI-STATE-AP]
#[derive(Debug)]
pub struct StatePlane {
    inner: Mutex<StateSnapshot>,
}

impl StatePlane {
    pub fn new(prefill_labels: &[String], decode_labels: &[String]) -> Arc<Self> {
        let mut prefill = HashMap::new();
        for label in prefill_labels {
            prefill.insert(label.clone(), BackendSnapshot::new(label, 0));
        }
        let mut decode = HashMap::new();
        for label in decode_labels {
            decode.insert(label.clone(), BackendSnapshot::new(label, 120_000_000));
        }
        Arc::new(Self {
            inner: Mutex::new(StateSnapshot {
                generation: 0,
                prefill,
                decode,
            }),
        })
    }

    /// RCU read — clone current snapshot for routing.
    pub fn snapshot(&self) -> StateSnapshot {
        self.inner.lock().expect("state plane").clone()
    }

    pub fn publish_snapshot(&self, snap: StateSnapshot) {
        *self.inner.lock().expect("state plane") = snap;
    }

    pub fn bump_generation(&self) -> u64 {
        let mut inner = self.inner.lock().expect("state plane");
        inner.generation += 1;
        inner.generation
    }
}
