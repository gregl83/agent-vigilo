use sqlx::PgPool;
use uuid::Uuid;

use crate::models::execution::{
    Execution,
    ExecutionDraft,
    ExecutionPatch,
};

pub(crate) async fn insert_execution(
    db: &PgPool,
    draft: &ExecutionDraft,
) -> anyhow::Result<Execution> {
    let execution = sqlx::query_as::<_, Execution>(
        r#"
        INSERT INTO executions (
            run_id, case_id, task_type,
            evaluation_profile_id, evaluation_profile_version,
            expected_evaluator_count
        )
        VALUES ($1::uuid, $2::text, $3, $4, $5, $6)
        RETURNING
            id,
            run_id,
            case_id::uuid as case_id,
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
            created_at,
            started_at,
            completed_at,
            updated_at
        "#,
    )
    .bind(draft.run_id)
    .bind(draft.case_id.to_string())
    .bind(&draft.task_type)
    .bind(&draft.evaluation_profile_id)
    .bind(&draft.evaluation_profile_version)
    .bind(draft.expected_evaluator_count)
    .fetch_one(db)
    .await?;

    Ok(execution)
}

pub(crate) async fn select_execution_by_id(
    db: &PgPool,
    id: Uuid,
) -> anyhow::Result<Option<Execution>> {
    let execution = sqlx::query_as::<_, Execution>(
        r#"
        SELECT
            id,
            run_id,
            case_id::uuid as case_id,
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
            created_at,
            started_at,
            completed_at,
            updated_at
        FROM executions
        WHERE id = $1::uuid
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(execution)
}

pub(crate) async fn list_executions_by_run_id(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Vec<Execution>> {
    let executions = sqlx::query_as::<_, Execution>(
        r#"
        SELECT
            id,
            run_id,
            case_id::uuid as case_id,
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

pub(crate) async fn update_execution_status(
    db: &PgPool,
    id: Uuid,
    patch: &ExecutionPatch,
) -> anyhow::Result<Option<Execution>> {
    let execution = sqlx::query_as::<_, Execution>(
        r#"
        UPDATE executions
        SET status = $2::execution_status,
            current_attempt_no = $3,
            current_attempt_id = $4::uuid,
            last_error_message = $5,
            updated_at = now()
        WHERE id = $1::uuid
        RETURNING
            id,
            run_id,
            case_id::uuid as case_id,
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
            created_at,
            started_at,
            completed_at,
            updated_at
        "#,
    )
    .bind(id)
    .bind(&patch.status)
    .bind(&patch.current_attempt_no)
    .bind(&patch.current_attempt_id)
    .bind(&patch.error_message)
    .fetch_optional(db)
    .await?;

    Ok(execution)
}

pub(crate) async fn delete_execution_by_id(db: &PgPool, id: Uuid) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        DELETE FROM executions
        WHERE id = $1::uuid
        "#,
    )
    .bind(id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

