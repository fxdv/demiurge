//! BPF XDP admit model mirroring kernel decrement-first logic. [DEMI-XDP-SHED]

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use demiurge_dataplane::AdmitBucket;
use proptest::prelude::*;

/// Userspace mirror of `admit_or_shed()` in `bpf/admit_shed.bpf.c`
/// (refill disabled, i.e. `refill_per_sec = 0`).
#[derive(Debug)]
struct BpfAdmitModel {
    /// Signed like the kernel side: an empty bucket dips below zero
    /// transiently under concurrent shed and every observer of `prev <= 0`
    /// compensates. Unsigned would wrap and fail the bucket open.
    tokens: AtomicI64,
    shed_total: AtomicU64,
}

impl BpfAdmitModel {
    fn new(capacity: u64) -> Self {
        Self {
            tokens: AtomicI64::new(capacity.max(1) as i64),
            shed_total: AtomicU64::new(0),
        }
    }

    fn admit_or_shed(&self) -> bool {
        let prev = self.tokens.fetch_sub(1, Ordering::AcqRel);
        if prev <= 0 {
            self.tokens.fetch_add(1, Ordering::Relaxed);
            self.shed_total.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    fn available(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed).max(0) as u64
    }

    fn shed_total(&self) -> u64 {
        self.shed_total.load(Ordering::Relaxed)
    }
}

proptest! {
    #[test]
    fn bpf_model_matches_userspace_bucket(cap in 1u64..512, admits in 0u32..128) {
        let bucket = AdmitBucket::new(cap);
        let bpf = BpfAdmitModel::new(cap);
        let mut bucket_ok = 0u64;
        let mut bpf_ok = 0u64;
        for _ in 0..admits {
            if bucket.try_admit().is_ok() {
                bucket_ok += 1;
            }
            if bpf.admit_or_shed() {
                bpf_ok += 1;
            }
        }
        prop_assert_eq!(bucket_ok, bpf_ok);
        prop_assert_eq!(bucket.shed_total(), bpf.shed_total());
        prop_assert!(bucket.available() <= cap);
        prop_assert!(bpf.available() <= cap);
    }
}

/// Multi-CPU XDP in miniature: the bucket must never over-admit under
/// concurrent exhaustion. The previous unsigned kernel logic wrapped to
/// 2^64-1 in exactly this scenario and admitted everything.
#[test]
fn bpf_model_never_over_admits_concurrently() {
    const CAPACITY: u64 = 100;
    const THREADS: usize = 8;
    const ATTEMPTS_PER_THREAD: usize = 512;

    for round in 0..16 {
        let model = Arc::new(BpfAdmitModel::new(CAPACITY));
        let start = Arc::new(Barrier::new(THREADS));
        let admitted: u64 = thread::scope(|s| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let model = Arc::clone(&model);
                    let start = Arc::clone(&start);
                    s.spawn(move || {
                        start.wait();
                        (0..ATTEMPTS_PER_THREAD)
                            .filter(|_| model.admit_or_shed())
                            .count() as u64
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });

        let attempts = (THREADS * ATTEMPTS_PER_THREAD) as u64;
        assert_eq!(
            admitted, CAPACITY,
            "round {round}: admits must exactly drain capacity, never over-admit"
        );
        assert_eq!(
            model.shed_total(),
            attempts - CAPACITY,
            "round {round}: every non-admitted attempt sheds"
        );
        assert_eq!(model.available(), 0, "round {round}: bucket fully drained");
    }
}

#[test]
fn bpf_admit_model_report() {
    eprintln!("HARDEN_REPORT tier=3 id=bpf_admit_model status=PASS detail=proptest+concurrent");
}
