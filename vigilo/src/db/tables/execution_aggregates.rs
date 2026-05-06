use sqlx::PgPool;
use uuid::Uuid;

use crate::models::execution_aggregate::{
    ExecutionAggregate,
    ExecutionAggregateDraft,
    ExecutionAggregatePatch,
};

pub(crate) async fn insert_execution_aggregate(
    db: &PgPool,
    draft: &ExecutionAggregateDraft,
) -> anyhow::Result<ExecutionAggregate> {
    let aggregate = sqlx::query_as::<_, ExecutionAggregate>(
        r#"
        INSERT INTO execution_aggregates (
            execution_id, run_id, attempt_id,
            overall_status, aggregate_score, evaluator_result_count
        )
        VALUES ($1::uuid, $2::uuid, $3::uuid, $4::evaluation_status, $5, $6)
        RETURNING
            execution_id,
            run_id,
            attempt_id,
            overall_status::text as overall_status,
            aggregate_score,
            evaluator_result_count,
            dimension_scores,
            blocking_failures,
            summary,
            created_at,
            updated_at
        "#,
    )
    .bind(draft.execution_id)
    .bind(draft.run_id)
    .bind(draft.attempt_id)
    .bind(&draft.overall_status)
    .bind(draft.aggregate_score)
    .bind(draft.evaluator_result_count)
    .fetch_one(db)
    .await?;

    Ok(aggregate)
}

pub(crate) async fn select_execution_aggregate_by_execution_id(
    db: &PgPool,
    execution_id: Uuid,
) -> anyhow::Result<Option<ExecutionAggregate>> {
    let aggregate = sqlx::query_as::<_, ExecutionAggregate>(
        r#"
        SELECT
            execution_id,
            run_id,
            attempt_id,
            overall_status::text as overall_status,
            aggregate_score,
            evaluator_result_count,
            dimension_scores,
            blocking_failures,
            summary,
            created_at,
            updated_at
        FROM execution_aggregates
        WHERE execution_id = $1::uuid
        "#,
    )
    .bind(execution_id)
    .fetch_optional(db)
    .await?;

    Ok(aggregate)
}

pub(crate) async fn list_execution_aggregates_by_run_id(
    db: &PgPool,
    run_id: Uuid,
) -> anyhow::Result<Vec<ExecutionAggregate>> {
    let aggregates = sqlx::query_as::<_, ExecutionAggregate>(
        r#"
        SELECT
            execution_id,
            run_id,
            attempt_id,
            overall_status::text as overall_status,
            aggregate_score,
            evaluator_result_count,
            dimension_scores,
            blocking_failures,
            summary,
            created_at,
            updated_at
        FROM execution_aggregates
        WHERE run_id = $1::uuid
        ORDER BY updated_at DESC
        "#,
    )
    .bind(run_id)
    .fetch_all(db)
    .await?;

    Ok(aggregates)
}

pub(crate) async fn update_execution_aggregate(
    db: &PgPool,
    execution_id: Uuid,
    patch: &ExecutionAggregatePatch,
) -> anyhow::Result<Option<ExecutionAggregate>> {
    let aggregate = sqlx::query_as::<_, ExecutionAggregate>(
        r#"
        UPDATE execution_aggregates
        SET overall_status = $2::evaluation_status,
            aggregate_score = $3,
            evaluator_result_count = $4,
            updated_at = now()
        WHERE execution_id = $1::uuid
        RETURNING
            execution_id,
            run_id,
            attempt_id,
            overall_status::text as overall_status,
            aggregate_score,
            evaluator_result_count,
            dimension_scores,
            blocking_failures,
            summary,
            created_at,
            updated_at
        "#,
    )
    .bind(execution_id)
    .bind(&patch.overall_status)
    .bind(patch.aggregate_score)
    .bind(patch.evaluator_result_count)
    .fetch_optional(db)
    .await?;

    Ok(aggregate)
}

pub(crate) async fn delete_execution_aggregate_by_execution_id(
    db: &PgPool,
    execution_id: Uuid,
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

