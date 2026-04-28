use sqlx::types::JsonValue;

#[derive(Debug, Clone)]
pub(crate) struct ExecutionDraft {
    pub(crate) run_id: String,
    pub(crate) case_id: String,
    pub(crate) task_type: String,
    pub(crate) evaluation_profile_id: String,
    pub(crate) evaluation_profile_version: String,
    pub(crate) expected_evaluator_count: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutionPatch {
    pub(crate) status: String,
    pub(crate) current_attempt_no: i32,
    pub(crate) current_attempt_id: Option<String>,
    pub(crate) error_message: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct Execution {
    pub(crate) id: String,
    pub(crate) run_id: String,
    pub(crate) case_id: String,
    pub(crate) task_type: String,
    pub(crate) tags: JsonValue,
    pub(crate) input_payload: JsonValue,
    pub(crate) expected_output: JsonValue,
    pub(crate) case_metadata: JsonValue,
    pub(crate) evaluation_profile_id: String,
    pub(crate) evaluation_profile_version: String,
    pub(crate) evaluator_manifest: JsonValue,
    pub(crate) expected_evaluator_count: i32,
    pub(crate) status: String,
    pub(crate) current_attempt_no: i32,
    pub(crate) current_attempt_id: Option<String>,
    pub(crate) last_error_message: Option<String>,
    pub(crate) created_at: String,
    pub(crate) started_at: Option<String>,
    pub(crate) completed_at: Option<String>,
    pub(crate) updated_at: String,
}
