//! Decode-pool KV reservation ledger. [DEMI-KV-RELEASE] [DEMI-BARRIER-PHI]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use demiurge_cost::{
    kv_breakdown, percentile90, phi_barrier_marginal, BarrierFactor, KV_ABANDONED_SESSION_TTL_S,
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
}

/// Fleet-aggregate reservation ledger with TTL reclaim.
#[derive(Debug)]
pub struct ReservationLedger {
    capacity_bytes: u64,
    reserved_bytes: AtomicU64,
    admit_rejects: AtomicU64,
    reservation_errors: AtomicU64,
    reservations: Mutex<HashMap<u64, ReservationEntry>>,
    recent_sizes: Mutex<VecDeque<u64>>,
}

impl ReservationLedger {
    pub fn new(capacity_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            capacity_bytes,
            reserved_bytes: AtomicU64::new(0),
            admit_rejects: AtomicU64::new(0),
            reservation_errors: AtomicU64::new(0),
            reservations: Mutex::new(HashMap::new()),
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

        reservations.insert(
            request_id,
            ReservationEntry {
                bytes,
                created: Instant::now(),
            },
        );
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
}
