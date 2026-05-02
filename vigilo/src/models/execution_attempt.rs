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
pub(crate) struct ExecutionAttemptDraft {
    pub(crate) execution_id: Uuid,
    pub(crate) run_id: Uuid,
    pub(crate) attempt_no: i32,
    pub(crate) worker_id: Option<Uuid>,
    pub(crate) worker_host: Option<String>,
    pub(crate) queue_message_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionAttemptPatch {
    pub(crate) status: String,
    pub(crate) error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct ExecutionAttempt {
    pub(crate) id: Uuid,
    pub(crate) execution_id: Uuid,
    pub(crate) run_id: Uuid,
    pub(crate) attempt_no: i32,
    pub(crate) status: String,
    pub(crate) worker_id: Option<Uuid>,
    pub(crate) worker_host: Option<String>,
    pub(crate) queue_message_id: Option<Uuid>,
    pub(crate) leased_until: Option<String>,
    pub(crate) heartbeat_at: Option<String>,
    pub(crate) request_artifact_uri: Option<String>,
    pub(crate) response_artifact_uri: Option<String>,
    pub(crate) agent_latency_ms: Option<i64>,
    pub(crate) evaluator_latency_ms: Option<i64>,
    pub(crate) total_latency_ms: Option<i64>,
    pub(crate) token_usage: serde_json::Value,
    pub(crate) outcome_summary: serde_json::Value,
    pub(crate) error_message: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) updated_at: DateTime<Utc>,
}
