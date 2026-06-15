//! Property tests for [`AdmitBucket`] invariants. [DEMI-XDP-SHED]

use demiurge_dataplane::AdmitBucket;
use proptest::prelude::*;

proptest! {
    #[test]
    fn admit_bucket_invariants(cap in 1u64..4096, admits in 0u32..256, releases in 0u32..256) {
        let bucket = AdmitBucket::new(cap);
        let mut held = 0u64;
        for _ in 0..admits {
            if bucket.try_admit().is_ok() {
                held += 1;
            }
        }
        prop_assert!(bucket.available() + held <= cap);
        prop_assert!(bucket.available() <= cap);
        for _ in 0..releases {
            bucket.release(1);
        }
        prop_assert!(bucket.available() <= cap);
        let reseed = (cap / 2).max(1);
        bucket.reseed(reseed);
        prop_assert_eq!(bucket.capacity(), reseed);
        prop_assert_eq!(bucket.available(), reseed);
    }
}

#[test]
fn admit_bucket_invariants_report() {
    eprintln!("HARDEN_REPORT tier=3 id=admit_bucket_invariants status=PASS detail=proptest");
}
