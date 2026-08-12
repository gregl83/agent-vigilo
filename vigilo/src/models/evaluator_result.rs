//! Evaluator invocation result and diagnostic persistence models.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Persisted result for one profile binding invocation.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct EvaluatorResult {
    pub(crate) id: Uuid,
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
    pub(crate) raw_boolean: Option<bool>,
    pub(crate) raw_numeric: Option<f64>,
    pub(crate) raw_ordinal: Option<String>,
    pub(crate) raw_unit: Option<String>,
    pub(crate) normalized_score: Option<f64>,
    pub(crate) normalization_policy_hash: String,
    pub(crate) pass_threshold: f64,
    pub(crate) weight: f64,
    pub(crate) error_code: Option<String>,
    pub(crate) error_message: Option<String>,
    pub(crate) abstention_category: Option<String>,
    pub(crate) abstention_reason: Option<String>,
    pub(crate) raw_evaluator_output: serde_json::Value,
    pub(crate) created_at: DateTime<Utc>,
}

/// Persisted non-authoritative diagnostic attached to an evaluator result.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct EvaluatorDiagnostic {
    pub(crate) id: Uuid,
    pub(crate) run_id: Uuid,
    pub(crate) run_shard: i16,
    pub(crate) evaluator_result_id: Uuid,
    pub(crate) diagnostic_index: i32,
    pub(crate) severity: String,
    pub(crate) category: String,
    pub(crate) reason: Option<String>,
    pub(crate) evidence: serde_json::Value,
    pub(crate) tags: Vec<String>,
    pub(crate) created_at: DateTime<Utc>,
}
