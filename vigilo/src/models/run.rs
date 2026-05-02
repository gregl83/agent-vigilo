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
pub(crate) struct RunDraft {
    pub(crate) run_key: String,
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) dataset_id: String,
    pub(crate) dataset_version: String,
    pub(crate) evaluation_profile_id: String,
    pub(crate) evaluation_profile_version: String,
    pub(crate) aggregation_policy_id: String,
    pub(crate) aggregation_policy_version: String,
    pub(crate) agent_provider: String,
    pub(crate) agent_name: String,
    pub(crate) agent_version: Option<String>,
    pub(crate) prompt_config_id: String,
    pub(crate) prompt_config_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunPatch {
    pub(crate) status: String,
    pub(crate) gate_status: String,
    pub(crate) error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct Run {
    pub(crate) id: Uuid,
    pub(crate) run_key: String,
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) dataset_id: Uuid,
    pub(crate) dataset_version: String,
    pub(crate) evaluation_profile_id: String,
    pub(crate) evaluation_profile_version: String,
    pub(crate) aggregation_policy_id: String,
    pub(crate) aggregation_policy_version: String,
    pub(crate) agent_provider: String,
    pub(crate) agent_name: String,
    pub(crate) agent_version: Option<String>,
    pub(crate) prompt_config_id: String,
    pub(crate) prompt_config_version: String,
    pub(crate) config_snapshot: serde_json::Value,
    pub(crate) status: String,
    pub(crate) gate_status: String,
    pub(crate) coordinator_id: Option<Uuid>,
    pub(crate) coordinator_leased_until: Option<DateTime<Utc>>,
    pub(crate) coordinator_heartbeat_at: Option<DateTime<Utc>>,
    pub(crate) expected_execution_count: i32,
    pub(crate) terminal_execution_count: i32,
    pub(crate) passed_execution_count: i32,
    pub(crate) failed_execution_count: i32,
    pub(crate) errored_execution_count: i32,
    pub(crate) summary: serde_json::Value,
    pub(crate) error_message: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) dispatched_at: Option<DateTime<Utc>>,
    pub(crate) finalized_at: Option<DateTime<Utc>>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) updated_at: DateTime<Utc>,
}
