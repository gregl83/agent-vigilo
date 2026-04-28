use sqlx::PgPool;

use crate::models::run::{
    RunDraft,
    RunPatch,
    Run,
};

const SELECT_COLUMNS: &str = r#"
    id::text AS id,
    run_key,
    name,
    description,
    dataset_id,
    dataset_version,
    evaluation_profile_id,
    evaluation_profile_version,
    aggregation_policy_id,
    aggregation_policy_version,
    agent_provider,
    agent_name,
    agent_version,
    prompt_config_id,
    prompt_config_version,
    config_snapshot,
    status::text AS status,
    gate_status::text AS gate_status,
    coordinator_id,
    coordinator_leased_until::text AS coordinator_leased_until,
    coordinator_heartbeat_at::text AS coordinator_heartbeat_at,
    expected_execution_count,
    terminal_execution_count,
    passed_execution_count,
    failed_execution_count,
    errored_execution_count,
    summary,
    error_message,
    created_at::text AS created_at,
    started_at::text AS started_at,
    dispatched_at::text AS dispatched_at,
    finalized_at::text AS finalized_at,
    completed_at::text AS completed_at,
    updated_at::text AS updated_at
"#;


pub(crate) async fn insert_run(db: &PgPool, draft: &RunDraft) -> anyhow::Result<Run> {
    let run = sqlx::query_as::<_, Run>(&format!(
        r#"
        INSERT INTO runs (
            run_key,
            name,
            description,
            dataset_id,
            dataset_version,
            evaluation_profile_id,
            evaluation_profile_version,
            aggregation_policy_id,
            aggregation_policy_version,
            agent_provider,
            agent_name,
            agent_version,
            prompt_config_id,
            prompt_config_version
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(&draft.run_key)
    .bind(&draft.name)
    .bind(&draft.description)
    .bind(&draft.dataset_id)
    .bind(&draft.dataset_version)
    .bind(&draft.evaluation_profile_id)
    .bind(&draft.evaluation_profile_version)
    .bind(&draft.aggregation_policy_id)
    .bind(&draft.aggregation_policy_version)
    .bind(&draft.agent_provider)
    .bind(&draft.agent_name)
    .bind(&draft.agent_version)
    .bind(&draft.prompt_config_id)
    .bind(&draft.prompt_config_version)
    .fetch_one(db)
    .await?;

    Ok(run)
}

pub(crate) async fn select_run_by_id(db: &PgPool, id: &str) -> anyhow::Result<Option<Run>> {
    let run = sqlx::query_as::<_, Run>(&format!(
        r#"
        SELECT {}
        FROM runs
        WHERE id = $1::uuid
        "#,
        SELECT_COLUMNS
    ))
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(run)
}

pub(crate) async fn select_run_by_key(db: &PgPool, run_key: &str) -> anyhow::Result<Option<Run>> {
    let run = sqlx::query_as::<_, Run>(&format!(
        r#"
        SELECT {}
        FROM runs
        WHERE run_key = $1
        "#,
        SELECT_COLUMNS
    ))
    .bind(run_key)
    .fetch_optional(db)
    .await?;

    Ok(run)
}

pub(crate) async fn list_runs(db: &PgPool, limit: i64, offset: i64) -> anyhow::Result<Vec<Run>> {
    let runs = sqlx::query_as::<_, Run>(&format!(
        r#"
        SELECT {}
        FROM runs
        ORDER BY created_at DESC
        LIMIT $1 OFFSET $2
        "#,
        SELECT_COLUMNS
    ))
    .bind(limit)
    .bind(offset)
    .fetch_all(db)
    .await?;

    Ok(runs)
}

pub(crate) async fn update_run_status(
    db: &PgPool,
    id: &str,
    patch: &RunPatch,
) -> anyhow::Result<Option<Run>> {
    let run = sqlx::query_as::<_, Run>(&format!(
        r#"
        UPDATE runs
        SET status = $2::run_status,
            gate_status = $3::gate_status,
            error_message = $4,
            updated_at = now()
        WHERE id = $1::uuid
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(id)
    .bind(&patch.status)
    .bind(&patch.gate_status)
    .bind(&patch.error_message)
    .fetch_optional(db)
    .await?;

    Ok(run)
}

pub(crate) async fn delete_run_by_id(db: &PgPool, id: &str) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        DELETE FROM runs
        WHERE id = $1::uuid
        "#,
    )
    .bind(id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

