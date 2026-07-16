//! Dataset-version case mapping models.
//!
//! A dataset version is an ordered list of case ids pointing at content
//! addressed case blobs. These models represent the join rows that preserve
//! per-version ordering while reusing shared case content.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for one case in a dataset version.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct DatasetVersionCaseDraft {
    /// Case id as it appears in the dataset contract.
    pub(crate) case_id: Uuid,
    /// Zero-based ordinal used to preserve dataset ordering.
    pub(crate) case_ordinal: i32,
    /// Hash that references the corresponding `case_blobs` row.
    pub(crate) case_hash: String,
}

/// Persisted mapping from a dataset version to one case blob.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct DatasetVersionCase {
    /// Stable dataset-version id.
    pub(crate) dataset_version_id: Uuid,
    /// Case id as it appears in the dataset contract.
    pub(crate) case_id: Uuid,
    /// Zero-based ordinal used to preserve dataset ordering.
    pub(crate) case_ordinal: i32,
    /// Hash that references the corresponding `case_blobs` row.
    pub(crate) case_hash: String,
    /// Time the mapping row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time the mapping row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}
