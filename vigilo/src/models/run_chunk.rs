//! Run chunk persistence models.
//!
//! Run chunks are scheduling units for converting dataset cases into
//! executions. They keep worker/coordinator passes bounded by grouping an
//! ordinal range of dataset cases for one profile group.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for one run scheduling chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunChunkDraft {
    /// Deterministic chunk id generated during run planning.
    pub(crate) chunk_id: Uuid,
    /// Evaluation profile group applied to cases in this chunk.
    pub(crate) profile_group_id: String,
    /// Inclusive start ordinal in the dataset version.
    pub(crate) ordinal_start: i32,
    /// Exclusive end ordinal in the dataset version.
    pub(crate) ordinal_end: i32,
}

/// Persisted run scheduling chunk.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct RunChunk {
    /// Chunk id, unique within the run partition key.
    pub(crate) id: Uuid,
    /// Run partition key and parent run id.
    pub(crate) run_id: Uuid,
    /// Dataset version whose cases are covered by this chunk.
    pub(crate) dataset_version_id: Uuid,
    /// Evaluation profile group applied to cases in this chunk.
    pub(crate) profile_group_id: String,
    /// Inclusive start ordinal in the dataset version.
    pub(crate) ordinal_start: i32,
    /// Exclusive end ordinal in the dataset version.
    pub(crate) ordinal_end: i32,
    /// Current chunk scheduling status.
    pub(crate) status: String,
    /// Worker/coordinator lease expiration, when leased.
    pub(crate) leased_until: Option<DateTime<Utc>>,
    /// Time this chunk row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time this chunk row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}
