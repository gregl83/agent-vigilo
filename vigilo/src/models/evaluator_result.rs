use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorResultDraft {
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EvaluatorResultPatch {
    pub(crate) reason: Option<String>,
    pub(crate) failure_category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct EvaluatorResult {
    pub(crate) id: Uuid,
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
    pub(crate) created_at: DateTime<Utc>,
}
