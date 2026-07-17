//! Decode-pool KV reservation ledger. [DEMI-KV-RELEASE] [DEMI-BARRIER-PHI]
//!
//! Phase 4: greedy pairing, length predictor, shadow pool rebalancer.

mod corrector_grad;
mod corrector_shadow;
mod fleet_pilot;
mod fleet_sim;
mod migration;
mod pairing;
mod predictor;
mod pressure;
mod rdma_cost_shadow;
mod rebalancer;
mod scored;

pub use corrector_grad::{
    eval_corrector_graduation, is_clamp_saturated, GraduationController, GraduationGate,
    GraduationStage, GraduationStep,
};
pub use corrector_shadow::{
    delta_within_envelope, eval_goodput_improvement, train_bounded_delta, CorrectorShadowLog,
    CorrectorShadowSample,
};
pub use fleet_pilot::{
    point_biserial_corr, replay_fleet_pilot, FleetPilotReport, TraceWindow, WindowReplay,
};
pub use fleet_sim::{
    eval_fleet_sim_gate, jitter_delay_us, load_fleet_trace, shadow_pilot_for_trace, tier_delay_us,
    window_knobs, FleetSimReport, FleetWindowResult, SimBaseKnobs, WindowKnobs,
};
pub use migration::{
    evaluate_cutover, MigrationBudget, MigrationDecision, MigrationStallLog, MigrationStallSample,
    QuiesceModel, QuiesceOutcome,
};
pub use pairing::{
    greedy_pair, oracle_pair, pairing_regret, pairing_regret_targets, select_decode,
    select_decode_target, select_prefill, select_prefill_target, PairingTarget,
    DEFAULT_TRANSFER_PENALTY,
};
pub use predictor::LengthPredictor;
pub use pressure::{export_pool_pressure, PoolPressure};
pub use rdma_cost_shadow::{eval_transfer_ratio_median, RdmaCostShadowLog, RdmaCostShadowSample};
pub use rebalancer::{PoolRebalancer, RebalancerMode};
pub use scored::ScoredBackend;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use demiurge_cost::{
    kv_breakdown, percentile90, phi_barrier_marginal, BarrierFactor, KV_ABANDONED_SESSION_TTL_S,
    KV_MAX_OUTSTANDING_PER_SOURCE_FRACTION,
};

const RECENT_SAMPLES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerMetrics {
    pub kv_bytes_reserved: u64,
    pub kv_admit_rejects_total: u64,
    pub kv_reservation_error: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitError {
    OverCapacity,
    DuplicateRequest,
}

impl std::fmt::Display for AdmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmitError::OverCapacity => write!(f, "decode pool KV capacity exceeded"),
            AdmitError::DuplicateRequest => write!(f, "request already reserved"),
        }
    }
}

impl std::error::Error for AdmitError {}

#[derive(Debug)]
struct ReservationEntry {
    bytes: u64,
    created: Instant,
    source_label: Option<String>,
}

/// Fleet-aggregate reservation ledger with TTL reclaim.
#[derive(Debug)]
pub struct ReservationLedger {
    capacity_bytes: u64,
    /// Soft cap on outstanding bytes attributed to one prefill source (G3).
    max_per_source: u64,
    reserved_bytes: AtomicU64,
    admit_rejects: AtomicU64,
    reservation_errors: AtomicU64,
    reservations: Mutex<HashMap<u64, ReservationEntry>>,
    source_outstanding: Mutex<HashMap<String, u64>>,
    recent_sizes: Mutex<VecDeque<u64>>,
}

impl ReservationLedger {
    pub fn new(capacity_bytes: u64) -> Arc<Self> {
        let frac = KV_MAX_OUTSTANDING_PER_SOURCE_FRACTION.clamp(0.0, 1.0);
        let max_per_source = ((capacity_bytes as f64) * frac).ceil() as u64;
        Arc::new(Self {
            capacity_bytes,
            max_per_source: max_per_source.max(1),
            reserved_bytes: AtomicU64::new(0),
            admit_rejects: AtomicU64::new(0),
            reservation_errors: AtomicU64::new(0),
            reservations: Mutex::new(HashMap::new()),
            source_outstanding: Mutex::new(HashMap::new()),
            recent_sizes: Mutex::new(VecDeque::with_capacity(RECENT_SAMPLES)),
        })
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    pub fn fleet_reserved(&self) -> u64 {
        self.reserved_bytes.load(Ordering::Relaxed)
    }

    pub fn p90_increment(&self) -> u64 {
        let recent = self.recent_sizes.lock().expect("recent lock");
        percentile90(recent.iter().copied().collect())
    }

    pub fn phi_barrier(&self) -> BarrierFactor {
        phi_barrier_marginal(
            self.fleet_reserved(),
            self.p90_increment(),
            self.capacity_bytes,
        )
    }

    pub fn metrics(&self) -> LedgerMetrics {
        LedgerMetrics {
            kv_bytes_reserved: self.fleet_reserved(),
            kv_admit_rejects_total: self.admit_rejects.load(Ordering::Relaxed),
            kv_reservation_error: self.reservation_errors.load(Ordering::Relaxed),
        }
    }

    /// Reserve `bytes` for `request_id` using fleet-aggregate marginal check.
    pub fn try_reserve(
        self: &Arc<Self>,
        request_id: u64,
        bytes: u64,
    ) -> Result<ReservationGuard, AdmitError> {
        self.try_reserve_from(None, request_id, bytes)
    }

    /// Like [`Self::try_reserve`], additionally enforcing a per-source outstanding
    /// byte quota so one compromised prefill cannot monopolize the ledger (G3).
    pub fn try_reserve_from(
        self: &Arc<Self>,
        source_label: Option<&str>,
        request_id: u64,
        bytes: u64,
    ) -> Result<ReservationGuard, AdmitError> {
        if bytes == 0 {
            self.admit_rejects.fetch_add(1, Ordering::Relaxed);
            return Err(AdmitError::OverCapacity);
        }

        let mut reservations = self.reservations.lock().expect("reservations lock");
        if reservations.contains_key(&request_id) {
            return Err(AdmitError::DuplicateRequest);
        }

        let fleet = self.reserved_bytes.load(Ordering::Relaxed);
        if fleet.saturating_add(bytes) > self.capacity_bytes {
            self.admit_rejects.fetch_add(1, Ordering::Relaxed);
            return Err(AdmitError::OverCapacity);
        }

        let mut sources = self.source_outstanding.lock().expect("source lock");
        if let Some(label) = source_label {
            let outstanding = sources.get(label).copied().unwrap_or(0);
            if outstanding.saturating_add(bytes) > self.max_per_source {
                self.admit_rejects.fetch_add(1, Ordering::Relaxed);
                return Err(AdmitError::OverCapacity);
            }
            *sources.entry(label.to_string()).or_insert(0) += bytes;
        }

        reservations.insert(
            request_id,
            ReservationEntry {
                bytes,
                created: Instant::now(),
                source_label: source_label.map(str::to_string),
            },
        );
        drop(sources);
        self.reserved_bytes.fetch_add(bytes, Ordering::Relaxed);

        let mut recent = self.recent_sizes.lock().expect("recent lock");
        if recent.len() >= RECENT_SAMPLES {
            recent.pop_front();
        }
        recent.push_back(bytes);

        Ok(ReservationGuard {
            ledger: Arc::clone(self),
            request_id,
            released: false,
        })
    }

    pub fn reserve_for_prompt(
        self: &Arc<Self>,
        request_id: u64,
        prompt_tokens: u64,
        bytes_per_token: u64,
    ) -> Result<ReservationGuard, AdmitError> {
        let bytes = kv_breakdown(prompt_tokens, bytes_per_token).kv_reserved;
        self.try_reserve(request_id, bytes)
    }

    fn release_inner(&self, request_id: u64) -> bool {
        let mut reservations = self.reservations.lock().expect("reservations lock");
        if let Some(entry) = reservations.remove(&request_id) {
            self.reserved_bytes
                .fetch_sub(entry.bytes, Ordering::Relaxed);
            if let Some(label) = entry.source_label.as_deref() {
                let mut sources = self.source_outstanding.lock().expect("source lock");
                if let Some(out) = sources.get_mut(label) {
                    *out = out.saturating_sub(entry.bytes);
                    if *out == 0 {
                        sources.remove(label);
                    }
                }
            }
            true
        } else {
            self.reservation_errors.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// Release on session end or abort. [DEMI-KV-RELEASE]
    pub fn release(&self, request_id: u64) {
        let _ = self.release_inner(request_id);
    }

    /// TTL reclaim for abandoned sessions. [DEMI-KV-RELEASE]
    pub fn reclaim_expired(&self) -> usize {
        let ttl = Duration::from_secs(KV_ABANDONED_SESSION_TTL_S);
        let now = Instant::now();
        let expired: Vec<u64> = {
            let reservations = self.reservations.lock().expect("reservations lock");
            reservations
                .iter()
                .filter(|(_, e)| now.duration_since(e.created) >= ttl)
                .map(|(id, _)| *id)
                .collect()
        };
        for id in &expired {
            self.release(*id);
        }
        expired.len()
    }
}

/// RAII guard — releases reservation on drop (session end). [DEMI-KV-RELEASE]
pub struct ReservationGuard {
    ledger: Arc<ReservationLedger>,
    request_id: u64,
    released: bool,
}

impl ReservationGuard {
    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    pub fn release(mut self) {
        if !self.released {
            self.ledger.release(self.request_id);
            self.released = true;
        }
    }

    /// Resolve a live migration whose target reservation is `target`. On
    /// `Commit` the source (`self`) is released and the target survives; on
    /// `Abort` the target is released and the source survives untouched.
    /// Exactly one reservation remains, so the fleet total is single-counted
    /// after the call. [DEMI-MIG-SUBITL]
    #[must_use]
    pub fn resolve_migration(
        self,
        target: ReservationGuard,
        decision: MigrationDecision,
    ) -> ReservationGuard {
        match decision {
            MigrationDecision::Commit => {
                self.release();
                target
            }
            MigrationDecision::Abort => {
                target.release();
                self
            }
        }
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if !self.released {
            self.ledger.release(self.request_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_released_on_session_end() {
        let ledger = ReservationLedger::new(10_000);
        {
            let _g = ledger.try_reserve(1, 500).expect("reserve");
            assert_eq!(ledger.fleet_reserved(), 500);
        }
        assert_eq!(ledger.fleet_reserved(), 0);
    }

    #[test]
    fn handoff_burst_no_oom() {
        let per_req = kv_breakdown(512, 128).kv_reserved;
        let capacity = per_req * 10;
        let ledger = ReservationLedger::new(capacity);
        let mut guards = Vec::new();
        for id in 0..10 {
            guards.push(ledger.try_reserve(id, per_req).expect("admit"));
        }
        assert_eq!(ledger.fleet_reserved(), capacity);
        assert!(ledger.try_reserve(99, per_req).is_err());
        assert_eq!(ledger.fleet_reserved(), capacity);
        drop(guards);
        assert_eq!(ledger.fleet_reserved(), 0);
    }

    #[test]
    fn per_source_outstanding_quota_sheds() {
        // capacity 1000 → 25% per-source = 250.
        let ledger = ReservationLedger::new(1000);
        let g1 = ledger
            .try_reserve_from(Some("pf-a"), 1, 200)
            .expect("first");
        assert!(
            ledger.try_reserve_from(Some("pf-a"), 2, 100).is_err(),
            "same source over quota"
        );
        let g2 = ledger
            .try_reserve_from(Some("pf-b"), 3, 200)
            .expect("other source ok");
        drop(g1);
        drop(g2);
        assert_eq!(ledger.fleet_reserved(), 0);
    }

    #[test]
    fn migration_commit_transfers_reservation() {
        let ledger = ReservationLedger::new(10_000);
        let source = ledger.try_reserve(1, 500).expect("source");
        // Transient double-reservation during the chunked move.
        let target = ledger.try_reserve(2, 500).expect("target");
        assert_eq!(ledger.fleet_reserved(), 1_000);

        let survivor = source.resolve_migration(target, MigrationDecision::Commit);
        // Source released, target kept: single reservation, on the target id.
        assert_eq!(survivor.request_id(), 2);
        assert_eq!(ledger.fleet_reserved(), 500);
        drop(survivor);
        assert_eq!(ledger.fleet_reserved(), 0);
    }

    #[test]
    fn migration_abort_restores_source_reservation() {
        let ledger = ReservationLedger::new(10_000);
        let source = ledger.try_reserve(1, 500).expect("source");
        let target = ledger.try_reserve(2, 500).expect("target");
        assert_eq!(ledger.fleet_reserved(), 1_000);

        let survivor = source.resolve_migration(target, MigrationDecision::Abort);
        // Target released, original source placement untouched.
        assert_eq!(survivor.request_id(), 1);
        assert_eq!(ledger.fleet_reserved(), 500);
        drop(survivor);
        assert_eq!(ledger.fleet_reserved(), 0);
    }

    #[test]
    fn ledger_consistent_after_commit_and_abort() {
        let ledger = ReservationLedger::new(10_000);

        // Commit path.
        let s1 = ledger.try_reserve(1, 400).expect("s1");
        let t1 = ledger.try_reserve(2, 400).expect("t1");
        let g1 = s1.resolve_migration(t1, MigrationDecision::Commit);

        // Abort path, concurrently held.
        let s2 = ledger.try_reserve(3, 600).expect("s2");
        let t2 = ledger.try_reserve(4, 600).expect("t2");
        let g2 = s2.resolve_migration(t2, MigrationDecision::Abort);

        // One survivor per migration: 400 (target of commit) + 600 (source of abort).
        assert_eq!(ledger.fleet_reserved(), 1_000);
        drop(g1);
        drop(g2);
        assert_eq!(ledger.fleet_reserved(), 0);
        // No phantom releases occurred (released ids were not double-counted).
        assert_eq!(ledger.metrics().kv_reservation_error, 0);
    }
}
