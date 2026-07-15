//! Routed run status projection helpers.
//!
//! Status reads keep the control run row authoritative while adding live
//! progress from shard-local summaries when execution placements have produced
//! them. The workflow avoids scanning execution hot tables on every status or
//! watch poll.

use uuid::Uuid;

use super::run_shard_summary::{
    RunShardSummary,
    select_run_shard_summary,
};
use crate::{
    context::database,
    db::tables::runs,
    models::run::Run,
};

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RunProgressSummary {
    pub(crate) expected_execution_count: i64,
    pub(crate) execution_count: i64,
    pub(crate) terminal_execution_count: i64,
    pub(crate) passed_execution_count: i64,
    pub(crate) failed_execution_count: i64,
    pub(crate) errored_execution_count: i64,
    pub(crate) skipped_execution_count: i64,
    pub(crate) missing_aggregate_count: i64,
    pub(crate) failed_chunk_count: i64,
    pub(crate) cancelled_chunk_count: i64,
}

impl RunProgressSummary {
    pub(crate) fn from_control_run(run: &Run) -> Self {
        Self {
            expected_execution_count: i64::from(run.expected_execution_count),
            terminal_execution_count: i64::from(run.terminal_execution_count),
            passed_execution_count: i64::from(run.passed_execution_count),
            failed_execution_count: i64::from(run.failed_execution_count),
            errored_execution_count: i64::from(run.errored_execution_count),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RunStatusProjection {
    pub(crate) run: Run,
    pub(crate) live_progress: RunProgressSummary,
    pub(crate) execution_route_count: usize,
    pub(crate) shard_summary_count: usize,
}

impl RunStatusProjection {
    pub(crate) fn from_control_run(run: Run) -> Self {
        let live_progress = RunProgressSummary::from_control_run(&run);
        Self {
            run,
            live_progress,
            execution_route_count: 0,
            shard_summary_count: 0,
        }
    }

    pub(crate) fn progress_source(&self) -> &'static str {
        if self.shard_summary_count > 0 {
            "execution_shards"
        } else {
            "control_run"
        }
    }

    pub(crate) fn live_progress_complete(&self) -> bool {
        self.execution_route_count > 0 && self.execution_route_count == self.shard_summary_count
    }
}

pub(crate) fn combine_run_shard_progress(summaries: &[RunShardSummary]) -> RunProgressSummary {
    let mut progress = RunProgressSummary::default();

    for summary in summaries {
        progress.expected_execution_count += i64::from(summary.expected_execution_count);
        progress.execution_count += i64::from(summary.execution_count);
        progress.terminal_execution_count += i64::from(summary.terminal_execution_count);
        progress.passed_execution_count += i64::from(summary.passed_execution_count);
        progress.failed_execution_count += i64::from(summary.failed_execution_count);
        progress.errored_execution_count += i64::from(summary.errored_execution_count);
        progress.skipped_execution_count += i64::from(summary.skipped_execution_count);
        progress.missing_aggregate_count += i64::from(summary.missing_aggregate_count);
        progress.failed_chunk_count += i64::from(summary.failed_chunk_count);
        progress.cancelled_chunk_count += i64::from(summary.cancelled_chunk_count);
    }

    progress
}

/// Reads the control run row plus routed shard-summary progress for a run.
pub(crate) async fn select_run_status(
    database: &database::Db,
    run_id: Uuid,
) -> anyhow::Result<Option<RunStatusProjection>> {
    let control_db = database.control().await?;
    let Some(run) = runs::select_run_by_id(control_db, run_id).await? else {
        return Ok(None);
    };

    let routes = database.execution_read_routes_for_run(run_id).await?;
    let mut summaries = Vec::with_capacity(routes.len());

    for (run_shard, _, db) in &routes {
        if let Some(summary) = select_run_shard_summary(db, run_id, *run_shard).await? {
            summaries.push(summary);
        }
    }

    let shard_summary_count = summaries.len();
    let live_progress = if summaries.is_empty() {
        RunProgressSummary::from_control_run(&run)
    } else {
        combine_run_shard_progress(&summaries)
    };

    Ok(Some(RunStatusProjection {
        run,
        live_progress,
        execution_route_count: routes.len(),
        shard_summary_count,
    }))
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn combine_run_shard_progress_rolls_up_live_counts() {
        let run_id = Uuid::now_v7();
        let summaries = vec![summary(run_id, 0, 10, 7), summary(run_id, 1, 5, 5)];

        let progress = combine_run_shard_progress(&summaries);

        assert_eq!(progress.expected_execution_count, 15);
        assert_eq!(progress.execution_count, 12);
        assert_eq!(progress.terminal_execution_count, 12);
        assert_eq!(progress.passed_execution_count, 8);
        assert_eq!(progress.failed_execution_count, 2);
        assert_eq!(progress.errored_execution_count, 2);
        assert_eq!(progress.cancelled_chunk_count, 2);
    }

    fn summary(
        run_id: Uuid,
        run_shard: i16,
        expected_execution_count: i32,
        terminal_execution_count: i32,
    ) -> RunShardSummary {
        RunShardSummary {
            run_id,
            run_shard,
            expected_execution_count,
            execution_count: terminal_execution_count,
            terminal_execution_count,
            aggregate_count: terminal_execution_count,
            passed_execution_count: terminal_execution_count - 2,
            failed_execution_count: 1,
            errored_execution_count: 1,
            skipped_execution_count: 0,
            missing_aggregate_count: 0,
            evaluator_result_count: 0,
            blocking_failure_count: 0,
            score_count: 0,
            score_sum: 0.0,
            min_score: None,
            max_score: None,
            failed_chunk_count: 0,
            cancelled_chunk_count: 1,
            status: "running".to_string(),
        }
    }
}
