//! Live decode migration cutover gate (Track C scaffolding). [DEMI-MIG-SUBITL]

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

/// Cutover commits only when estimated stall ≤ ε · ITL.
pub fn evaluate_cutover(est_stall_us: u64, budget: &MigrationBudget) -> MigrationDecision {
    let limit = (budget.epsilon * budget.itl_us as f64) as u64;
    if est_stall_us <= limit {
        MigrationDecision::Commit
    } else {
        MigrationDecision::Abort
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
}
