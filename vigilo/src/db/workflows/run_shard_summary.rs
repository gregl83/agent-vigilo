//! Shard-local run summary workflow helpers.
//!
//! Summaries roll chunk and execution progress up to one row per
//! `run_id + run_shard` so later control-database finalization can combine shard
//! results without scanning all execution rows directly.

use uuid::Uuid;

/// Current shard-local summary for one run shard.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct RunShardSummary {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) expected_execution_count: i32,
    pub(crate) execution_count: i32,
    pub(crate) terminal_execution_count: i32,
    pub(crate) aggregate_count: i32,
    pub(crate) passed_execution_count: i32,
    pub(crate) failed_execution_count: i32,
    pub(crate) errored_execution_count: i32,
    pub(crate) skipped_execution_count: i32,
    pub(crate) missing_aggregate_count: i32,
    pub(crate) evaluator_result_count: i64,
    pub(crate) blocking_failure_count: i64,
    pub(crate) score_count: i64,
    pub(crate) score_sum: f64,
    pub(crate) min_score: Option<f64>,
    pub(crate) max_score: Option<f64>,
    pub(crate) failed_chunk_count: i32,
    pub(crate) cancelled_chunk_count: i32,
    pub(crate) status: String,
}

impl RunShardSummary {
    pub(crate) fn is_terminal(&self) -> bool {
        is_terminal_summary_status(&self.status)
    }
}

fn is_terminal_summary_status(status: &str) -> bool {
    matches!(status, "completed" | "failed")
}

mod queries;

#[cfg(test)]
pub(crate) use queries::refresh_run_shard_summary;
pub(crate) use queries::{
    refresh_run_shard_summary_with,
    select_run_shard_summary,
};

#[cfg(test)]
#[path = "run_shard_summary/postgres_tests.rs"]
mod postgres_tests;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_summary_statuses_are_strict_and_exhaustive() {
        for terminal in ["completed", "failed"] {
            assert!(is_terminal_summary_status(terminal));
        }
        for nonterminal in ["", "running", "pending", "cancelled", "Completed"] {
            assert!(!is_terminal_summary_status(nonterminal));
        }
    }
}
