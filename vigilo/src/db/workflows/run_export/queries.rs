//! PostgreSQL reads for routed run exports.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::{
    evaluator_result::EvaluatorResult,
    execution::Execution,
    execution_aggregate::ExecutionAggregate,
    execution_attempt::ExecutionAttempt,
};

pub(super) async fn select_executions(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    after_execution_id: Option<Uuid>,
    limit: i64,
) -> anyhow::Result<Vec<Execution>> {
    Ok(sqlx::query_as::<_, Execution>(
        r#"
        SELECT
            id, run_id, run_shard, chunk_id, case_id, task_type, tags,
            input_payload, expected_output, case_metadata,
            evaluation_profile_id, evaluation_profile_version,
            evaluator_manifest, expected_evaluator_count,
            status::text as status, current_attempt_no, current_attempt_id,
            last_error_message, retry_after, retry_count,
            last_attempt_completed_at, created_at, started_at, completed_at,
            updated_at
        FROM executions
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND ($3::uuid IS NULL OR id > $3::uuid)
        ORDER BY id
        LIMIT $4
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(after_execution_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

pub(super) async fn select_attempts(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    execution_ids: &[Uuid],
) -> anyhow::Result<Vec<ExecutionAttempt>> {
    Ok(sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        SELECT
            ea.id, ea.execution_id, ea.run_id, ea.run_shard, ea.attempt_no,
            ea.status::text as status, ea.worker_id, ea.worker_host,
            ea.queue_message_id, ea.broker_message_id,
            ea.leased_until::text as leased_until,
            ea.heartbeat_at::text as heartbeat_at, ea.request_artifact_uri,
            ea.response_artifact_uri, ea.agent_latency_ms,
            ea.evaluator_latency_ms, ea.total_latency_ms, ea.token_usage,
            ea.outcome_summary, ea.error_message, ea.created_at, ea.started_at,
            ea.completed_at, ea.updated_at
        FROM execution_attempts ea
        WHERE ea.run_id = $1::uuid
          AND ea.run_shard = $2
          AND ea.execution_id = ANY($3::uuid[])
        ORDER BY ea.execution_id, ea.attempt_no, ea.id
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(execution_ids)
    .fetch_all(db)
    .await?)
}

pub(super) async fn select_aggregates(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    execution_ids: &[Uuid],
) -> anyhow::Result<Vec<ExecutionAggregate>> {
    Ok(sqlx::query_as::<_, ExecutionAggregate>(
        r#"
        SELECT
            eag.execution_id, eag.run_id, eag.run_shard, eag.attempt_id,
            eag.overall_status::text as overall_status, eag.aggregate_score,
            eag.evaluator_result_count, eag.dimension_scores,
            eag.blocking_failures, eag.summary, eag.created_at, eag.updated_at
        FROM execution_aggregates eag
        WHERE eag.run_id = $1::uuid
          AND eag.run_shard = $2
          AND eag.execution_id = ANY($3::uuid[])
        ORDER BY eag.execution_id
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(execution_ids)
    .fetch_all(db)
    .await?)
}

pub(super) async fn select_evaluator_results(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    attempt_ids: &[Uuid],
) -> anyhow::Result<Vec<EvaluatorResult>> {
    Ok(sqlx::query_as::<_, EvaluatorResult>(
        r#"
        SELECT
            er.id, er.run_id, er.run_shard, er.execution_id, er.attempt_id,
            er.evaluator_id, er.finding_index, er.evaluator_version,
            er.evaluator_profile_id, er.evaluator_profile_version,
            er.evaluator_interface_version, er.evaluator_runtime_version,
            er.dimension, er.status::text as status, er.blocking,
            er.score_kind, er.raw_score, er.raw_score_min, er.raw_score_max,
            er.normalized_score, er.weight, er.severity::text as severity,
            er.failure_category, er.reason, er.evidence,
            er.raw_evaluator_output, er.created_at
        FROM evaluator_results er
        WHERE er.run_id = $1::uuid
          AND er.run_shard = $2
          AND er.attempt_id = ANY($3::uuid[])
        ORDER BY er.execution_id, er.attempt_id, er.evaluator_id,
                 er.finding_index, er.created_at, er.id
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(attempt_ids)
    .fetch_all(db)
    .await?)
}
