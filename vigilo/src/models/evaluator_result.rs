use sqlx::types::JsonValue;

#[derive(Debug, Clone)]
pub(crate) struct EvaluatorResultDraft {
    pub(crate) run_id: String,
    pub(crate) execution_id: String,
    pub(crate) attempt_id: String,
    pub(crate) evaluator_id: String,
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

#[derive(Debug, Clone)]
pub(crate) struct EvaluatorResultPatch {
    pub(crate) reason: Option<String>,
    pub(crate) failure_category: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct EvaluatorResult {
    pub(crate) id: String,
    pub(crate) run_id: String,
    pub(crate) execution_id: String,
    pub(crate) attempt_id: String,
    pub(crate) evaluator_id: String,
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
    pub(crate) evidence: JsonValue,
    pub(crate) raw_evaluator_output: JsonValue,
    pub(crate) created_at: String,
}
