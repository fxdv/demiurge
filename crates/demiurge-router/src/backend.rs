//! Backend cost surface and min-cost selection. [DEMI-ROUTE-MINCOST]
//!
//! A [`Backend`] carries the live load signal (inflight queue) and composes
//! its routing cost from the shared [`demiurge_cost::service_cost`] surface —
//! the same algebra the control plane's shadow pairing scores against, so the
//! two can never drift apart.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use demiurge_control::PairingTarget;
use demiurge_cost::{
    append_barriers_fail_expensive, rdma_transfer_ln, service_cost, warmth_discount, BarrierFactor,
    Cost, Discount, TopologyId, MAX_SERVICE_BARRIERS, ROUTING_OBSERVED_LATENCY_EWMA_ALPHA,
    ROUTING_TRANSFER_PENALTY,
};
use demiurge_dataplane::pool_core_scale;
use demiurge_state::{default_routing_blocks, StateSnapshot};

use crate::routing::RequestIdentity;
use crate::SharedPrefixGroupRegistry;

/// A backend instance plus its live load signal.
#[derive(Debug)]
pub struct Backend {
    pub label: String,
    pub addr: SocketAddr,
    topology: TopologyId,
    base_service_seconds: f64,
    /// ln(base_service_seconds) — valid bases only; invalid inputs fail-expensive at construction.
    ln_base: f64,
    /// EWMA of observed prefill wall (`f64` bits; 0 = unset). T6.
    observed_ewma_bits: AtomicU64,
    inflight: AtomicUsize,
}

#[inline]
fn queue_ln(inflight: usize) -> f64 {
    (1.0 + inflight as f64).ln()
}

/// Warmth discount for `label` under the optional isolation context.
/// [DEMI-WARM-DISCOUNT] [DEMI-S1-DOMAIN]
fn warmth_discount_for(
    snapshot: Option<&StateSnapshot>,
    decode_pool: bool,
    label: &str,
    blocks: &[u64],
    isolation: Option<(&SharedPrefixGroupRegistry, &RequestIdentity)>,
) -> Option<Discount> {
    let snap = snapshot?;
    let strength = snap.pool_hit_strength(
        decode_pool,
        label,
        blocks,
        isolation.map(|(reg, id)| (reg, id.tenant, id.group, id.content_fp)),
    );
    warmth_discount(strength)
}

impl Backend {
    pub fn new(label: impl Into<String>, addr: SocketAddr, base_service_seconds: f64) -> Arc<Self> {
        Self::new_with_topology(label, addr, base_service_seconds, TopologyId::default())
    }

    pub fn new_with_topology(
        label: impl Into<String>,
        addr: SocketAddr,
        base_service_seconds: f64,
        topology: TopologyId,
    ) -> Arc<Self> {
        let ln_base = if base_service_seconds.is_finite() && base_service_seconds > 0.0 {
            base_service_seconds.ln()
        } else {
            f64::MAX.ln()
        };
        Arc::new(Self {
            label: label.into(),
            addr,
            topology,
            base_service_seconds,
            ln_base,
            observed_ewma_bits: AtomicU64::new(0),
            inflight: AtomicUsize::new(0),
        })
    }

    #[must_use]
    pub fn topology(&self) -> &TopologyId {
        &self.topology
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    pub fn incr_inflight(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    pub fn decr_inflight(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// Fold an observed service wall into the EWMA used for effective T_core.
    /// Non-finite / non-positive samples are ignored. [T6]
    pub fn note_observed_seconds(&self, secs: f64) {
        if !secs.is_finite() || secs <= 0.0 {
            return;
        }
        let alpha = ROUTING_OBSERVED_LATENCY_EWMA_ALPHA.clamp(0.0, 1.0);
        loop {
            let prev_bits = self.observed_ewma_bits.load(Ordering::Relaxed);
            let next = if prev_bits == 0 {
                secs
            } else {
                let prev = f64::from_bits(prev_bits);
                alpha * secs + (1.0 - alpha) * prev
            };
            if self
                .observed_ewma_bits
                .compare_exchange_weak(
                    prev_bits,
                    next.to_bits(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                break;
            }
        }
    }

    /// `max(claimed, observed_ewma)` — under-reported claim cannot undercut
    /// measured prefill wall. [T6]
    #[inline]
    pub fn effective_base_seconds(&self) -> f64 {
        let claimed = if self.base_service_seconds.is_finite() && self.base_service_seconds > 0.0 {
            self.base_service_seconds
        } else {
            f64::MAX
        };
        let bits = self.observed_ewma_bits.load(Ordering::Relaxed);
        if bits == 0 {
            return claimed;
        }
        let observed = f64::from_bits(bits);
        if observed.is_finite() && observed > claimed {
            observed
        } else {
            claimed
        }
    }

    #[inline]
    fn effective_ln_base(&self) -> f64 {
        let effective = self.effective_base_seconds();
        if (effective - self.base_service_seconds).abs() < f64::EPSILON {
            self.ln_base
        } else if effective.is_finite() && effective > 0.0 {
            effective.ln()
        } else {
            f64::MAX.ln()
        }
    }

    pub fn cost(&self) -> Cost {
        Cost::from_ln(self.effective_ln_base() + queue_ln(self.inflight()))
    }

    #[inline]
    pub(crate) fn selection_ln(&self, pool_pi: f64, prefill: bool) -> f64 {
        self.effective_ln_base()
            + pool_core_scale(1.0, pool_pi, prefill).ln()
            + queue_ln(self.inflight())
    }

    #[inline]
    pub(crate) fn ln_base(&self) -> f64 {
        self.effective_ln_base()
    }

    pub fn cost_with_warmth(
        &self,
        extra: &[BarrierFactor],
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        decode_pool: bool,
    ) -> Cost {
        self.cost_with_warmth_pi(extra, snapshot, blocks, decode_pool, 0.5)
    }

    pub fn cost_with_warmth_pi(
        &self,
        extra: &[BarrierFactor],
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        decode_pool: bool,
        pool_pi: f64,
    ) -> Cost {
        self.cost_with_isolation_pi(extra, snapshot, blocks, decode_pool, pool_pi, None)
    }

    /// `cost_with_warmth_pi`, but the warmth discount is gated by cache-domain
    /// isolation. [DEMI-S1-DOMAIN]
    ///
    /// `isolation` is `Some((registry, identity))` for an already-authenticated
    /// request; the discount is then measured under the cache-domain key the
    /// registry resolves for `identity` — a non-member or template mismatch
    /// falls back to a tenant-private key and can only ever discount against
    /// its own warmth, never another tenant's shared cache. `None` is
    /// identical to [`Self::cost_with_warmth_pi`].
    pub fn cost_with_isolation_pi(
        &self,
        extra: &[BarrierFactor],
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        decode_pool: bool,
        pool_pi: f64,
        isolation: Option<(&SharedPrefixGroupRegistry, &RequestIdentity)>,
    ) -> Cost {
        let discount = warmth_discount_for(snapshot, decode_pool, &self.label, blocks, isolation);
        if extra.is_empty() && discount.is_none() {
            return Cost::from_ln(self.selection_ln(pool_pi, !decode_pool));
        }
        let scaled = pool_core_scale(self.effective_base_seconds(), pool_pi, !decode_pool);
        let ds = discount.map(|d| [d]);
        let discounts: &[Discount] = ds.as_ref().map_or(&[], |a| a.as_slice());
        service_cost(scaled, self.inflight(), extra, discounts)
    }

    pub fn cost_with_barriers(&self, extra: &[BarrierFactor]) -> Cost {
        self.cost_with_barriers_pi(extra, 0.5, true)
    }

    pub fn cost_with_barriers_pi(
        &self,
        extra: &[BarrierFactor],
        pool_pi: f64,
        prefill: bool,
    ) -> Cost {
        if extra.is_empty() {
            return Cost::from_ln(self.selection_ln(pool_pi, prefill));
        }
        let scaled = pool_core_scale(self.effective_base_seconds(), pool_pi, prefill);
        service_cost(scaled, self.inflight(), extra, &[])
    }
}

impl PairingTarget for Backend {
    fn label(&self) -> &str {
        &self.label
    }

    fn prefill_ln(&self, snapshot: Option<&StateSnapshot>, blocks: &[u64]) -> f64 {
        self.cost_with_warmth(&[], snapshot, blocks, false).ln()
    }

    fn decode_ln(
        &self,
        snapshot: Option<&StateSnapshot>,
        blocks: &[u64],
        prefill_label: &str,
        transfer_penalty: f64,
        extra_barriers: &[BarrierFactor],
    ) -> f64 {
        let mut barriers = [BarrierFactor::clamped(1.0); MAX_SERVICE_BARRIERS];
        let mut len = 0;
        append_barriers_fail_expensive(&mut barriers, &mut len, extra_barriers);
        if prefill_label != self.label {
            append_barriers_fail_expensive(
                &mut barriers,
                &mut len,
                &[BarrierFactor::clamped(transfer_penalty.max(1.0))],
            );
        }
        self.cost_with_warmth(&barriers[..len], snapshot, blocks, true)
            .ln()
    }
}

/// Minimum-cost prefill selection over `candidates`, with the warmth discount
/// driving the choice optionally gated by cache-domain isolation.
/// [DEMI-S1-DOMAIN]
pub(crate) fn select_prefill_with_identity_pi(
    candidates: &[Arc<Backend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    pool_pi: f64,
    isolation: Option<(&SharedPrefixGroupRegistry, &RequestIdentity)>,
) -> Option<Arc<Backend>> {
    if snapshot.is_none() {
        return select_with_barriers_pi(candidates, &[], pool_pi, true);
    }
    let blocks = default_routing_blocks(prompt_tokens);
    candidates
        .iter()
        .min_by(|a, b| {
            a.cost_with_isolation_pi(&[], snapshot, &blocks, false, pool_pi, isolation)
                .ln()
                .total_cmp(
                    &b.cost_with_isolation_pi(&[], snapshot, &blocks, false, pool_pi, isolation)
                        .ln(),
                )
        })
        .cloned()
}

#[derive(Clone, Copy)]
pub(crate) struct DecodePlacementCtx<'a> {
    pub(crate) prefill_label: &'a str,
    pub(crate) pf_topo: &'a TopologyId,
    pub(crate) kv_bytes: u64,
    pub(crate) use_rdma_routing: bool,
}

impl DecodePlacementCtx<'_> {
    /// Cross-node transfer penalty for placing decode on `candidate`.
    fn transfer_penalty(&self, candidate: &Backend) -> Option<f64> {
        if self.prefill_label == candidate.label {
            return None;
        }
        let penalty = if self.use_rdma_routing {
            rdma_transfer_ln(self.kv_bytes, self.pf_topo, candidate.topology())
                .exp()
                .max(1.0)
        } else {
            ROUTING_TRANSFER_PENALTY.max(1.0)
        };
        Some(penalty)
    }
}

pub(crate) fn select_decode_with_pi(
    ctx: &DecodePlacementCtx<'_>,
    candidates: &[Arc<Backend>],
    snapshot: Option<&StateSnapshot>,
    prompt_tokens: u64,
    extra_barriers: &[BarrierFactor],
    pool_pi: f64,
) -> Option<Arc<Backend>> {
    let blocks = default_routing_blocks(prompt_tokens);
    // One reusable barrier buffer: O(candidates) work, no per-comparison clones.
    let mut barriers = Vec::with_capacity(extra_barriers.len() + 1);
    let mut best: Option<(f64, &Arc<Backend>)> = None;
    for candidate in candidates {
        barriers.clear();
        barriers.extend_from_slice(extra_barriers);
        if let Some(penalty) = ctx.transfer_penalty(candidate) {
            barriers.push(BarrierFactor::clamped(penalty));
        }
        let ln = candidate
            .cost_with_warmth_pi(&barriers, snapshot, &blocks, true, pool_pi)
            .ln();
        if best.as_ref().is_none_or(|(b, _)| ln < *b) {
            best = Some((ln, candidate));
        }
    }
    best.map(|(_, b)| Arc::clone(b))
}

pub fn select_with_warmth_pi(
    candidates: &[Arc<Backend>],
    extra: &[BarrierFactor],
    snapshot: Option<&StateSnapshot>,
    blocks: &[u64],
    decode_pool: bool,
    pool_pi: f64,
) -> Option<Arc<Backend>> {
    candidates
        .iter()
        .min_by(|a, b| {
            a.cost_with_warmth_pi(extra, snapshot, blocks, decode_pool, pool_pi)
                .ln()
                .total_cmp(
                    &b.cost_with_warmth_pi(extra, snapshot, blocks, decode_pool, pool_pi)
                        .ln(),
                )
        })
        .cloned()
}

pub fn select_with_warmth(
    candidates: &[Arc<Backend>],
    extra: &[BarrierFactor],
    snapshot: Option<&StateSnapshot>,
    blocks: &[u64],
    decode_pool: bool,
) -> Option<Arc<Backend>> {
    candidates
        .iter()
        .min_by(|a, b| {
            a.cost_with_warmth(extra, snapshot, blocks, decode_pool)
                .ln()
                .total_cmp(
                    &b.cost_with_warmth(extra, snapshot, blocks, decode_pool)
                        .ln(),
                )
        })
        .cloned()
}

/// Select minimum-cost backend, optionally with extra barriers (e.g. Φ). [DEMI-ROUTE-MINCOST]
pub fn select(candidates: &[Arc<Backend>]) -> Option<Arc<Backend>> {
    select_with_barriers(candidates, &[])
}

pub fn select_with_barriers_pi(
    candidates: &[Arc<Backend>],
    extra: &[BarrierFactor],
    pool_pi: f64,
    prefill: bool,
) -> Option<Arc<Backend>> {
    if extra.is_empty() {
        let ln_scale = pool_core_scale(1.0, pool_pi, prefill).ln();
        return candidates
            .iter()
            .min_by(|a, b| {
                let la = a.ln_base() + ln_scale + queue_ln(a.inflight());
                let lb = b.ln_base() + ln_scale + queue_ln(b.inflight());
                la.total_cmp(&lb)
            })
            .cloned();
    }
    candidates
        .iter()
        .min_by(|a, b| {
            a.cost_with_barriers_pi(extra, pool_pi, prefill)
                .ln()
                .total_cmp(&b.cost_with_barriers_pi(extra, pool_pi, prefill).ln())
        })
        .cloned()
}

pub fn select_with_barriers(
    candidates: &[Arc<Backend>],
    extra: &[BarrierFactor],
) -> Option<Arc<Backend>> {
    select_with_barriers_pi(candidates, extra, 0.5, true)
}
