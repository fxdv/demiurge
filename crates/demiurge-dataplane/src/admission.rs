//! Token-bucket admission shed (XDP proof in userspace). [DEMI-XDP-SHED]

use std::sync::atomic::{AtomicU64, Ordering};

/// Shed reason when the admit bucket is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedReason {
    BucketExhausted,
}

impl std::fmt::Display for ShedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShedReason::BucketExhausted => write!(f, "admit bucket exhausted"),
        }
    }
}

impl std::error::Error for ShedReason {}

/// Fixed-capacity token bucket for overload shedding before L7.
#[derive(Debug)]
pub struct AdmitBucket {
    tokens: AtomicU64,
    capacity: AtomicU64,
    shed_total: AtomicU64,
}

impl AdmitBucket {
    pub fn new(capacity: u64) -> Self {
        let cap = capacity.max(1);
        Self {
            tokens: AtomicU64::new(cap),
            capacity: AtomicU64::new(cap),
            shed_total: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Relaxed)
    }

    pub fn available(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
    }

    pub fn shed_total(&self) -> u64 {
        self.shed_total.load(Ordering::Relaxed)
    }

    /// Try to consume one admit token.
    pub fn try_admit(&self) -> Result<(), ShedReason> {
        loop {
            let cur = self.tokens.load(Ordering::Relaxed);
            if cur == 0 {
                self.shed_total.fetch_add(1, Ordering::Relaxed);
                return Err(ShedReason::BucketExhausted);
            }
            if self
                .tokens
                .compare_exchange_weak(cur, cur - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    /// Return tokens up to capacity (e.g. after request completes).
    pub fn release(&self, count: u64) {
        if count == 0 {
            return;
        }
        loop {
            let cur = self.tokens.load(Ordering::Relaxed);
            let cap = self.capacity.load(Ordering::Relaxed);
            let next = (cur + count).min(cap);
            if self
                .tokens
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Reset tokens to `capacity` and update capacity (control-plane / XDP map sync).
    pub fn reseed(&self, capacity: u64) {
        let cap = capacity.max(1);
        self.capacity.store(cap, Ordering::Relaxed);
        self.tokens.store(cap, Ordering::Relaxed);
    }
}
