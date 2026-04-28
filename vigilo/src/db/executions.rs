use sqlx::PgPool;

use crate::models::execution::{
    Execution,
    NewExecution,
};

const SELECT_COLUMNS: &str = r#"
    id::text AS id,
    run_id::text AS run_id,
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
    status::text AS status,
    current_attempt_no,
    current_attempt_id::text AS current_attempt_id,
    last_error_message,
    created_at::text AS created_at,
    started_at::text AS started_at,
    completed_at::text AS completed_at,
    updated_at::text AS updated_at
"#;


pub(crate) async fn insert_execution(db: &PgPool, new: &NewExecution) -> anyhow::Result<Execution> {
    let execution = sqlx::query_as::<_, Execution>(&format!(
        r#"
        INSERT INTO executions (
            run_id,
            case_id,
            task_type,
            evaluation_profile_id,
            evaluation_profile_version,
            expected_evaluator_count
        )
        VALUES ($1::uuid, $2, $3, $4, $5, $6)
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(&new.run_id)
    .bind(&new.case_id)
    .bind(&new.task_type)
    .bind(&new.evaluation_profile_id)
    .bind(&new.evaluation_profile_version)
    .bind(new.expected_evaluator_count)
    .fetch_one(db)
    .await?;

    Ok(execution)
}

pub(crate) async fn select_execution_by_id(db: &PgPool, id: &str) -> anyhow::Result<Option<Execution>> {
    let execution = sqlx::query_as::<_, Execution>(&format!(
        r#"
        SELECT {}
        FROM executions
        WHERE id = $1::uuid
        "#,
        SELECT_COLUMNS
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(execution)
}

pub(crate) async fn list_executions_by_run_id(db: &PgPool, run_id: &str) -> anyhow::Result<Vec<Execution>> {
    let executions = sqlx::query_as::<_, Execution>(&format!(
        r#"
        SELECT {}
        FROM executions
        WHERE run_id = $1::uuid
        ORDER BY created_at ASC
        "#,
        SELECT_COLUMNS
    ))
    .bind(run_id)
    .fetch_all(db)
    .await?;

    Ok(executions)
}

pub(crate) async fn update_execution_status(
    db: &PgPool,
    id: &str,
    status: &str,
    current_attempt_no: i32,
    current_attempt_id: Option<&str>,
    error_message: Option<&str>,
) -> anyhow::Result<Option<Execution>> {
    let execution = sqlx::query_as::<_, Execution>(&format!(
        r#"
        UPDATE executions
        SET status = $2::execution_status,
            current_attempt_no = $3,
            current_attempt_id = $4::uuid,
            last_error_message = $5,
            updated_at = now()
        WHERE id = $1::uuid
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(id)
    .bind(status)
    .bind(current_attempt_no)
    .bind(current_attempt_id)
    .bind(error_message)
    .fetch_optional(db)
    .await?;

    Ok(execution)
}

pub(crate) async fn delete_execution_by_id(db: &PgPool, id: &str) -> anyhow::Result<u64> {
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

