use sqlx::PgPool;

use crate::models::execution_aggregate::{
    ExecutionAggregate,
    NewExecutionAggregate,
};

const SELECT_COLUMNS: &str = r#"
    execution_id::text AS execution_id,
    run_id::text AS run_id,
    attempt_id::text AS attempt_id,
    overall_status::text AS overall_status,
    aggregate_score,
    evaluator_result_count,
    dimension_scores,
    blocking_failures,
    summary,
    created_at::text AS created_at,
    updated_at::text AS updated_at
"#;


pub(crate) async fn insert_execution_aggregate(
    db: &PgPool,
    new: &NewExecutionAggregate,
) -> anyhow::Result<ExecutionAggregate> {
    let aggregate = sqlx::query_as::<_, ExecutionAggregate>(&format!(
        r#"
        INSERT INTO execution_aggregates (
            execution_id,
            run_id,
            attempt_id,
            overall_status,
            aggregate_score,
            evaluator_result_count
        )
        VALUES ($1::uuid, $2::uuid, $3::uuid, $4::evaluation_status, $5, $6)
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(&new.execution_id)
    .bind(&new.run_id)
    .bind(&new.attempt_id)
    .bind(&new.overall_status)
    .bind(new.aggregate_score)
    .bind(new.evaluator_result_count)
    .fetch_one(db)
    .await?;

    Ok(aggregate)
}

pub(crate) async fn select_execution_aggregate_by_execution_id(
    db: &PgPool,
    execution_id: &str,
) -> anyhow::Result<Option<ExecutionAggregate>> {
    let aggregate = sqlx::query_as::<_, ExecutionAggregate>(&format!(
        r#"
        SELECT {}
        FROM execution_aggregates
        WHERE execution_id = $1::uuid
        "#,
        SELECT_COLUMNS
    ))
    .bind(execution_id)
    .fetch_optional(db)
    .await?;

    Ok(aggregate)
}

pub(crate) async fn list_execution_aggregates_by_run_id(
    db: &PgPool,
    run_id: &str,
) -> anyhow::Result<Vec<ExecutionAggregate>> {
    let aggregates = sqlx::query_as::<_, ExecutionAggregate>(&format!(
        r#"
        SELECT {}
        FROM execution_aggregates
        WHERE run_id = $1::uuid
        ORDER BY updated_at DESC
        "#,
        SELECT_COLUMNS
    ))
    .bind(run_id)
    .fetch_all(db)
    .await?;

    Ok(aggregates)
}

pub(crate) async fn update_execution_aggregate(
    db: &PgPool,
    execution_id: &str,
    overall_status: &str,
    aggregate_score: Option<f64>,
    evaluator_result_count: i32,
) -> anyhow::Result<Option<ExecutionAggregate>> {
    let aggregate = sqlx::query_as::<_, ExecutionAggregate>(&format!(
        r#"
        UPDATE execution_aggregates
        SET overall_status = $2::evaluation_status,
            aggregate_score = $3,
            evaluator_result_count = $4,
            updated_at = now()
        WHERE execution_id = $1::uuid
        RETURNING {}
        "#,
        SELECT_COLUMNS
    ))
    .bind(execution_id)
    .bind(overall_status)
    .bind(aggregate_score)
    .bind(evaluator_result_count)
    .fetch_optional(db)
    .await?;

    Ok(aggregate)
}

pub(crate) async fn delete_execution_aggregate_by_execution_id(
    db: &PgPool,
    execution_id: &str,
) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        DELETE FROM execution_aggregates
        WHERE execution_id = $1::uuid
        "#,
    )
    .bind(execution_id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

