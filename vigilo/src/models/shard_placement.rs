//! Shard placement persistence models.
//!
//! Shard placements map one `run_id + run_shard` pair to a configured database
//! placement alias. They are control-plane routing metadata, not evaluator
//! execution contracts.

use chrono::{
    DateTime,
    Utc,
};
use serde::{
    Deserialize,
    Serialize,
};
use uuid::Uuid;

/// Insert payload for a shard placement row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShardPlacementDraft {
    /// Run whose logical shard should be routed.
    pub(crate) run_id: Uuid,
    /// Logical shard number inside the 128-shard range.
    pub(crate) run_shard: i16,
    /// Database placement alias for this run shard.
    pub(crate) database_alias: String,
    /// Placement lifecycle: active, moving, or draining.
    pub(crate) status: String,
}

/// Persisted shard placement row.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub(crate) struct ShardPlacement {
    /// Run whose logical shard is routed.
    pub(crate) run_id: Uuid,
    /// Logical shard number inside the 128-shard range.
    pub(crate) run_shard: i16,
    /// Database placement alias for this run shard.
    pub(crate) database_alias: String,
    /// Placement lifecycle: active, moving, or draining.
    pub(crate) status: String,
    /// Time this placement row was inserted.
    pub(crate) created_at: DateTime<Utc>,
    /// Time this placement row was last updated.
    pub(crate) updated_at: DateTime<Utc>,
}
