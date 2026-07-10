//! Routed run export read helpers.
//!
//! Export reads execution-owned rows from the database placement that owns each
//! `run_id + run_shard` route. The CLI owns output formatting; this module owns
//! routing and SQL pagination.

use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    context::database,
    models::{
        evaluator_result::EvaluatorResult,
        execution::Execution,
        execution_aggregate::ExecutionAggregate,
        execution_attempt::ExecutionAttempt,
    },
};

#[derive(Debug)]
pub(crate) struct RunExportBatch {
    pub(crate) executions: Vec<Execution>,
    pub(crate) attempts: Vec<ExecutionAttempt>,
    pub(crate) aggregates: Vec<ExecutionAggregate>,
    pub(crate) evaluator_results: Vec<EvaluatorResult>,
}

#[derive(Debug, Clone)]
pub(crate) struct RunExportRoute {
    run_shard: i16,
    database_alias: String,
    db: PgPool,
}

impl RunExportRoute {
    pub(crate) fn run_shard(&self) -> i16 {
        self.run_shard
    }

    pub(crate) fn database_alias(&self) -> &str {
        &self.database_alias
    }
}

pub(crate) async fn select_run_export_routes(
    database: &database::Db,
    run_id: Uuid,
) -> anyhow::Result<Vec<RunExportRoute>> {
    let routes = database
        .execution_read_routes_for_run(run_id)
        .await?
        .into_iter()
        .map(|(run_shard, database_alias, db)| RunExportRoute {
            run_shard,
            database_alias,
            db,
        })
        .collect();

    Ok(routes)
}

pub(crate) async fn select_execution_batch(
    route: &RunExportRoute,
    run_id: Uuid,
    after_execution_id: Option<Uuid>,
    limit: i64,
) -> anyhow::Result<Vec<Execution>> {
    let executions = sqlx::query_as::<_, Execution>(
        r#"
        SELECT
            id,
            run_id,
            run_shard,
            chunk_id,
            case_id,
            task_type,
            tags,
            input_payload,
            expected_output,
            case_metadata,
            evaluation_profile_id,
            evaluation_profile_version,
            evaluator_manifest,
            expected_evaluator_count,
            status::text as status,
            current_attempt_no,
            current_attempt_id,
            last_error_message,
            retry_after,
            retry_count,
            last_attempt_completed_at,
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM executions
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND (
              $3::uuid IS NULL
              OR id > $3::uuid
          )
        ORDER BY id
        LIMIT $4
        "#,
    )
    .bind(run_id)
    .bind(route.run_shard)
    .bind(after_execution_id)
    .bind(limit)
    .fetch_all(&route.db)
    .await?;

    Ok(executions)
}

pub(crate) async fn select_batch_for_executions(
    route: &RunExportRoute,
    run_id: Uuid,
    executions: Vec<Execution>,
) -> anyhow::Result<RunExportBatch> {
    if executions.is_empty() {
        return Ok(RunExportBatch {
            executions,
            attempts: Vec::new(),
            aggregates: Vec::new(),
            evaluator_results: Vec::new(),
        });
    }

    let execution_ids = executions
        .iter()
        .map(|execution| execution.id)
        .collect::<Vec<_>>();

    let attempts = sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        SELECT
            ea.id,
            ea.execution_id,
            ea.run_id,
            ea.run_shard,
            ea.attempt_no,
            ea.status::text as status,
            ea.worker_id,
            ea.worker_host,
            ea.queue_message_id,
            ea.broker_message_id,
            ea.leased_until::text as leased_until,
            ea.heartbeat_at::text as heartbeat_at,
            ea.request_artifact_uri,
            ea.response_artifact_uri,
            ea.agent_latency_ms,
            ea.evaluator_latency_ms,
            ea.total_latency_ms,
            ea.token_usage,
            ea.outcome_summary,
            ea.error_message,
            ea.created_at,
            ea.started_at,
            ea.completed_at,
            ea.updated_at
        FROM execution_attempts ea
        WHERE ea.run_id = $1::uuid
          AND ea.run_shard = $2
          AND ea.execution_id = ANY($3::uuid[])
        ORDER BY ea.execution_id, ea.attempt_no, ea.id
        "#,
    )
    .bind(run_id)
    .bind(route.run_shard)
    .bind(&execution_ids)
    .fetch_all(&route.db)
    .await?;

    let aggregates = sqlx::query_as::<_, ExecutionAggregate>(
        r#"
        SELECT
            eag.execution_id,
            eag.run_id,
            eag.run_shard,
            eag.attempt_id,
            eag.overall_status::text as overall_status,
            eag.aggregate_score,
            eag.evaluator_result_count,
            eag.dimension_scores,
            eag.blocking_failures,
            eag.summary,
            eag.created_at,
            eag.updated_at
        FROM execution_aggregates eag
        WHERE eag.run_id = $1::uuid
          AND eag.run_shard = $2
          AND eag.execution_id = ANY($3::uuid[])
        ORDER BY eag.execution_id
        "#,
    )
    .bind(run_id)
    .bind(route.run_shard)
    .bind(&execution_ids)
    .fetch_all(&route.db)
    .await?;

    let attempt_ids = attempts
        .iter()
        .map(|attempt| attempt.id)
        .collect::<Vec<_>>();

    let evaluator_results = if attempt_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as::<_, EvaluatorResult>(
            r#"
            SELECT
                er.id,
                er.run_id,
                er.run_shard,
                er.execution_id,
                er.attempt_id,
                er.evaluator_id,
                er.finding_index,
                er.evaluator_version,
                er.evaluator_profile_id,
                er.evaluator_profile_version,
                er.evaluator_interface_version,
                er.evaluator_runtime_version,
                er.dimension,
                er.status::text as status,
                er.blocking,
                er.score_kind,
                er.raw_score,
                er.raw_score_min,
                er.raw_score_max,
                er.normalized_score,
                er.weight,
                er.severity::text as severity,
                er.failure_category,
                er.reason,
                er.evidence,
                er.raw_evaluator_output,
                er.created_at
            FROM evaluator_results er
            WHERE er.run_id = $1::uuid
              AND er.run_shard = $2
              AND er.attempt_id = ANY($3::uuid[])
            ORDER BY er.execution_id, er.attempt_id, er.evaluator_id, er.finding_index, er.created_at, er.id
            "#,
        )
        .bind(run_id)
        .bind(route.run_shard)
        .bind(&attempt_ids)
        .fetch_all(&route.db)
        .await?
    };

    Ok(RunExportBatch {
        executions,
        attempts,
        aggregates,
        evaluator_results,
    })
}
