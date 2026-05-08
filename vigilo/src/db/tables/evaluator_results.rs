use sqlx::PgPool;
use uuid::Uuid;

use crate::models::evaluator_result::{
    EvaluatorResult,
    EvaluatorResultDraft,
    EvaluatorResultPatch,
};

#[derive(Debug, Clone)]
pub(crate) struct EvaluatorResultInsertRow {
    pub(crate) run_id: Uuid,
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) evaluator_id: Uuid,
    pub(crate) evaluator_version: String,
    pub(crate) evaluator_profile_id: String,
    pub(crate) evaluator_profile_version: String,
    pub(crate) evaluator_interface_version: Option<String>,
    pub(crate) evaluator_runtime_version: Option<String>,
    pub(crate) dimension: String,
    pub(crate) status: String,
    pub(crate) blocking: bool,
    pub(crate) score_kind: String,
    pub(crate) raw_score: Option<f64>,
    pub(crate) raw_score_min: Option<f64>,
    pub(crate) raw_score_max: Option<f64>,
    pub(crate) normalized_score: Option<f64>,
    pub(crate) weight: f64,
    pub(crate) severity: String,
    pub(crate) failure_category: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) evidence: serde_json::Value,
    pub(crate) raw_evaluator_output: serde_json::Value,
}

pub(crate) async fn insert_evaluator_results_batch(
    db: &PgPool,
    rows: &[EvaluatorResultInsertRow],
) -> anyhow::Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }

    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"
        INSERT INTO evaluator_results (
            run_id,
            execution_id,
            attempt_id,
            evaluator_id,
            evaluator_version,
            evaluator_profile_id,
            evaluator_profile_version,
            evaluator_interface_version,
            evaluator_runtime_version,
            dimension,
            status,
            blocking,
            score_kind,
            raw_score,
            raw_score_min,
            raw_score_max,
            normalized_score,
            weight,
            severity,
            failure_category,
            reason,
            evidence,
            raw_evaluator_output
        )
        "#,
    );

    qb.push_values(rows, |mut b, row| {
        b.push_bind(row.run_id)
            .push_bind(row.execution_id)
            .push_bind(row.attempt_id)
            .push_bind(row.evaluator_id.to_string())
            .push_bind(&row.evaluator_version)
            .push_bind(&row.evaluator_profile_id)
            .push_bind(&row.evaluator_profile_version)
            .push_bind(&row.evaluator_interface_version)
            .push_bind(&row.evaluator_runtime_version)
            .push_bind(&row.dimension)
            .push_bind(&row.status)
            .push("::evaluation_status")
            .push_bind(row.blocking)
            .push_bind(&row.score_kind)
            .push_bind(row.raw_score)
            .push_bind(row.raw_score_min)
            .push_bind(row.raw_score_max)
            .push_bind(row.normalized_score)
            .push_bind(row.weight)
            .push_bind(&row.severity)
            .push("::severity")
            .push_bind(&row.failure_category)
            .push_bind(&row.reason)
            .push_bind(&row.evidence)
            .push_bind(&row.raw_evaluator_output);
    });

    let result = qb.build().execute(db).await?;
    Ok(result.rows_affected())
}

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
