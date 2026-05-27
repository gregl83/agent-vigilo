//! Evaluator result persistence models.
//!
//! Evaluator results are append-oriented evidence rows produced when an
//! evaluator scores an execution attempt. They are shard-partitioned in the
//! database and feed execution aggregates, run summaries, and gate decisions.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for one evaluator outcome on an execution attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorResultDraft {
    /// Parent run id.
    pub(crate) run_id: Uuid,
    /// Logical shard inherited from the parent execution.
    pub(crate) run_shard: i16,
    /// Execution being evaluated.
    pub(crate) execution_id: Uuid,
    /// Attempt that produced the agent output being evaluated.
    pub(crate) attempt_id: Uuid,
    /// Evaluator catalog id.
    pub(crate) evaluator_id: Uuid,
    /// Evaluator version captured at execution time.
    pub(crate) evaluator_version: String,
    /// Evaluation profile id that selected this evaluator.
    pub(crate) evaluator_profile_id: String,
    /// Evaluation profile version that selected this evaluator.
    pub(crate) evaluator_profile_version: String,
    /// Optional evaluator interface version used by the runtime.
    pub(crate) evaluator_interface_version: Option<String>,
    /// Optional evaluator runtime version used by the host.
    pub(crate) evaluator_runtime_version: Option<String>,
    /// Scoring dimension emitted by the evaluator.
    pub(crate) dimension: String,
    /// Evaluator outcome status.
    pub(crate) status: String,
    /// Whether a failing result should block the execution/run gate.
    pub(crate) blocking: bool,
    /// Score representation emitted by the evaluator.
    pub(crate) score_kind: String,
    /// Raw score before normalization, when applicable.
    pub(crate) raw_score: Option<f64>,
    /// Lower bound for raw score normalization.
    pub(crate) raw_score_min: Option<f64>,
    /// Upper bound for raw score normalization.
    pub(crate) raw_score_max: Option<f64>,
    /// Score normalized to the system's comparable scale.
    pub(crate) normalized_score: Option<f64>,
    /// Weight applied when aggregating this result.
    pub(crate) weight: f64,
    /// Severity associated with a failing or degraded result.
    pub(crate) severity: String,
    /// Optional category for failed results.
    pub(crate) failure_category: Option<String>,
    /// Human-readable evaluator explanation.
    pub(crate) reason: Option<String>,
}

/// Mutable result explanation fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorResultPatch {
    /// Updated human-readable evaluator explanation.
    pub(crate) reason: Option<String>,
    /// Updated category for failed results.
    pub(crate) failure_category: Option<String>,
}

/// Persisted evaluator result row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct EvaluatorResult {
    /// Result row id, unique within the run and shard key.
    pub(crate) id: Uuid,
    /// Parent run id.
    pub(crate) run_id: Uuid,
    /// Logical shard inherited from the parent execution.
    pub(crate) run_shard: i16,
    /// Execution being evaluated.
    pub(crate) execution_id: Uuid,
    /// Attempt that produced the agent output being evaluated.
    pub(crate) attempt_id: Uuid,
    /// Evaluator catalog id.
    pub(crate) evaluator_id: Uuid,
    /// Evaluator version captured at execution time.
    pub(crate) evaluator_version: String,
    /// Evaluation profile id that selected this evaluator.
    pub(crate) evaluator_profile_id: String,
    /// Evaluation profile version that selected this evaluator.
    pub(crate) evaluator_profile_version: String,
    /// Optional evaluator interface version used by the runtime.
    pub(crate) evaluator_interface_version: Option<String>,
    /// Optional evaluator runtime version used by the host.
    pub(crate) evaluator_runtime_version: Option<String>,
    /// Scoring dimension emitted by the evaluator.
    pub(crate) dimension: String,
    /// Evaluator outcome status.
    pub(crate) status: String,
    /// Whether a failing result should block the execution/run gate.
    pub(crate) blocking: bool,
    /// Score representation emitted by the evaluator.
    pub(crate) score_kind: String,
    /// Raw score before normalization, when applicable.
    pub(crate) raw_score: Option<f64>,
    /// Lower bound for raw score normalization.
    pub(crate) raw_score_min: Option<f64>,
    /// Upper bound for raw score normalization.
    pub(crate) raw_score_max: Option<f64>,
    /// Score normalized to the system's comparable scale.
    pub(crate) normalized_score: Option<f64>,
    /// Weight applied when aggregating this result.
    pub(crate) weight: f64,
    /// Severity associated with a failing or degraded result.
    pub(crate) severity: String,
    /// Optional category for failed results.
    pub(crate) failure_category: Option<String>,
    /// Human-readable evaluator explanation.
    pub(crate) reason: Option<String>,
    /// Structured evidence extracted from evaluator output.
    pub(crate) evidence: serde_json::Value,
    /// Full evaluator output captured for audit/debugging.
    pub(crate) raw_evaluator_output: serde_json::Value,
    /// Time the result was inserted.
    pub(crate) created_at: DateTime<Utc>,
}
