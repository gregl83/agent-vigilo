use sqlx::PgPool;

use crate::models::execution_attempt::{
    ExecutionAttempt,
    NewExecutionAttempt,
};

const SELECT_COLUMNS: &str = r#"
    id::text AS id,
    execution_id::text AS execution_id,
    run_id::text AS run_id,
    attempt_no,
    status::text AS status,
    worker_id,
    worker_host,
    queue_message_id,
    leased_until::text AS leased_until,
    heartbeat_at::text AS heartbeat_at,
    request_artifact_uri,
    response_artifact_uri,
    agent_latency_ms,
    evaluator_latency_ms,
    total_latency_ms,
    token_usage,
    outcome_summary,
    error_message,
    created_at::text AS created_at,
    started_at::text AS started_at,
    completed_at::text AS completed_at,
    updated_at::text AS updated_at
"#;


pub(crate) async fn insert_execution_attempt(
    db: &PgPool,
    new: &NewExecutionAttempt,
) -> anyhow::Result<ExecutionAttempt> {
    let attempt = sqlx::query_as::<_, ExecutionAttempt>(&format!(
        r#"
        INSERT INTO execution_attempts (
            execution_id,
            run_id,
            attempt_no,
            worker_id,
            worker_host,
            queue_message_id
        )
        VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6)
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(&new.execution_id)
    .bind(&new.run_id)
    .bind(new.attempt_no)
    .bind(&new.worker_id)
    .bind(&new.worker_host)
    .bind(&new.queue_message_id)
    .fetch_one(db)
    .await?;

    Ok(attempt)
}

pub(crate) async fn select_execution_attempt_by_id(
    db: &PgPool,
    id: &str,
) -> anyhow::Result<Option<ExecutionAttempt>> {
    let attempt = sqlx::query_as::<_, ExecutionAttempt>(&format!(
        r#"
        SELECT {}
        FROM execution_attempts
        WHERE id = $1::uuid
        "#,
        SELECT_COLUMNS
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(attempt)
}

pub(crate) async fn list_execution_attempts_by_execution_id(
    db: &PgPool,
    execution_id: &str,
) -> anyhow::Result<Vec<ExecutionAttempt>> {
    let attempts = sqlx::query_as::<_, ExecutionAttempt>(&format!(
        r#"
        SELECT {}
        FROM execution_attempts
        WHERE execution_id = $1::uuid
        ORDER BY attempt_no ASC
        "#,
        SELECT_COLUMNS
    ))
    .bind(execution_id)
    .fetch_all(db)
    .await?;

    Ok(attempts)
}

pub(crate) async fn update_execution_attempt_status(
    db: &PgPool,
    id: &str,
    status: &str,
    error_message: Option<&str>,
) -> anyhow::Result<Option<ExecutionAttempt>> {
    let attempt = sqlx::query_as::<_, ExecutionAttempt>(&format!(
        r#"
        UPDATE execution_attempts
        SET status = $2::attempt_status,
            error_message = $3,
            updated_at = now()
        WHERE id = $1::uuid
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(id)
    .bind(status)
    .bind(error_message)
    .fetch_optional(db)
    .await?;

    Ok(attempt)
}

pub(crate) async fn delete_execution_attempt_by_id(db: &PgPool, id: &str) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        DELETE FROM execution_attempts
        WHERE id = $1::uuid
        "#,
    )
    .bind(id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

