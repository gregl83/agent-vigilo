//! Case blob persistence models.
//!
//! Case blobs store the reusable, content-addressed payload for an evaluation
//! case. Dataset versions reference these blobs through `dataset_version_cases`
//! so identical case content can be reused across datasets or versions.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};

/// Insert payload for a content-addressed case blob.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct CaseBlobDraft {
    /// Stable hash of the normalized case content.
    pub(crate) case_hash: String,
    /// Task type used to match the case to compatible evaluator groups.
    pub(crate) task_type: String,
    /// Optional explicit profile case-group routing override.
    pub(crate) case_group: Option<String>,
    /// Model input payload provided to the agent under evaluation.
    pub(crate) input_payload: serde_json::Value,
    /// Expected output or reference answer used by evaluators.
    pub(crate) expected_output: serde_json::Value,
    /// Supplemental context supplied with the case.
    pub(crate) context_payload: serde_json::Value,
    /// Case tags stored as JSON for flexible filtering.
    pub(crate) tags: serde_json::Value,
    /// Additional case metadata stored without schema-specific columns.
    pub(crate) metadata: serde_json::Value,
}

/// Persisted case blob row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct CaseBlob {
    /// Stable hash and primary key for the normalized case content.
    pub(crate) case_hash: String,
    /// Task type used to match the case to compatible evaluator groups.
    pub(crate) task_type: String,
    /// Optional explicit profile case-group routing override.
    pub(crate) case_group: Option<String>,
    /// Model input payload provided to the agent under evaluation.
    pub(crate) input_payload: serde_json::Value,
    /// Expected output or reference answer used by evaluators.
    pub(crate) expected_output: serde_json::Value,
    /// Supplemental context supplied with the case.
    pub(crate) context_payload: serde_json::Value,
    /// Case tags stored as JSON for flexible filtering.
    pub(crate) tags: serde_json::Value,
    /// Additional case metadata stored without schema-specific columns.
    pub(crate) metadata: serde_json::Value,
    /// Time the blob was first inserted.
    pub(crate) created_at: DateTime<Utc>,
}
