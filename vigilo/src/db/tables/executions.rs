//! Execution table access.
//!
//! Executions bind a run to a dataset case and track the current authoritative
//! attempt. Complex attempt allocation and terminal transitions live in
//! workflow code so these helpers stay narrow and predictable.

use sqlx::PgPool;
use uuid::Uuid;

use crate::models::execution::{
    Execution,
    ExecutionDraft,
    ExecutionPatch,
};

/// Inserts a new execution row.
pub(crate) async fn insert_execution(
    db: &PgPool,
    draft: &ExecutionDraft,
) -> anyhow::Result<Execution> {
    let execution = sqlx::query_as::<_, Execution>(
        r#"
        INSERT INTO executions (
            run_id, run_shard, chunk_id, case_id, task_type,
            evaluation_profile_id, evaluation_profile_version,
            expected_evaluator_count
        )
        VALUES ($1::uuid, $2, $3::uuid, $4::uuid, $5, $6, $7, $8)
        RETURNING
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
        "#,
    )
    .bind(draft.run_id)
    .bind(draft.run_shard)
    .bind(draft.chunk_id)
    .bind(draft.case_id)
    .bind(&draft.task_type)
    .bind(&draft.evaluation_profile_id)
    .bind(&draft.evaluation_profile_version)
    .bind(draft.expected_evaluator_count)
    .fetch_one(db)
    .await?;

    Ok(execution)
}

/// Finds an execution by run and shard-local primary key.
pub(crate) async fn select_execution_by_id(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    id: Uuid,
) -> anyhow::Result<Option<Execution>> {
    let execution = sqlx::query_as::<_, Execution>(
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
          AND id = $3::uuid
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(execution)
}

/// Lists executions for a run in creation order.
pub(crate) async fn list_executions_by_run_id(
    db: &PgPool,
    run_id: Uuid,
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
        ORDER BY created_at ASC
        "#,
    )
    .bind(run_id)
    .fetch_all(db)
    .await?;

    Ok(executions)
}

/// Updates status and current-attempt fields for an execution.
pub(crate) async fn update_execution_status(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    id: Uuid,
    patch: &ExecutionPatch,
) -> anyhow::Result<Option<Execution>> {
    let execution = sqlx::query_as::<_, Execution>(
        r#"
        UPDATE executions
        SET status = $3::execution_status,
            current_attempt_no = $4,
            current_attempt_id = $5::uuid,
            last_error_message = $6,
            updated_at = now()
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND id = $7::uuid
        RETURNING
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
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(&patch.status)
    .bind(&patch.current_attempt_no)
    .bind(&patch.current_attempt_id)
    .bind(&patch.error_message)
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(execution)
}

/// Deletes an execution by run and shard-local primary key.
pub(crate) async fn delete_execution_by_id(
    db: &PgPool,
    run_id: Uuid,
    run_shard: i16,
    id: Uuid,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        DELETE FROM executions
        WHERE run_id = $1::uuid
          AND run_shard = $2
          AND id = $3::uuid
        "#,
    )
    .bind(run_id)
    .bind(run_shard)
    .bind(id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}
