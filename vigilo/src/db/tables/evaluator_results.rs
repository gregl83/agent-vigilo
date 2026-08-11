//! Batched persistence for evaluator invocations and diagnostic evidence.

use sqlx::{
    Postgres,
    QueryBuilder,
    Transaction,
};
use uuid::Uuid;

const BATCH_CHUNK_SIZE: usize = 500;

#[derive(Debug, Clone)]
pub(crate) struct EvaluatorDiagnosticInsertRow {
    pub(crate) diagnostic_index: i32,
    pub(crate) severity: String,
    pub(crate) category: String,
    pub(crate) reason: Option<String>,
    pub(crate) evidence: serde_json::Value,
    pub(crate) tags: Vec<String>,
}

/// One evaluator invocation plus its non-authoritative diagnostics.
#[derive(Debug, Clone)]
pub(crate) struct EvaluatorResultInsertRow {
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) execution_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) binding_id: String,
    pub(crate) evaluator_id: Uuid,
    pub(crate) evaluator_version: String,
    pub(crate) evaluator_profile_id: String,
    pub(crate) evaluator_profile_version: String,
    pub(crate) evaluator_interface_version: Option<String>,
    pub(crate) evaluator_runtime_version: Option<String>,
    pub(crate) dimension: String,
    pub(crate) outcome: String,
    pub(crate) judgment: Option<String>,
    pub(crate) blocking: bool,
    pub(crate) measurement_kind: Option<String>,
    pub(crate) raw_score: Option<f64>,
    pub(crate) raw_score_min: Option<f64>,
    pub(crate) raw_score_max: Option<f64>,
    pub(crate) normalized_score: Option<f64>,
    pub(crate) pass_threshold: f64,
    pub(crate) weight: f64,
    pub(crate) error_code: Option<String>,
    pub(crate) error_message: Option<String>,
    pub(crate) abstention_category: Option<String>,
    pub(crate) abstention_reason: Option<String>,
    pub(crate) raw_evaluator_output: serde_json::Value,
    pub(crate) diagnostics: Vec<EvaluatorDiagnosticInsertRow>,
}

/// Inserts invocation rows first, then diagnostics joined through the stable
/// `(attempt_id, binding_id)` idempotency key in the same transaction.
pub(crate) async fn insert_evaluator_results_batch(
    tx: &mut Transaction<'_, Postgres>,
    rows: &[EvaluatorResultInsertRow],
) -> anyhow::Result<u64> {
    let mut inserted = 0;
    for chunk in rows.chunks(BATCH_CHUNK_SIZE) {
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO evaluator_results (run_id, run_shard, execution_id, attempt_id, binding_id, evaluator_id, evaluator_version, evaluator_profile_id, evaluator_profile_version, evaluator_interface_version, evaluator_runtime_version, dimension, outcome, judgment, blocking, measurement_kind, raw_score, raw_score_min, raw_score_max, normalized_score, pass_threshold, weight, error_code, error_message, abstention_category, abstention_reason, raw_evaluator_output) ",
        );
        query.push_values(chunk, |mut b, row| {
            b.push_bind(row.run_id)
                .push_bind(row.run_shard)
                .push_bind(row.execution_id)
                .push_bind(row.attempt_id)
                .push_bind(&row.binding_id)
                .push_bind(row.evaluator_id)
                .push_bind(&row.evaluator_version)
                .push_bind(&row.evaluator_profile_id)
                .push_bind(&row.evaluator_profile_version)
                .push_bind(&row.evaluator_interface_version)
                .push_bind(&row.evaluator_runtime_version)
                .push_bind(&row.dimension)
                .push_bind(&row.outcome)
                .push_unseparated("::evaluator_outcome")
                .push_bind(&row.judgment)
                .push_unseparated("::evaluation_status")
                .push_bind(row.blocking)
                .push_bind(&row.measurement_kind)
                .push_bind(row.raw_score)
                .push_bind(row.raw_score_min)
                .push_bind(row.raw_score_max)
                .push_bind(row.normalized_score)
                .push_bind(row.pass_threshold)
                .push_bind(row.weight)
                .push_bind(&row.error_code)
                .push_bind(&row.error_message)
                .push_bind(&row.abstention_category)
                .push_bind(&row.abstention_reason)
                .push_bind(&row.raw_evaluator_output);
        });
        query.push(" ON CONFLICT (run_id, run_shard, attempt_id, binding_id) DO NOTHING");
        inserted += query.build().execute(&mut **tx).await?.rows_affected();
    }

    let diagnostics = rows
        .iter()
        .flat_map(|row| {
            row.diagnostics
                .iter()
                .map(move |diagnostic| (row, diagnostic))
        })
        .collect::<Vec<_>>();
    for chunk in diagnostics.chunks(BATCH_CHUNK_SIZE) {
        let mut query = QueryBuilder::<Postgres>::new(
            "WITH input (run_id, run_shard, attempt_id, binding_id, diagnostic_index, severity, category, reason, evidence, tags) AS (",
        );
        query.push_values(chunk, |mut b, (row, diagnostic)| {
            b.push_bind(row.run_id)
                .push_bind(row.run_shard)
                .push_bind(row.attempt_id)
                .push_bind(&row.binding_id)
                .push_bind(diagnostic.diagnostic_index)
                .push_bind(&diagnostic.severity)
                .push_bind(&diagnostic.category)
                .push_bind(&diagnostic.reason)
                .push_bind(&diagnostic.evidence)
                .push_bind(&diagnostic.tags);
        });
        query.push(
            ") INSERT INTO evaluator_diagnostics (run_id, run_shard, evaluator_result_id, diagnostic_index, severity, category, reason, evidence, tags) SELECT i.run_id, i.run_shard, er.id, i.diagnostic_index, i.severity::severity, i.category, i.reason, i.evidence, i.tags FROM input i JOIN evaluator_results er ON er.run_id = i.run_id AND er.run_shard = i.run_shard AND er.attempt_id = i.attempt_id AND er.binding_id = i.binding_id ON CONFLICT (run_id, run_shard, evaluator_result_id, diagnostic_index) DO NOTHING",
        );
        query.build().execute(&mut **tx).await?;
    }

    Ok(inserted)
}
