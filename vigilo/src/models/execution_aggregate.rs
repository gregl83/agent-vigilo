//! Execution aggregate persistence models.
//!
//! Aggregates summarize evaluator results for the authoritative attempt of an
//! execution. They are derived from evaluator result rows and used by
//! finalization to compute run counters, scores, and gate state.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for an execution aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionAggregateDraft {
    /// Execution being summarized.
    pub(crate) execution_id: Uuid,
    /// Run partition key and parent run id.
    pub(crate) run_id: Uuid,
    /// Attempt whose evaluator results produced this aggregate.
    pub(crate) attempt_id: Uuid,
    /// Overall status derived from evaluator results.
    pub(crate) overall_status: String,
    /// Weighted aggregate score for the execution, when scoreable.
    pub(crate) aggregate_score: Option<f64>,
    /// Number of evaluator results included in the aggregate.
    pub(crate) evaluator_result_count: i32,
}

/// Mutable aggregate fields recomputed from evaluator results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionAggregatePatch {
    /// Overall status derived from evaluator results.
    pub(crate) overall_status: String,
    /// Weighted aggregate score for the execution, when scoreable.
    pub(crate) aggregate_score: Option<f64>,
    /// Number of evaluator results included in the aggregate.
    pub(crate) evaluator_result_count: i32,
}

/// Persisted execution aggregate row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct ExecutionAggregate {
    /// Execution being summarized.
    pub(crate) execution_id: Uuid,
    /// Run partition key and parent run id.
    pub(crate) run_id: Uuid,
    /// Attempt whose evaluator results produced this aggregate.
    pub(crate) attempt_id: Uuid,
    /// Overall status derived from evaluator results.
    pub(crate) overall_status: String,
    /// Weighted aggregate score for the execution, when scoreable.
    pub(crate) aggregate_score: Option<f64>,
    /// Number of evaluator results included in the aggregate.
    pub(crate) evaluator_result_count: i32,
    /// Per-dimension score details.
    pub(crate) dimension_scores: serde_json::Value,
    /// Blocking evaluator failures that affected gate state.
    pub(crate) blocking_failures: serde_json::Value,
    /// Human- and API-facing aggregate summary payload.
    pub(crate) summary: serde_json::Value,
    /// Time this aggregate row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time this aggregate row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}
