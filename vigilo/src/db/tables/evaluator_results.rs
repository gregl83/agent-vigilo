use sqlx::PgPool;
use uuid::Uuid;

use crate::models::evaluator_result::{
    EvaluatorResult,
    EvaluatorResultDraft,
    EvaluatorResultPatch,
};

pub(crate) async fn insert_evaluator_result(
    db: &PgPool,
    draft: &EvaluatorResultDraft,
) -> anyhow::Result<EvaluatorResult> {
    let result = sqlx::query_as::<_, EvaluatorResult>(
        r#"
        INSERT INTO evaluator_results (
            run_id, execution_id, attempt_id,
            evaluator_id, evaluator_version,
            evaluator_profile_id, evaluator_profile_version,
            evaluator_interface_version, evaluator_runtime_version,
            dimension, status, blocking, score_kind,
            raw_score, raw_score_min, raw_score_max,
            normalized_score, weight, severity,
            failure_category, reason
        )
        VALUES (
            $1::uuid,
            $2::uuid,
            $3::uuid,
            $4,
            $5,
            $6,
            $7,
            $8,
            $9,
            $10,
            $11::evaluation_status,
            $12,
            $13,
            $14,
            $15,
            $16,
            $17,
            $18,
            $19::severity,
            $20,
            $21
        )
        RETURNING
            id,
            run_id,
            execution_id,
            attempt_id,
            evaluator_id::uuid as evaluator_id,
            evaluator_version,
            evaluator_profile_id,
            evaluator_profile_version,
            evaluator_interface_version,
            evaluator_runtime_version,
            dimension,
            status::text as status,
            blocking,
            score_kind,
            raw_score,
            raw_score_min,
            raw_score_max,
            normalized_score,
            weight,
            severity::text as severity,
            failure_category,
            reason,
            evidence,
            raw_evaluator_output,
            created_at
        "#,
    )
    .bind(draft.run_id)
    .bind(draft.execution_id)
    .bind(draft.attempt_id)
    .bind(draft.evaluator_id.to_string())
    .bind(&draft.evaluator_version)
    .bind(&draft.evaluator_profile_id)
    .bind(&draft.evaluator_profile_version)
    .bind(&draft.evaluator_interface_version)
    .bind(&draft.evaluator_runtime_version)
    .bind(&draft.dimension)
    .bind(&draft.status)
    .bind(draft.blocking)
    .bind(&draft.score_kind)
    .bind(draft.raw_score)
    .bind(draft.raw_score_min)
    .bind(draft.raw_score_max)
    .bind(draft.normalized_score)
    .bind(draft.weight)
    .bind(&draft.severity)
    .bind(&draft.failure_category)
    .bind(&draft.reason)
    .fetch_one(db)
    .await?;

    Ok(result)
}

pub(crate) async fn select_evaluator_result_by_id(
    db: &PgPool,
    id: Uuid,
) -> anyhow::Result<Option<EvaluatorResult>> {
    let result = sqlx::query_as::<_, EvaluatorResult>(
        r#"
        SELECT
            id,
            run_id,
            execution_id,
            attempt_id,
            evaluator_id::uuid as evaluator_id,
            evaluator_version,
            evaluator_profile_id,
            evaluator_profile_version,
            evaluator_interface_version,
            evaluator_runtime_version,
            dimension,
            status::text as status,
            blocking,
            score_kind,
            raw_score,
            raw_score_min,
            raw_score_max,
            normalized_score,
            weight,
            severity::text as severity,
            failure_category,
            reason,
            evidence,
            raw_evaluator_output,
            created_at
        FROM evaluator_results
        WHERE id = $1::uuid
        "#,
    )
    .bind(id)
    .fetch_optional(db)
    .await?;

    Ok(result)
}

pub(crate) async fn list_evaluator_results_by_attempt_id(
    db: &PgPool,
    attempt_id: Uuid,
) -> anyhow::Result<Vec<EvaluatorResult>> {
    let results = sqlx::query_as::<_, EvaluatorResult>(
        r#"
        SELECT
            id,
            run_id,
            execution_id,
            attempt_id,
            evaluator_id::uuid as evaluator_id,
            evaluator_version,
            evaluator_profile_id,
            evaluator_profile_version,
            evaluator_interface_version,
            evaluator_runtime_version,
            dimension,
            status::text as status,
            blocking,
            score_kind,
            raw_score,
            raw_score_min,
            raw_score_max,
            normalized_score,
            weight,
            severity::text as severity,
            failure_category,
            reason,
            evidence,
            raw_evaluator_output,
            created_at
        FROM evaluator_results
        WHERE attempt_id = $1::uuid
        ORDER BY created_at ASC
        "#,
    )
    .bind(attempt_id)
    .fetch_all(db)
    .await?;

    Ok(results)
}

pub(crate) async fn update_evaluator_result_reason(
    db: &PgPool,
    id: Uuid,
    patch: &EvaluatorResultPatch,
) -> anyhow::Result<Option<EvaluatorResult>> {
    let result = sqlx::query_as::<_, EvaluatorResult>(
        r#"
        UPDATE evaluator_results
        SET reason = $2,
            failure_category = $3
        WHERE id = $1::uuid
        RETURNING
            id,
            run_id,
            execution_id,
            attempt_id,
            evaluator_id::uuid as evaluator_id,
            evaluator_version,
            evaluator_profile_id,
            evaluator_profile_version,
            evaluator_interface_version,
            evaluator_runtime_version,
            dimension,
            status::text as status,
            blocking,
            score_kind,
            raw_score,
            raw_score_min,
            raw_score_max,
            normalized_score,
            weight,
            severity::text as severity,
            failure_category,
            reason,
            evidence,
            raw_evaluator_output,
            created_at
        "#,
    )
    .bind(id)
    .bind(&patch.reason)
    .bind(&patch.failure_category)
    .fetch_optional(db)
    .await?;

    Ok(result)
}

pub(crate) async fn delete_evaluator_result_by_id(db: &PgPool, id: Uuid) -> anyhow::Result<u64> {
    let result = sqlx::query(
        r#"
        DELETE FROM evaluator_results
        WHERE id = $1::uuid
        "#,
    )
    .bind(id)
    .execute(db)
    .await?;

    Ok(result.rows_affected())
}

