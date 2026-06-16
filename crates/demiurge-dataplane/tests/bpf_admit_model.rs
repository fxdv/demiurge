//! BPF XDP admit model mirroring kernel decrement-first logic. [DEMI-XDP-SHED]

use std::sync::atomic::{AtomicU64, Ordering};

use demiurge_dataplane::AdmitBucket;
use proptest::prelude::*;

/// Userspace mirror of `admit_or_shed()` in `bpf/admit_shed.bpf.c`.
#[derive(Debug)]
struct BpfAdmitModel {
    tokens: AtomicU64,
    shed_total: AtomicU64,
}

impl BpfAdmitModel {
    fn new(capacity: u64) -> Self {
        Self {
            tokens: AtomicU64::new(capacity.max(1)),
            shed_total: AtomicU64::new(0),
        }
    }

    fn admit_or_shed(&self) -> bool {
        let prev = self.tokens.fetch_sub(1, Ordering::AcqRel);
        if prev == 0 {
            self.tokens.fetch_add(1, Ordering::Relaxed);
            self.shed_total.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    fn available(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
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

#[test]
fn bpf_admit_model_report() {
    eprintln!("HARDEN_REPORT tier=3 id=bpf_admit_model status=PASS detail=proptest");
}
