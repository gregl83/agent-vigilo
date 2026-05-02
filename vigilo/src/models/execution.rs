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
pub(crate) struct ExecutionDraft {
    pub(crate) run_id: Uuid,
    pub(crate) case_id: Uuid,
    pub(crate) task_type: String,
    pub(crate) evaluation_profile_id: String,
    pub(crate) evaluation_profile_version: String,
    pub(crate) expected_evaluator_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionPatch {
    pub(crate) status: String,
    pub(crate) current_attempt_no: i32,
    pub(crate) current_attempt_id: Option<Uuid>,
    pub(crate) error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct Execution {
    pub(crate) id: Uuid,
    pub(crate) run_id: Uuid,
    pub(crate) case_id: Uuid,
    pub(crate) task_type: String,
    pub(crate) tags: serde_json::Value,
    pub(crate) input_payload: serde_json::Value,
    pub(crate) expected_output: serde_json::Value,
    pub(crate) case_metadata: serde_json::Value,
    pub(crate) evaluation_profile_id: String,
    pub(crate) evaluation_profile_version: String,
    pub(crate) evaluator_manifest: serde_json::Value,
    pub(crate) expected_evaluator_count: i32,
    pub(crate) status: String,
    pub(crate) current_attempt_no: i32,
    pub(crate) current_attempt_id: Option<Uuid>,
    pub(crate) last_error_message: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) updated_at: DateTime<Utc>,
}
