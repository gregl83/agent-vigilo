use sqlx::types::JsonValue;

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub(crate) struct RunPatch {
    pub(crate) status: String,
    pub(crate) gate_status: String,
    pub(crate) error_message: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct Run {
    pub(crate) id: String,
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
    pub(crate) config_snapshot: JsonValue,
    pub(crate) status: String,
    pub(crate) gate_status: String,
    pub(crate) coordinator_id: Option<String>,
    pub(crate) coordinator_leased_until: Option<String>,
    pub(crate) coordinator_heartbeat_at: Option<String>,
    pub(crate) expected_execution_count: i32,
    pub(crate) terminal_execution_count: i32,
    pub(crate) passed_execution_count: i32,
    pub(crate) failed_execution_count: i32,
    pub(crate) errored_execution_count: i32,
    pub(crate) summary: JsonValue,
    pub(crate) error_message: Option<String>,
    pub(crate) created_at: String,
    pub(crate) started_at: Option<String>,
    pub(crate) dispatched_at: Option<String>,
    pub(crate) finalized_at: Option<String>,
    pub(crate) completed_at: Option<String>,
    pub(crate) updated_at: String,
}


