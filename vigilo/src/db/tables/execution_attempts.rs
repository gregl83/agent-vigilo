//! Execution attempt table access.
//!
//! Attempts represent worker-owned tries for an execution. Workflow code owns
//! authoritative attempt transitions; this module provides direct row helpers
//! for simple CRUD-style access.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::execution_attempt::{
    ExecutionAttempt,
    ExecutionAttemptDraft,
    ExecutionAttemptPatch,
};

/// Inserts a new execution attempt row.
pub(crate) async fn insert_execution_attempt(
    db: &PgPool,
    draft: &ExecutionAttemptDraft,
) -> anyhow::Result<ExecutionAttempt> {
    let attempt = sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        INSERT INTO execution_attempts (
            execution_id, run_id, attempt_no,
            worker_id, worker_host, queue_message_id
        )
        VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6)
        RETURNING
            id,
            execution_id,
            run_id,
            attempt_no,
            status::text as status,
            worker_id::uuid as worker_id,
            worker_host,
            queue_message_id::uuid as queue_message_id,
            leased_until::text as leased_until,
            heartbeat_at::text as heartbeat_at,
            request_artifact_uri,
            response_artifact_uri,
            agent_latency_ms,
            evaluator_latency_ms,
            total_latency_ms,
            token_usage,
            outcome_summary,
            error_message,
            created_at,
            started_at,
            completed_at,
            updated_at
        "#,
    )
    .bind(draft.execution_id)
    .bind(draft.run_id)
    .bind(draft.attempt_no)
    .bind(draft.worker_id.map(|v| v.to_string()))
    .bind(&draft.worker_host)
    .bind(draft.queue_message_id.map(|v| v.to_string()))
    .fetch_one(db)
    .await?;

    Ok(attempt)
}

/// Finds an execution attempt by run-local primary key.
pub(crate) async fn select_execution_attempt_by_id(
    db: &PgPool,
    run_id: Uuid,
    id: Uuid,
) -> anyhow::Result<Option<ExecutionAttempt>> {
    let attempt = sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        SELECT
            id,
            execution_id,
            run_id,
            attempt_no,
            status::text as status,
            worker_id::uuid as worker_id,
            worker_host,
            queue_message_id::uuid as queue_message_id,
            leased_until::text as leased_until,
            heartbeat_at::text as heartbeat_at,
            request_artifact_uri,
            response_artifact_uri,
            agent_latency_ms,
            evaluator_latency_ms,
            total_latency_ms,
            token_usage,
            outcome_summary,
            error_message,
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM execution_attempts
        WHERE run_id = $1::uuid
          AND id = $2::uuid
        "#,
    )
    .bind(run_id)
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(attempt)
}

/// Lists attempts for an execution in attempt order.
pub(crate) async fn list_execution_attempts_by_execution_id(
    db: &PgPool,
    run_id: Uuid,
    execution_id: Uuid,
) -> anyhow::Result<Vec<ExecutionAttempt>> {
    let attempts = sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        SELECT
            id,
            execution_id,
            run_id,
            attempt_no,
            status::text as status,
            worker_id::uuid as worker_id,
            worker_host,
            queue_message_id::uuid as queue_message_id,
            leased_until::text as leased_until,
            heartbeat_at::text as heartbeat_at,
            request_artifact_uri,
            response_artifact_uri,
            agent_latency_ms,
            evaluator_latency_ms,
            total_latency_ms,
            token_usage,
            outcome_summary,
            error_message,
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM execution_attempts
        WHERE run_id = $1::uuid
          AND execution_id = $2::uuid
        ORDER BY attempt_no ASC
        "#,
    )
    .bind(run_id)
    .bind(execution_id)
    .fetch_all(db)
    .await?;

    Ok(attempts)
}

/// Lists all attempts for a run ordered by execution and attempt number.
pub(crate) async fn list_execution_attempts_by_run_id(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Vec<ExecutionAttempt>> {
    let attempts = sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        SELECT
            id,
            execution_id,
            run_id,
            attempt_no,
            status::text as status,
            worker_id::uuid as worker_id,
            worker_host,
            queue_message_id::uuid as queue_message_id,
            leased_until::text as leased_until,
            heartbeat_at::text as heartbeat_at,
            request_artifact_uri,
            response_artifact_uri,
            agent_latency_ms,
            evaluator_latency_ms,
            total_latency_ms,
            token_usage,
            outcome_summary,
            error_message,
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM execution_attempts
        WHERE run_id = $1::uuid
        ORDER BY execution_id ASC, attempt_no ASC
        "#,
    )
    .bind(run_id)
    .fetch_all(db)
    .await?;

    Ok(attempts)
}

/// Updates the status and error message for an execution attempt.
pub(crate) async fn update_execution_attempt_status(
    db: &PgPool,
    run_id: Uuid,
    id: Uuid,
    patch: &ExecutionAttemptPatch,
) -> anyhow::Result<Option<ExecutionAttempt>> {
    let attempt = sqlx::query_as::<_, ExecutionAttempt>(
        r#"
        UPDATE execution_attempts
        SET status = $3::attempt_status,
            error_message = $4,
            updated_at = now()
        WHERE run_id = $1::uuid
          AND id = $2::uuid
        RETURNING
            id,
            execution_id,
            run_id,
            attempt_no,
            status::text as status,
            worker_id::uuid as worker_id,
            worker_host,
            queue_message_id::uuid as queue_message_id,
            leased_until::text as leased_until,
            heartbeat_at::text as heartbeat_at,
            request_artifact_uri,
            response_artifact_uri,
            agent_latency_ms,
            evaluator_latency_ms,
            total_latency_ms,
            token_usage,
            outcome_summary,
            error_message,
            created_at,
            started_at,
            completed_at,
            updated_at
        "#,
    )
    .bind(run_id)
    .bind(id)
    .bind(&patch.status)
    .bind(&patch.error_message)
    .fetch_optional(db)
    .await?;

    Ok(attempt)
}

/// Deletes an execution attempt by run-local primary key.
pub(crate) async fn delete_execution_attempt_by_id(
    db: &PgPool,
    run_id: Uuid,
    id: Uuid,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        DELETE FROM execution_attempts
        WHERE run_id = $1::uuid
          AND id = $2::uuid
        "#,
    )
    .bind(run_id)
    .bind(id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}
