use sqlx::types::JsonValue;

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct ExecutionAttempt {
    pub(crate) id: String,
    pub(crate) execution_id: String,
    pub(crate) run_id: String,
    pub(crate) attempt_no: i32,
    pub(crate) status: String,
    pub(crate) worker_id: Option<String>,
    pub(crate) worker_host: Option<String>,
    pub(crate) queue_message_id: Option<String>,
    pub(crate) leased_until: Option<String>,
    pub(crate) heartbeat_at: Option<String>,
    pub(crate) request_artifact_uri: Option<String>,
    pub(crate) response_artifact_uri: Option<String>,
    pub(crate) agent_latency_ms: Option<i64>,
    pub(crate) evaluator_latency_ms: Option<i64>,
    pub(crate) total_latency_ms: Option<i64>,
    pub(crate) token_usage: JsonValue,
    pub(crate) outcome_summary: JsonValue,
    pub(crate) error_message: Option<String>,
    pub(crate) created_at: String,
    pub(crate) started_at: Option<String>,
    pub(crate) completed_at: Option<String>,
    pub(crate) updated_at: String,
}

