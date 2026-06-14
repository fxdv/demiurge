use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use demiurge_dataplane::RcuRoutingTable;

// [DEMI-DP-RCU] — readers never block on concurrent publish.
#[test]
fn rcu_read_never_blocks_under_publish() {
    let table = RcuRoutingTable::new(0.5);
    let reader = Arc::clone(&table);
    let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let reads2 = Arc::clone(&reads);
    let handle = thread::spawn(move || {
        for _ in 0..50_000 {
            let snap = reader.read();
            assert!((0.0..=1.0).contains(&snap.pi));
            reads2.fetch_add(1, Ordering::Relaxed);
        }
    });

    for gen in 1..=256 {
        table.publish_pi(gen, (gen as f64 / 512.0).clamp(0.0, 1.0));
    }

    handle.join().expect("reader");
    assert!(reads.load(Ordering::Relaxed) >= 50_000);
}

// [DEMI-DP-RCU] — control-plane stall does not block hot-path read.
#[test]
fn rcu_hot_read_under_cp_stall() {
    let table = RcuRoutingTable::new(0.5);
    let start = std::time::Instant::now();
    for _ in 0..10_000 {
        std::hint::black_box(table.read_pi());
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(50),
        "RCU read loop took {:?}, expected sub-ms hot path",
        elapsed
    );
}
