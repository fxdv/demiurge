//! Abortable live decode migration: chunked KV move, QuiesceOneStep loop, and
//! the sub-ITL cutover gate. [DEMI-MIG-SUBITL]
//!
//! Track A ships the cutover *logic* and its telemetry; the budget itself is
//! measured on reference fleet hardware (Track C).

use std::sync::Mutex;

use demiurge_cost::MIGRATION_ITL_BUDGET_FRACTION_EPS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationDecision {
    Commit,
    Abort,
}

#[derive(Debug, Clone, Copy)]
pub struct MigrationBudget {
    pub itl_us: u64,
    pub epsilon: f64,
}

impl MigrationBudget {
    /// Budget for an inter-token latency, using the canonical
    /// `migration.itl_budget_fraction_eps` from the generated params.
    #[must_use]
    pub fn for_itl(itl_us: u64) -> Self {
        Self {
            itl_us,
            epsilon: MIGRATION_ITL_BUDGET_FRACTION_EPS,
        }
    }

    /// Absolute stall ceiling in microseconds: `epsilon * ITL`.
    #[must_use]
    pub fn limit_us(&self) -> u64 {
        (self.epsilon.max(0.0) * self.itl_us as f64) as u64
    }
}

/// Cutover commits only when estimated stall ≤ ε · ITL; otherwise the original
/// placement is restored. [DEMI-MIG-SUBITL]
#[must_use]
pub fn evaluate_cutover(est_stall_us: u64, budget: &MigrationBudget) -> MigrationDecision {
    if est_stall_us <= budget.limit_us() {
        MigrationDecision::Commit
    } else {
        MigrationDecision::Abort
    }
}

/// Chunked-move model for the QuiesceOneStep loop. KV is copied to the target in
/// background passes; each pass re-dirties a fraction of what it copied. The
/// migration converges when the remaining dirty set fits in a single chunk — the
/// final one-step quiesce whose copy time is the estimated cutover stall.
#[derive(Debug, Clone, Copy)]
pub struct QuiesceModel {
    pub total_bytes: u64,
    pub chunk_bytes: u64,
    pub per_chunk_us: u64,
    /// Fraction of copied bytes re-dirtied each background pass, in `[0, 1)`.
    pub dirty_fraction: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuiesceOutcome {
    pub passes: u32,
    /// Estimated stall of the final QuiesceOneStep (copy of the residual set).
    pub est_stall_us: u64,
    /// Bytes copied under the final quiesce.
    pub residual_bytes: u64,
}

impl QuiesceModel {
    fn chunk(&self) -> u64 {
        self.chunk_bytes.max(1)
    }

    /// Time to copy `bytes` as whole chunks.
    fn copy_us(&self, bytes: u64) -> u64 {
        bytes
            .div_ceil(self.chunk())
            .saturating_mul(self.per_chunk_us)
    }

    /// Run background copy passes until the dirty set fits in one chunk (final
    /// QuiesceOneStep) or `max_passes` is exhausted. The estimated cutover stall
    /// is the copy time of whatever remains. [DEMI-MIG-SUBITL]
    #[must_use]
    pub fn quiesce(&self, max_passes: u32) -> QuiesceOutcome {
        let chunk = self.chunk();
        let frac = self.dirty_fraction.clamp(0.0, 0.999_999);
        let mut dirty = self.total_bytes;
        let mut passes = 0u32;
        while dirty > chunk && passes < max_passes {
            dirty = (dirty as f64 * frac) as u64;
            passes += 1;
        }
        QuiesceOutcome {
            passes,
            est_stall_us: self.copy_us(dirty),
            residual_bytes: dirty,
        }
    }
}

/// One recorded migration: estimated vs measured cutover stall and the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationStallSample {
    pub estimated_us: u64,
    pub measured_us: u64,
    pub decision: MigrationDecision,
}

/// In-memory ring buffer of migration stall samples (shadow telemetry; no
/// production actuation). Mirrors the corrector/RDMA shadow logs.
#[derive(Debug, Default)]
pub struct MigrationStallLog {
    inner: Mutex<Vec<MigrationStallSample>>,
    max_samples: usize,
}

impl MigrationStallLog {
    #[must_use]
    pub fn new(max_samples: usize) -> Self {
        Self {
            inner: Mutex::new(Vec::with_capacity(max_samples.min(4096))),
            max_samples: max_samples.max(1),
        }
    }

    /// Record a migration stall sample (the `RecordMigrationStall` telemetry).
    pub fn record(&self, sample: MigrationStallSample) {
        let mut buf = self.inner.lock().expect("migration stall lock");
        if buf.len() >= self.max_samples {
            buf.remove(0);
        }
        buf.push(sample);
    }

    #[must_use]
    pub fn samples(&self) -> Vec<MigrationStallSample> {
        self.inner.lock().expect("migration stall lock").clone()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("migration stall lock").len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Mean absolute error between estimated and measured stall over all samples.
    #[must_use]
    pub fn mean_abs_error_us(&self) -> f64 {
        let buf = self.inner.lock().expect("migration stall lock");
        if buf.is_empty() {
            return 0.0;
        }
        let sum: u64 = buf
            .iter()
            .map(|s| s.estimated_us.abs_diff(s.measured_us))
            .sum();
        sum as f64 / buf.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_aborts_when_over_budget() {
        let budget = MigrationBudget {
            itl_us: 1_000,
            epsilon: 0.05,
        };
        assert_eq!(evaluate_cutover(40, &budget), MigrationDecision::Commit);
        assert_eq!(evaluate_cutover(60, &budget), MigrationDecision::Abort);
    }

    #[test]
    fn budget_uses_canonical_epsilon() {
        let budget = MigrationBudget::for_itl(1_000);
        assert_eq!(budget.epsilon, MIGRATION_ITL_BUDGET_FRACTION_EPS);
        assert_eq!(
            budget.limit_us(),
            (MIGRATION_ITL_BUDGET_FRACTION_EPS * 1_000.0) as u64
        );
    }

    #[test]
    fn quiesce_one_step_accumulates_estimated_stall() {
        let model = QuiesceModel {
            total_bytes: 1_000_000,
            chunk_bytes: 64 * 1024,
            per_chunk_us: 50,
            dirty_fraction: 0.25,
        };
        let out = model.quiesce(32);

        // The loop ran (large dirty set) and converged into a single chunk.
        assert!(out.passes >= 1);
        assert!(out.residual_bytes <= model.chunk_bytes);
        // Final-step stall is far below copying the whole footprint at once.
        assert!(out.est_stall_us <= model.copy_us(model.total_bytes));

        // A faster dirtier workload needs more passes / leaves no less residual.
        let dirtier = QuiesceModel {
            dirty_fraction: 0.75,
            ..model
        };
        let out_dirty = dirtier.quiesce(32);
        assert!(out_dirty.passes >= out.passes);

        // With zero re-dirtying it converges in exactly one pass.
        let clean = QuiesceModel {
            dirty_fraction: 0.0,
            ..model
        };
        assert_eq!(clean.quiesce(32).passes, 1);
        assert_eq!(clean.quiesce(32).residual_bytes, 0);
    }

    #[test]
    fn record_migration_stall_tracks_estimated_vs_measured() {
        let log = MigrationStallLog::new(8);
        assert!(log.is_empty());
        log.record(MigrationStallSample {
            estimated_us: 100,
            measured_us: 120,
            decision: MigrationDecision::Commit,
        });
        log.record(MigrationStallSample {
            estimated_us: 200,
            measured_us: 180,
            decision: MigrationDecision::Abort,
        });
        assert_eq!(log.len(), 2);
        // Mean |est - measured| = (20 + 20) / 2 = 20.
        assert!((log.mean_abs_error_us() - 20.0).abs() < 1e-9);

        // Ring buffer caps at max_samples.
        for i in 0..20 {
            log.record(MigrationStallSample {
                estimated_us: i,
                measured_us: i,
                decision: MigrationDecision::Commit,
            });
        }
        assert_eq!(log.len(), 8);
    }
}
