use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;


#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionAggregateDraft {
    pub(crate) execution_id: Uuid,
    pub(crate) run_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) overall_status: String,
    pub(crate) aggregate_score: Option<f64>,
    pub(crate) evaluator_result_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionAggregatePatch {
    pub(crate) overall_status: String,
    pub(crate) aggregate_score: Option<f64>,
    pub(crate) evaluator_result_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct ExecutionAggregate {
    pub(crate) execution_id: Uuid,
    pub(crate) run_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) overall_status: String,
    pub(crate) aggregate_score: Option<f64>,
    pub(crate) evaluator_result_count: i32,
    pub(crate) dimension_scores: serde_json::Value,
    pub(crate) blocking_failures: serde_json::Value,
    pub(crate) summary: serde_json::Value,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}
