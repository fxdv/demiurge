//! RCU-published fleet state snapshot.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use demiurge_auth::{GroupId, PrefixFingerprint, SharedPrefixGroupRegistry, TenantId};

use crate::warmth::{gated_hit_strength, WarmthMap};

/// Cache-domain isolation context for a warmth lookup: the Shared-Prefix
/// Group authority plus the already-authenticated requester identity.
/// [DEMI-S1-DOMAIN]
pub type IsolationCtx<'a> = (
    &'a SharedPrefixGroupRegistry,
    TenantId,
    GroupId,
    PrefixFingerprint,
);

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

    /// Warmth hit strength for `label` in the prefill or decode pool,
    /// optionally gated by cache-domain isolation; a backend absent from the
    /// snapshot reads as fully cold (`0.0`). This is the single warmth-lookup
    /// path shared by live routing and shadow pairing. [DEMI-WARM-DISCOUNT]
    /// [DEMI-S1-DOMAIN]
    #[must_use]
    pub fn pool_hit_strength(
        &self,
        decode_pool: bool,
        label: &str,
        blocks: &[u64],
        isolation: Option<IsolationCtx<'_>>,
    ) -> f64 {
        let pool = if decode_pool {
            &self.decode
        } else {
            &self.prefill
        };
        let Some(bs) = pool.get(label) else {
            return 0.0;
        };
        match isolation {
            Some((registry, tenant, group, content_fp)) => {
                gated_hit_strength(&bs.warmth, registry, tenant, group, content_fp, blocks)
            }
            None => bs.warmth.hit_strength(blocks),
        }
    }
}

/// Eventually-consistent state plane with gossip merge. [DEMI-STATE-AP]
#[derive(Debug)]
pub struct StatePlane {
    inner: ArcSwap<StateSnapshot>,
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
            inner: ArcSwap::from_pointee(StateSnapshot {
                generation: 0,
                prefill,
                decode,
            }),
        })
    }

    /// RCU read — lock-free `Arc` clone for routing.
    pub fn snapshot(&self) -> Arc<StateSnapshot> {
        self.inner.load_full()
    }

    pub fn publish_snapshot(&self, snap: StateSnapshot) {
        self.inner.store(Arc::new(snap));
    }

    /// Copy-on-write update for publishers (warmth, gossip).
    pub fn update_snapshot<F>(&self, f: F)
    where
        F: FnOnce(&mut StateSnapshot),
    {
        let mut snap = (*self.inner.load_full()).clone();
        f(&mut snap);
        self.inner.store(Arc::new(snap));
    }

    pub fn bump_generation(&self) -> u64 {
        let mut generation = 0;
        self.update_snapshot(|snap| {
            snap.generation += 1;
            generation = snap.generation;
        });
        generation
    }
}
