//! Execution persistence models.
//!
//! An execution is the run-local unit of work for evaluating one dataset case
//! against the target agent and selected evaluator manifest. Execution rows are
//! hash partitioned by `run_id` and carry the current attempt pointer used by
//! workers and finalization.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for one run-local case execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionDraft {
    /// Run partition key and parent run id.
    pub(crate) run_id: Uuid,
    /// Dataset case id resolved for this run.
    pub(crate) case_id: Uuid,
    /// Task type used to select compatible evaluators.
    pub(crate) task_type: String,
    /// Evaluation profile id used for this execution.
    pub(crate) evaluation_profile_id: String,
    /// Evaluation profile version used for this execution.
    pub(crate) evaluation_profile_version: String,
    /// Number of evaluator results expected for a complete attempt.
    pub(crate) expected_evaluator_count: i32,
}

/// Mutable execution state and current-attempt pointer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionPatch {
    /// New execution lifecycle status.
    pub(crate) status: String,
    /// Attempt number currently considered active or authoritative.
    pub(crate) current_attempt_no: i32,
    /// Attempt id currently considered active or authoritative.
    pub(crate) current_attempt_id: Option<Uuid>,
    /// Optional error associated with the latest state transition.
    pub(crate) error_message: Option<String>,
}

/// Persisted execution row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct Execution {
    /// Execution row id, unique within the run partition key.
    pub(crate) id: Uuid,
    /// Run partition key and parent run id.
    pub(crate) run_id: Uuid,
    /// Dataset case id resolved for this run.
    pub(crate) case_id: Uuid,
    /// Task type used to select compatible evaluators.
    pub(crate) task_type: String,
    /// Case tags copied from the case blob for run-local filtering/reporting.
    pub(crate) tags: serde_json::Value,
    /// Model input payload copied from the case blob.
    pub(crate) input_payload: serde_json::Value,
    /// Expected output copied from the case blob.
    pub(crate) expected_output: serde_json::Value,
    /// Case metadata copied from the case blob.
    pub(crate) case_metadata: serde_json::Value,
    /// Evaluation profile id used for this execution.
    pub(crate) evaluation_profile_id: String,
    /// Evaluation profile version used for this execution.
    pub(crate) evaluation_profile_version: String,
    /// Evaluator selection manifest materialized for this execution.
    pub(crate) evaluator_manifest: serde_json::Value,
    /// Number of evaluator results expected for a complete attempt.
    pub(crate) expected_evaluator_count: i32,
    /// Current execution lifecycle status.
    pub(crate) status: String,
    /// Attempt number currently considered active or authoritative.
    pub(crate) current_attempt_no: i32,
    /// Attempt id currently considered active or authoritative.
    pub(crate) current_attempt_id: Option<Uuid>,
    /// Latest error recorded for this execution, if any.
    pub(crate) last_error_message: Option<String>,
    /// Earliest timestamp when retry-scheduled work may run again.
    pub(crate) retry_after: Option<DateTime<Utc>>,
    /// Number of retry transitions scheduled after failed attempts.
    pub(crate) retry_count: i32,
    /// Completion time for the latest authoritative attempt.
    pub(crate) last_attempt_completed_at: Option<DateTime<Utc>>,
    /// Time this execution row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time execution processing first started.
    pub(crate) started_at: Option<DateTime<Utc>>,
    /// Time execution processing reached a terminal status.
    pub(crate) completed_at: Option<DateTime<Utc>>,
    /// Time this execution row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}
