//! Run persistence models.
//!
//! A run is the top-level evaluation job. It captures immutable configuration
//! snapshots, coordinator ownership, materialized execution counters, and the
//! final gate decision used by CLI gating and integrations.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for a new run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunDraft {
    /// Stable external key for idempotent creation and integrations.
    pub(crate) run_key: String,
    /// Optional human-readable run name.
    pub(crate) name: Option<String>,
    /// Optional run description.
    pub(crate) description: Option<String>,
    /// Dataset id being evaluated.
    pub(crate) dataset_id: Uuid,
    /// Dataset version label from the run contract.
    pub(crate) dataset_version: String,
    /// Stable internal dataset-version id.
    pub(crate) dataset_version_id: Uuid,
    /// Evaluation profile id used for the run.
    pub(crate) evaluation_profile_id: String,
    /// Evaluation profile version used for the run.
    pub(crate) evaluation_profile_version: String,
    /// Stable internal profile-version id.
    pub(crate) profile_version_id: String,
    /// Hash of the normalized evaluation profile.
    pub(crate) profile_hash: String,
    /// Aggregation policy id used for scoring and gate decisions.
    pub(crate) aggregation_policy_id: String,
    /// Aggregation policy version used for scoring and gate decisions.
    pub(crate) aggregation_policy_version: String,
    /// Hash of the normalized aggregation policy.
    pub(crate) aggregation_policy_hash: String,
    /// Provider for the agent under evaluation.
    pub(crate) agent_provider: String,
    /// Agent name under evaluation.
    pub(crate) agent_name: String,
    /// Optional agent version under evaluation.
    pub(crate) agent_version: Option<String>,
    /// Prompt configuration id used by the run.
    pub(crate) prompt_config_id: String,
    /// Prompt configuration version used by the run.
    pub(crate) prompt_config_version: String,
    /// Compact JSON snapshot of run inputs needed for reproducibility.
    pub(crate) config_snapshot: serde_json::Value,
    /// Number of executions expected after run planning.
    pub(crate) expected_execution_count: i32,
}

/// Mutable run status fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunPatch {
    /// New run lifecycle status.
    pub(crate) status: String,
    /// New gate status derived from aggregate results.
    pub(crate) gate_status: String,
    /// Optional error associated with the latest state transition.
    pub(crate) error_message: Option<String>,
}

/// Persisted run row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct Run {
    /// Run id and partition key for run-owned child tables.
    pub(crate) id: Uuid,
    /// Stable external key for idempotent creation and integrations.
    pub(crate) run_key: String,
    /// Optional human-readable run name.
    pub(crate) name: Option<String>,
    /// Optional run description.
    pub(crate) description: Option<String>,
    /// Dataset id being evaluated.
    pub(crate) dataset_id: Uuid,
    /// Dataset version label from the run contract.
    pub(crate) dataset_version: String,
    /// Evaluation profile id used for the run.
    pub(crate) evaluation_profile_id: String,
    /// Evaluation profile version used for the run.
    pub(crate) evaluation_profile_version: String,
    /// Aggregation policy id used for scoring and gate decisions.
    pub(crate) aggregation_policy_id: String,
    /// Aggregation policy version used for scoring and gate decisions.
    pub(crate) aggregation_policy_version: String,
    /// Provider for the agent under evaluation.
    pub(crate) agent_provider: String,
    /// Agent name under evaluation.
    pub(crate) agent_name: String,
    /// Optional agent version under evaluation.
    pub(crate) agent_version: Option<String>,
    /// Prompt configuration id used by the run.
    pub(crate) prompt_config_id: String,
    /// Prompt configuration version used by the run.
    pub(crate) prompt_config_version: String,
    /// Compact JSON snapshot of run inputs needed for reproducibility.
    pub(crate) config_snapshot: serde_json::Value,
    /// Current run lifecycle status.
    pub(crate) status: String,
    /// Current gate status derived from aggregate results.
    pub(crate) gate_status: String,
    /// Coordinator currently leasing this run, when leased.
    pub(crate) coordinator_id: Option<Uuid>,
    /// Expiration time for the coordinator lease.
    pub(crate) coordinator_leased_until: Option<DateTime<Utc>>,
    /// Last heartbeat from the coordinator that owns the lease.
    pub(crate) coordinator_heartbeat_at: Option<DateTime<Utc>>,
    /// Number of executions expected after run planning.
    pub(crate) expected_execution_count: i32,
    /// Number of executions that reached a terminal status.
    pub(crate) terminal_execution_count: i32,
    /// Number of terminal executions that passed.
    pub(crate) passed_execution_count: i32,
    /// Number of terminal executions that failed policy/evaluator checks.
    pub(crate) failed_execution_count: i32,
    /// Number of terminal executions that errored before evaluation completed.
    pub(crate) errored_execution_count: i32,
    /// Materialized run summary produced during finalization.
    pub(crate) summary: serde_json::Value,
    /// Latest run-level error, if any.
    pub(crate) error_message: Option<String>,
    /// Time this run row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time run processing first started.
    pub(crate) started_at: Option<DateTime<Utc>>,
    /// Time run chunks were dispatched for worker processing.
    pub(crate) dispatched_at: Option<DateTime<Utc>>,
    /// Time finalization completed aggregate rollups.
    pub(crate) finalized_at: Option<DateTime<Utc>>,
    /// Time the run reached a terminal lifecycle status.
    pub(crate) completed_at: Option<DateTime<Utc>>,
    /// Time this run row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}
