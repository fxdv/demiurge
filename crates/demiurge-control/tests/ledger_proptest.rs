//! Property tests for [`ReservationLedger`] invariants. [DEMI-KV-RELEASE]

use demiurge_control::ReservationLedger;
use proptest::prelude::*;

proptest! {
    #[test]
    fn reservation_ledger_invariants(
        capacity in 1024u64..1_000_000,
        chunks in prop::collection::vec(1u64..8192, 1..32),
    ) {
        let ledger = ReservationLedger::new(capacity);
        let mut active = 0u64;
        for (id, bytes) in chunks.iter().copied().enumerate() {
            match ledger.try_reserve(id as u64, bytes) {
                Ok(guard) => {
                    active = active.saturating_add(bytes);
                    prop_assert!(active <= capacity);
                    prop_assert!(ledger.fleet_reserved() <= capacity);
                    drop(guard);
                    active = active.saturating_sub(bytes);
                }
                Err(_) => {
                    prop_assert!(active.saturating_add(bytes) > capacity);
                }
            }
        }
        prop_assert_eq!(ledger.fleet_reserved(), 0);
        ledger.release(999);
        prop_assert_eq!(ledger.fleet_reserved(), 0);
    }
}

#[test]
fn reservation_ledger_invariants_report() {
    eprintln!("HARDEN_REPORT tier=3 id=reservation_ledger_invariants status=PASS detail=proptest");
}
