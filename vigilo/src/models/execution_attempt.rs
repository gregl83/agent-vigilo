//! Execution attempt persistence models.
//!
//! Attempts represent worker-owned tries for an execution. Multiple attempts
//! can exist for retries or expired leases, while the parent execution points
//! at the current authoritative attempt.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for a worker attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionAttemptDraft {
    /// Execution this attempt belongs to.
    pub(crate) execution_id: Uuid,
    /// Parent run id.
    pub(crate) run_id: Uuid,
    /// Logical shard inherited from the parent execution.
    pub(crate) run_shard: i16,
    /// Monotonic attempt number within the execution.
    pub(crate) attempt_no: i32,
    /// Worker id that claimed this attempt, when known.
    pub(crate) worker_id: Option<Uuid>,
    /// Hostname or instance label for the worker, when known.
    pub(crate) worker_host: Option<String>,
    /// Queue message id associated with the work item, when available.
    pub(crate) queue_message_id: Option<Uuid>,
    /// Broker-provided message id or dedupe key, when available.
    pub(crate) broker_message_id: Option<String>,
}

/// Mutable attempt status fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionAttemptPatch {
    /// New attempt lifecycle status.
    pub(crate) status: String,
    /// Optional error associated with the latest state transition.
    pub(crate) error_message: Option<String>,
}

/// Persisted execution attempt row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct ExecutionAttempt {
    /// Attempt row id, unique within the run and shard key.
    pub(crate) id: Uuid,
    /// Execution this attempt belongs to.
    pub(crate) execution_id: Uuid,
    /// Parent run id.
    pub(crate) run_id: Uuid,
    /// Logical shard inherited from the parent execution.
    pub(crate) run_shard: i16,
    /// Monotonic attempt number within the execution.
    pub(crate) attempt_no: i32,
    /// Current attempt lifecycle status.
    pub(crate) status: String,
    /// Worker id that claimed this attempt, when known.
    pub(crate) worker_id: Option<Uuid>,
    /// Hostname or instance label for the worker, when known.
    pub(crate) worker_host: Option<String>,
    /// Queue message id associated with the work item, when available.
    pub(crate) queue_message_id: Option<Uuid>,
    /// Broker-provided message id or dedupe key, when available.
    pub(crate) broker_message_id: Option<String>,
    /// Lease expiration returned as text by direct table helpers.
    pub(crate) leased_until: Option<String>,
    /// Last worker heartbeat returned as text by direct table helpers.
    pub(crate) heartbeat_at: Option<String>,
    /// URI for the persisted request artifact, when stored externally.
    pub(crate) request_artifact_uri: Option<String>,
    /// URI for the persisted response artifact, when stored externally.
    pub(crate) response_artifact_uri: Option<String>,
    /// Agent call latency in milliseconds.
    pub(crate) agent_latency_ms: Option<i64>,
    /// Evaluator runtime latency in milliseconds.
    pub(crate) evaluator_latency_ms: Option<i64>,
    /// End-to-end attempt latency in milliseconds.
    pub(crate) total_latency_ms: Option<i64>,
    /// Token usage summary emitted by the agent provider.
    pub(crate) token_usage: serde_json::Value,
    /// Structured summary of attempt outcomes.
    pub(crate) outcome_summary: serde_json::Value,
    /// Latest error recorded for this attempt, if any.
    pub(crate) error_message: Option<String>,
    /// Time this attempt row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time attempt processing first started.
    pub(crate) started_at: Option<DateTime<Utc>>,
    /// Time attempt processing reached a terminal status.
    pub(crate) completed_at: Option<DateTime<Utc>>,
    /// Time this attempt row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}
