use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use demiurge_dataplane::RcuRoutingTable;

fn percentile_ns(mut samples: Vec<u128>, p: f64) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = ((samples.len() as f64 * p) as usize).min(samples.len() - 1);
    samples[idx]
}

// [DEMI-DP-RCU] — slow control-plane publish must not inflate read_pi p99.
#[test]
fn rcu_read_p99_unchanged_under_slow_publish() {
    let table = RcuRoutingTable::new(0.5);
    let reader_table = Arc::clone(&table);
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);

    let reader = thread::spawn(move || {
        let mut samples = Vec::with_capacity(20_000);
        while !stop2.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            std::hint::black_box(reader_table.read_pi());
            samples.push(t0.elapsed().as_nanos());
            if samples.len() >= 20_000 {
                break;
            }
        }
        samples
    });

    for gen in 0..32 {
        thread::sleep(Duration::from_millis(10));
        table.publish_pi(gen, (gen as f64 / 64.0).clamp(0.05, 0.95));
    }

    stop.store(true, Ordering::Relaxed);
    let samples = reader.join().expect("reader");
    let p99 = percentile_ns(samples, 0.99);
    assert!(
        p99 < 100_000,
        "read_pi p99 {p99} ns expected sub-100µs under concurrent slow publish"
    );
}
