use sqlx::types::JsonValue;

#[derive(Debug, Clone)]
pub(crate) struct ExecutionAggregateDraft {
    pub(crate) execution_id: String,
    pub(crate) run_id: String,
    pub(crate) attempt_id: String,
    pub(crate) overall_status: String,
    pub(crate) aggregate_score: Option<f64>,
    pub(crate) evaluator_result_count: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutionAggregatePatch {
    pub(crate) overall_status: String,
    pub(crate) aggregate_score: Option<f64>,
    pub(crate) evaluator_result_count: i32,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct ExecutionAggregate {
    pub(crate) execution_id: String,
    pub(crate) run_id: String,
    pub(crate) attempt_id: String,
    pub(crate) overall_status: String,
    pub(crate) aggregate_score: Option<f64>,
    pub(crate) evaluator_result_count: i32,
    pub(crate) dimension_scores: JsonValue,
    pub(crate) blocking_failures: JsonValue,
    pub(crate) summary: JsonValue,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

