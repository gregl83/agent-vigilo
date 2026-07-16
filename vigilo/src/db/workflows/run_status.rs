//! Routed run status projection helpers.
//!
//! Status reads keep the control run row authoritative. Creating runs report
//! control-owned placement progress without contacting unseeded databases;
//! active runs add live progress from shard-local summaries. The workflow
//! avoids scanning execution hot tables on every status or watch poll.

use uuid::Uuid;

use super::{
    run_creation::{
        RUN_STATUS_CREATING,
        RunCreationProgress,
        select_creation_progress,
    },
    run_shard_summary::{
        RunShardSummary,
        select_run_shard_summary,
    },
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
    pub(crate) creation_progress: Option<RunCreationProgress>,
}

impl RunStatusProjection {
    pub(crate) fn from_control_run(run: Run) -> Self {
        let live_progress = RunProgressSummary::from_control_run(&run);
        Self {
            run,
            live_progress,
            execution_route_count: 0,
            shard_summary_count: 0,
            creation_progress: None,
        }
    }

    pub(crate) fn progress_source(&self) -> &'static str {
        if self.creation_progress.is_some() {
            "run_creation"
        } else if self.shard_summary_count > 0 {
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

/// Reads a control-only creation projection or routed shard-summary progress.
pub(crate) async fn select_run_status(
    database: &database::Db,
    run_id: Uuid,
) -> anyhow::Result<Option<RunStatusProjection>> {
    let control_db = database.control().await?;
    let Some(run) = runs::select_run_by_id(control_db, run_id).await? else {
        return Ok(None);
    };

    let creation_owned_status = run.status == RUN_STATUS_CREATING
        || (run.status == "failed" && run.started_at.is_none() && run.dispatched_at.is_none());
    if creation_owned_status {
        let live_progress = RunProgressSummary::from_control_run(&run);
        let creation_progress = select_creation_progress(control_db, run_id).await?;
        if run.status == RUN_STATUS_CREATING || creation_progress.placement_count > 0 {
            return Ok(Some(RunStatusProjection {
                run,
                live_progress,
                execution_route_count: 0,
                shard_summary_count: 0,
                creation_progress: Some(creation_progress),
            }));
        }
    }

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
        creation_progress: None,
    }))
}

#[cfg(test)]
mod tests {
    use tokio::sync::OnceCell;
    use uuid::Uuid;

    use super::*;
    use crate::context::database::{
        Db,
        PlacementConfig,
        new_shard_placement_cache,
    };

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

    #[sqlx::test(migrations = "../migrations")]
    #[ignore = "requires a PostgreSQL DATABASE_URL for sqlx status tests"]
    async fn creation_failure_status_does_not_resolve_unseeded_routes(pool: sqlx::PgPool) {
        let run_id = Uuid::now_v7();
        let dataset_id = Uuid::now_v7();
        let dataset_version_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO dataset_versions (dataset_version_id, dataset_id, dataset_version) VALUES ($1, $2, 'test')",
        )
        .bind(dataset_version_id)
        .bind(dataset_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO runs (
                id, run_key, dataset_id, dataset_version_id, dataset_version,
                evaluation_profile_id, evaluation_profile_version,
                profile_version_id, profile_hash, aggregation_policy_id,
                aggregation_policy_version, aggregation_policy_hash,
                agent_provider, agent_name, prompt_config_id,
                prompt_config_version, status, error_message, completed_at
            )
            VALUES (
                $1, $2, $3, $4, 'test', 'profile', '1.0.0', 'profile-version',
                'profile-hash', 'aggregation', '1.0.0', 'aggregation-hash',
                'example', 'agent', 'prompt', '1.0.0', 'failed'::run_status,
                'immutable seed mismatch', now()
            )
            "#,
        )
        .bind(run_id)
        .bind(format!("run-{run_id}"))
        .bind(dataset_id)
        .bind(dataset_version_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO database_placements (alias, database_url_env, role, status)
            VALUES ('unseeded', 'VIGILO_TEST_UNSEEDED_DATABASE_URL', 'shard', 'active')
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO shard_placements (run_id, run_shard, database_alias, status)
            VALUES ($1, 0, 'unseeded', 'active')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO run_creation_placements (
                run_id, database_alias, status, attempt_count, last_error
            )
            VALUES ($1, 'unseeded', 'failed', 1, 'immutable seed mismatch')
            "#,
        )
        .bind(run_id)
        .execute(&pool)
        .await
        .unwrap();
        let database = context_with_control_pool(pool);

        let status = select_run_status(&database, run_id).await.unwrap().unwrap();

        assert_eq!(status.execution_route_count, 0);
        assert_eq!(
            status
                .creation_progress
                .as_ref()
                .map(|progress| progress.failed_placement_count),
            Some(1)
        );
    }

    fn context_with_control_pool(pool: sqlx::PgPool) -> Db {
        let database = Db {
            uri: "postgres://injected-control-pool".to_string(),
            max_connections: 5,
            placement_config: PlacementConfig::default_single_database(),
            cell: OnceCell::new(),
            placement_catalog: OnceCell::new(),
            shard_placement_cache: new_shard_placement_cache(),
        };
        assert!(database.cell.set(pool).is_ok());
        database
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
