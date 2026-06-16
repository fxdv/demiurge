//! Integration gate for trace-driven fleet simulation. [DEMI-FLEET-SIM]

use demiurge_control::{
    eval_fleet_sim_gate, load_fleet_trace, shadow_pilot_for_trace, FleetWindowResult,
};

#[test]
fn fleet_sim_gate_on_synthetic_trace() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../design/traces/synthetic-fleet.jsonl");
    let windows = load_fleet_trace(&path).expect("trace");
    let pilot = shadow_pilot_for_trace(&windows, 0.45);
    assert!(
        pilot.gate_pass,
        "shadow pilot should pass on synthetic trace"
    );

    let live: Vec<FleetWindowResult> = windows
        .iter()
        .enumerate()
        .map(|(i, w)| FleetWindowResult {
            ts_ms: w.ts_ms,
            prefill_heavy: w.prefill_heavy,
            held_out: w.held_out,
            ok: if w.prefill_heavy { 20 } else { 5 },
            errors: 0,
            errors_graceful: 0,
            errors_hard: 0,
            p99_us: 1000,
            dataplane_pi: pilot.replays.get(i).map(|r| r.pi_star).unwrap_or(0.5),
            pi_star: pilot.replays.get(i).map(|r| r.pi_star).unwrap_or(0.5),
        })
        .collect();

    let report = eval_fleet_sim_gate(&pilot, &live, 0.45, None, None);
    assert!(
        report.gate_pass,
        "synthetic trace with π_live=π* should pass fleet gate"
    );
}
